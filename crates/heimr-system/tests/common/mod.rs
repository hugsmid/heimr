//! A writable copy of the fixture host.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use heimr_testkit::{Fault, Upstream};

/// A directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> std::io::Result<Self> {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("heimr-system-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host")
}

pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

fn overwrite_all(dir: &Path, junk: &[u8]) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            overwrite_all(&e.path(), junk);
        } else {
            std::fs::write(e.path(), junk).unwrap();
        }
    }
}

/// A copy of the fixture host in a temp dir, with faults.
pub struct Host {
    pub dir: TempDir,
}

impl Host {
    pub fn new() -> Self {
        let dir = TempDir::new("system-host").unwrap();
        copy_tree(&fixture(), dir.path());
        Self { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    pub fn settings(&self) -> serde_json::Value {
        let p = |rel: &str| self.path(rel).display().to_string();
        serde_json::json!({
            "interval": "10s",
            "proc_path": p("proc"),
            "sys_path": p("sys"),
            "cgroup_path": p("cgroup"),
            "root_path": p("root"),
            "processes": ["exampled"],
            "units": ["example.service"],
            "top": 2,
        })
    }

    fn restore(&self) {
        let _ = std::fs::remove_dir_all(self.path("proc"));
        let _ = std::fs::remove_dir_all(self.path("proc.off"));
        copy_tree(&fixture().join("proc"), &self.path("proc"));
    }
}

impl Upstream for Host {
    fn settings(&self) -> serde_json::Value {
        Host::settings(self)
    }

    fn inject(&self, fault: Option<Fault>) -> bool {
        match fault {
            None => {
                self.restore();
                true
            }
            Some(Fault::Unavailable) => {
                self.restore();
                std::fs::rename(self.path("proc"), self.path("proc.off")).unwrap();
                true
            }
            Some(Fault::Malformed) => {
                self.restore();
                overwrite_all(
                    &self.path("proc"),
                    b"cpu x y z\n(((( ) 18446744073709551616 -1 NaN \xff\xfe\n",
                );
                true
            }
            _ => false,
        }
    }
}
