//! The default [`AuditLog`]: an append-only, fsynced JSON-lines journal.
//!
//! Records go to `audit-NNNNNNNN.jsonl` files in one directory (by default
//! `<state_dir>/audit/`), one JSON object per line. Each append writes the
//! whole line and calls `fdatasync` before it resolves, so a command is
//! delivered only after its record is on disk. When the current file
//! reaches [`JournalOptions::max_file_bytes`] a new file is started (and the
//! directory fsynced); the newest [`JournalOptions::retain_files`] files are
//! kept and older ones deleted. Nothing within the retained files is ever
//! rewritten.
//!
//! Every line carries the format version, `"v": 1`. A line that cannot be
//! read (torn by a crash or a failed write, unparsable, or of another
//! version) is never taken for a record: it is reported to the actuator as
//! an [`UnreadableEntry`], with whatever time, instance, target and id
//! could be salvaged, and counted as an accepted command. After a crash or
//! a failed append, the next record starts on a fresh line, so a torn
//! prefix cannot swallow it.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use heimr_core::{
    AuditError, AuditLog, AuditRecord, AuditTail, BoxFuture, CommandId, UnreadableEntry,
};
use jiff::Timestamp;

/// Rotation and retention settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JournalOptions {
    /// Start a new file once the current one reaches this size.
    pub max_file_bytes: u64,
    /// How many files to keep (the newest ones; at least 2).
    pub retain_files: u32,
}

impl JournalOptions {
    /// Options with the given limits.
    pub fn new(max_file_bytes: u64, retain_files: u32) -> Self {
        Self {
            max_file_bytes,
            retain_files: retain_files.max(2),
        }
    }
}

impl Default for JournalOptions {
    /// 16 MiB per file, 64 files.
    fn default() -> Self {
        Self::new(16 * 1024 * 1024, 64)
    }
}

/// The journal format version written on every line as `"v"`.
pub const FORMAT_VERSION: u64 = 1;

#[derive(Debug)]
struct Current {
    file: File,
    index: u64,
    size: u64,
    /// The last write failed part-way: start the next on a fresh line.
    torn: bool,
}

#[derive(Debug)]
struct Inner {
    dir: PathBuf,
    opts: JournalOptions,
    current: Mutex<Current>,
    #[cfg(test)]
    fail_next: std::sync::atomic::AtomicBool,
}

/// An append-only, fsynced JSON-lines [`AuditLog`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct JsonlJournal {
    inner: Arc<Inner>,
}

fn file_name(index: u64) -> String {
    format!("audit-{index:08}.jsonl")
}

fn parse_index(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("audit-")?.strip_suffix(".jsonl")?;
    (digits.len() == 8 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// Journal file indices in `dir`, ascending.
fn indices(dir: &Path) -> std::io::Result<Vec<u64>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        if let Some(i) = entry?.file_name().to_str().and_then(parse_index) {
            out.push(i);
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
}

impl JsonlJournal {
    /// Open (or create) the journal in `dir`.
    pub fn open(dir: impl AsRef<Path>, opts: JournalOptions) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_owned();
        std::fs::create_dir_all(&dir)?;
        let index = indices(&dir)?.last().copied().unwrap_or(1);
        let path = dir.join(file_name(index));
        let mut file = open_append(&path)?;
        let mut size = file.metadata()?.len();
        if size > 0 {
            // A crash may have torn the last line: start the next on its own.
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
                file.sync_data()?;
                size += 1;
            }
        }
        sync_dir(&dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                opts,
                current: Mutex::new(Current {
                    file,
                    index,
                    size,
                    torn: false,
                }),
                #[cfg(test)]
                fail_next: std::sync::atomic::AtomicBool::new(false),
            }),
        })
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }
}

impl Inner {
    fn append_line(&self, line: &[u8]) -> std::io::Result<()> {
        let mut cur = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if cur.size > 0 && cur.size + line.len() as u64 > self.opts.max_file_bytes {
            self.rotate(&mut cur)?;
        }
        let mut buf = Vec::with_capacity(line.len() + 1);
        if cur.torn {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line);
        let written = self.write_synced(&mut cur.file, &buf);
        match written {
            Ok(()) => {
                cur.size += buf.len() as u64;
                cur.torn = false;
                Ok(())
            }
            Err(e) => {
                // Part of the line may be on disk: the next one starts fresh.
                cur.torn = true;
                if let Ok(m) = cur.file.metadata() {
                    cur.size = m.len();
                }
                Err(e)
            }
        }
    }

    fn write_synced(&self, file: &mut File, buf: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            file.write_all(&buf[..buf.len() / 2])?;
            return Err(std::io::Error::other("injected failure"));
        }
        file.write_all(buf)?;
        file.sync_data()
    }

    fn rotate(&self, cur: &mut Current) -> std::io::Result<()> {
        let index = cur.index + 1;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .read(true)
            .open(self.dir.join(file_name(index)))?;
        sync_dir(&self.dir)?;
        *cur = Current {
            file,
            index,
            size: 0,
            torn: false,
        };
        let all = indices(&self.dir)?;
        let keep = self.opts.retain_files as usize;
        if all.len() > keep {
            for old in &all[..all.len() - keep] {
                if *old < index {
                    std::fs::remove_file(self.dir.join(file_name(*old)))?;
                }
            }
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    fn tail(&self, since: Timestamp) -> std::io::Result<AuditTail> {
        let mut last_id: Option<CommandId> = None;
        let mut chunks: Vec<Vec<AuditRecord>> = Vec::new();
        let mut unknown: Vec<UnreadableEntry> = Vec::new();
        let mut skipped = 0usize;
        for index in indices(&self.dir)?.into_iter().rev() {
            let file = File::open(self.dir.join(file_name(index)))?;
            let mut records = Vec::new();
            let mut earliest: Option<Timestamp> = None;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match parse_line(&line) {
                    Ok(r) => {
                        last_id = last_id.max(Some(r.id));
                        earliest = Some(earliest.map_or(r.time, |e| e.min(r.time)));
                        if r.time >= since {
                            records.push(r);
                        }
                    }
                    Err((entry, id)) => {
                        skipped += 1;
                        last_id = last_id.max(id);
                        if let Some(t) = entry.time {
                            earliest = Some(earliest.map_or(t, |e| e.min(t)));
                        }
                        // Unknown time: it may be recent, so it counts.
                        if entry.time.is_none_or(|t| t >= since) {
                            unknown.push(entry);
                        }
                    }
                }
            }
            chunks.push(records);
            // Older files cannot hold newer records (unless the wall clock
            // stepped back, which the actuator treats conservatively).
            if earliest.is_some_and(|t| t < since) {
                break;
            }
        }
        let records = chunks.into_iter().rev().flatten().collect();
        if skipped > 0 {
            tracing::warn!(
                skipped,
                "unreadable audit journal lines (counted as accepted commands)"
            );
        }
        Ok(AuditTail::new(last_id, records)
            .with_unknown(unknown)
            .with_skipped(skipped))
    }
}

/// One journal line as a record, or what can be salvaged from it (and its
/// id, if readable) when it is torn, unparsable or of another version.
fn parse_line(line: &str) -> Result<AuditRecord, (UnreadableEntry, Option<CommandId>)> {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        return Err((UnreadableEntry::unknown(), None));
    };
    let version = obj.remove("v");
    let salvage = |obj: &serde_json::Map<String, serde_json::Value>| {
        let text = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let entry = UnreadableEntry::new(
            text("time").and_then(|t| t.parse().ok()),
            text("adapter"),
            text("target"),
        );
        (entry, text("id").and_then(|i| i.parse().ok()))
    };
    if version.as_ref().and_then(serde_json::Value::as_u64) != Some(FORMAT_VERSION) {
        return Err(salvage(&obj));
    }
    match serde_json::from_value::<AuditRecord>(serde_json::Value::Object(obj.clone())) {
        Ok(r) => Ok(r),
        Err(_) => Err(salvage(&obj)),
    }
}

impl AuditLog for JsonlJournal {
    fn append(&self, record: AuditRecord) -> BoxFuture<Result<(), AuditError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let encode = |e: serde_json::Error| AuditError::Encode(e.to_string());
            let serde_json::Value::Object(fields) =
                serde_json::to_value(&record).map_err(encode)?
            else {
                return Err(AuditError::Encode("record is not an object".into()));
            };
            let mut obj = serde_json::Map::new();
            obj.insert("v".into(), FORMAT_VERSION.into());
            obj.extend(fields);
            let mut line = serde_json::to_vec(&obj).map_err(encode)?;
            line.push(b'\n');
            tokio::task::spawn_blocking(move || inner.append_line(&line))
                .await
                .map_err(|e| AuditError::Unavailable(e.to_string()))??;
            Ok(())
        })
    }

    fn tail(&self, since: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            Ok(tokio::task::spawn_blocking(move || inner.tail(since))
                .await
                .map_err(|e| AuditError::Unavailable(e.to_string()))??)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heimr_core::{
        Actuator, Capabilities, ClimateCommand, Clock, CommandKind, CommandRequest, Hold,
        InstanceSpec, ManualClock, Mode, SafetyPolicy, Setpoint, ZoneId,
    };
    use jiff::SignedDuration;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("heimr-journal-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn t0() -> Timestamp {
        "2030-01-01T00:00:00Z".parse().unwrap()
    }

    async fn actuator(journal: &JsonlJournal, clock: &ManualClock) -> Actuator {
        let a = Actuator::builder(
            SafetyPolicy::default()
                .with_max_per_hour(1000)
                .with_min_interval(SignedDuration::ZERO),
            Arc::new(clock.clone()),
            Arc::new(journal.clone()),
        )
        .build()
        .await
        .unwrap();
        a.register(InstanceSpec::new(
            "heat",
            "test",
            Capabilities::read_write([CommandKind::ClimateTarget]),
            Mode::Shadow,
        ))
        .unwrap();
        a
    }

    fn cmd(z: usize) -> CommandRequest {
        let c = ClimateCommand::set_target(
            ZoneId::new(format!("z{z}")).unwrap(),
            Setpoint::new(20.0).unwrap(),
            Hold::NextScheduleChange,
        );
        CommandRequest::new("heat", c, "r")
    }

    #[tokio::test]
    async fn appends_rotates_retains_and_survives_reopen() {
        let dir = TempDir::new("rotate");
        let journal = JsonlJournal::open(&dir.0, JournalOptions::new(4096, 3)).unwrap();
        let clock = ManualClock::new(t0());
        let act = actuator(&journal, &clock).await;
        let mut ids = vec![];
        for i in 0..60 {
            ids.push(
                act.submitter("test")
                    .unwrap()
                    .submit(cmd(i))
                    .await
                    .unwrap()
                    .id(),
            );
            clock.advance(SignedDuration::from_secs(1));
        }
        let files = indices(&dir.0).unwrap();
        assert_eq!(
            files.len(),
            3,
            "older files beyond the retention count are deleted"
        );
        assert!(*files.first().unwrap() > 1, "rotation happened");
        for i in &files {
            let len = std::fs::metadata(dir.0.join(file_name(*i))).unwrap().len();
            assert!(len <= 4096 + 1024);
        }

        // Simulate a torn last line, then reopen.
        drop(act);
        let last = dir.0.join(file_name(*files.last().unwrap()));
        OpenOptions::new()
            .append(true)
            .open(&last)
            .unwrap()
            .write_all(b"{\"kind\":\"comm")
            .unwrap();
        let journal = JsonlJournal::open(&dir.0, JournalOptions::new(4096, 3)).unwrap();
        let tail = journal.tail(t0()).await.unwrap();
        assert_eq!(tail.skipped, 1);
        assert_eq!(tail.last_id, ids.last().copied());
        assert!(!tail.records.is_empty());
        let times: Vec<_> = tail.records.iter().map(|r| r.time).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "append order");

        // A new boot continues above every id in the journal.
        let act = actuator(&journal, &clock).await;
        let next = act
            .submitter("test")
            .unwrap()
            .submit(cmd(99))
            .await
            .unwrap()
            .id();
        assert!(next > *ids.last().unwrap());
        let reread = journal
            .tail(clock.now() - SignedDuration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reread.records.last().unwrap().id, next);
    }

    fn lines(dir: &Path) -> Vec<String> {
        let mut out = vec![];
        for i in indices(dir).unwrap() {
            let text = std::fs::read_to_string(dir.join(file_name(i))).unwrap();
            out.extend(text.lines().map(str::to_owned));
        }
        out
    }

    #[tokio::test]
    async fn every_line_is_versioned_and_other_versions_count_as_accepted() {
        let dir = TempDir::new("version");
        let journal = JsonlJournal::open(&dir.0, JournalOptions::default()).unwrap();
        let clock = ManualClock::new(t0());
        let act = actuator(&journal, &clock).await;
        act.submitter("test").unwrap().submit(cmd(1)).await.unwrap();
        drop(act);
        for l in lines(&dir.0) {
            let v: serde_json::Value = serde_json::from_str(&l).unwrap();
            assert_eq!(v["v"], 1, "{l}");
        }

        // A future version's line and an unparsable one, both recent.
        let mut future: serde_json::Value = serde_json::from_str(&lines(&dir.0)[0]).unwrap();
        future["v"] = 2.into();
        let high = CommandId::new(9_999_999_999, 1).unwrap();
        future["id"] = high.to_string().into();
        future["target"] = "z7".into();
        let path = dir.0.join(file_name(1));
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{future}").unwrap();
        writeln!(f, "not json").unwrap();
        drop(f);
        let tail = journal.tail(t0()).await.unwrap();
        assert_eq!(
            tail.records.len(),
            1,
            "other versions are never read as records"
        );
        assert_eq!(tail.unknown.len(), 2);
        assert_eq!(tail.unknown[0].target.as_deref(), Some("z7"));
        assert!(tail.unknown[1].time.is_none());
        assert_eq!(
            tail.last_id,
            Some(high),
            "ids in unreadable lines still move the boot number up"
        );

        // Rebuilt limits count both as accepted: with a cap of 3 per hour,
        // the one real command and the two unreadable lines use it up.
        let capped = Actuator::builder(
            SafetyPolicy::default()
                .with_max_per_hour(3)
                .with_min_interval(SignedDuration::ZERO),
            Arc::new(clock.clone()),
            Arc::new(journal.clone()),
        )
        .build()
        .await
        .unwrap();
        capped
            .register(InstanceSpec::new(
                "heat",
                "test",
                Capabilities::read_write([CommandKind::ClimateTarget]),
                Mode::Shadow,
            ))
            .unwrap();
        let out = capped.submitter("test").unwrap().submit(cmd(2)).await;
        assert!(
            matches!(out, Err(heimr_core::Rejection::RateLimited { .. })),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_append_cannot_swallow_the_next_record() {
        let dir = TempDir::new("torn");
        let journal = JsonlJournal::open(&dir.0, JournalOptions::default()).unwrap();
        let clock = ManualClock::new(t0());
        let act = actuator(&journal, &clock).await;
        let planner = act.submitter("test").unwrap();
        journal
            .inner
            .fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(planner.submit(cmd(1)).await.is_err(), "withheld");
        let next = planner.submit(cmd(2)).await.unwrap().id();
        let tail = journal.tail(t0()).await.unwrap();
        let ids: Vec<_> = tail.records.iter().map(|r| r.id).collect();
        assert!(
            ids.contains(&next),
            "the record after a torn write is intact"
        );
        assert!(tail.skipped >= 1, "the torn prefix is reported, not merged");
    }

    #[tokio::test]
    async fn tail_reads_back_only_what_it_needs() {
        let dir = TempDir::new("tail");
        let journal = JsonlJournal::open(&dir.0, JournalOptions::new(4096, 100)).unwrap();
        let clock = ManualClock::new(t0());
        let act = actuator(&journal, &clock).await;
        for i in 0..40 {
            act.submitter("test").unwrap().submit(cmd(i)).await.unwrap();
            clock.advance(SignedDuration::from_mins(5));
        }
        let since = clock.now() - SignedDuration::from_hours(1);
        let tail = journal.tail(since).await.unwrap();
        assert!(tail.records.iter().all(|r| r.time >= since));
        assert_eq!(tail.records.len(), 12);
    }
}
