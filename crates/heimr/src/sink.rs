//! Where records go: every [`Envelope`] (adapter readings, state and events,
//! app lifecycle events, and copies of the actuator's audit records) is
//! handed to each sink. Sinks are telemetry: the durable command audit
//! trail is the actuator's [`AuditLog`](heimr_core::AuditLog), not a sink.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use heimr_core::{Envelope, Event, Reading, Record, StateKey};

/// A destination for records.
///
/// `record` is called from a single pump task, in order, and must not block:
/// a sink that does I/O queues internally and works in its own task.
//
// Storage sink: not yet implemented. Nothing but the audit journal is
// persisted.
pub trait Sink: Send + Sync + 'static {
    /// Short name for logs.
    fn name(&self) -> &str;
    /// Accept one record.
    fn record(&self, envelope: &Envelope);
}

/// Keeps the most recent records in memory. Clones share the buffer, so keep
/// one to inspect what the app emitted (tests, a future live view).
#[derive(Debug, Clone)]
pub struct MemorySink {
    buf: Arc<Mutex<VecDeque<Envelope>>>,
    capacity: usize,
}

impl MemorySink {
    /// A sink holding at most `capacity` records (oldest dropped first).
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(4096)))),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Envelope>> {
        self.buf.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every record held, oldest first.
    pub fn snapshot(&self) -> Vec<Envelope> {
        self.lock().iter().cloned().collect()
    }

    /// Readings held, oldest first.
    pub fn readings(&self) -> Vec<Reading> {
        self.lock()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Reading(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    /// Events held (from any source), oldest first.
    pub fn events(&self) -> Vec<Event> {
        self.lock()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Event(ev) => Some(ev.clone()),
                _ => None,
            })
            .collect()
    }

    /// The newest state record held for each zone or device, keyed by
    /// (source instance, class, id): instances never overwrite each other's
    /// state, whatever ids they use.
    pub fn latest_states(&self) -> BTreeMap<StateKey, Record> {
        let mut out = BTreeMap::new();
        for env in self.lock().iter() {
            if let Some(key) = env.state_key() {
                out.insert(key, env.record.clone());
            }
        }
        out
    }

    /// Number of records held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

impl Sink for MemorySink {
    fn name(&self) -> &str {
        "memory"
    }

    fn record(&self, envelope: &Envelope) {
        if self.capacity == 0 {
            return;
        }
        let mut buf = self.lock();
        while buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(envelope.clone());
    }
}

/// Logs records through `tracing`: readings and state at `debug`, events at
/// `info`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingSink;

impl Sink for LoggingSink {
    fn name(&self) -> &str {
        "logging"
    }

    fn record(&self, envelope: &Envelope) {
        let source = envelope.source.as_str();
        match &envelope.record {
            Record::Reading(r) => tracing::debug!(
                source,
                device = %r.device,
                metric = %r.metric,
                value = r.value,
                unit = %r.unit,
                time = %r.time,
                "reading"
            ),
            Record::Event(e) => tracing::info!(
                source,
                kind = %e.kind,
                actor = %e.actor,
                payload = %e.payload,
                "event"
            ),
            other => tracing::debug!(source, record = ?other, "state"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heimr_core::{ClimateMode, ClimateState, Emitter, ZoneId};

    fn t0() -> jiff::Timestamp {
        jiff::Timestamp::UNIX_EPOCH
    }

    /// Envelopes as the core makes them (only the core can).
    async fn envelopes(records: Vec<(&str, Record)>) -> Vec<Envelope> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        for (source, record) in records {
            Emitter::for_adapter(source, tx.clone())
                .emit(record)
                .await
                .unwrap();
        }
        drop(tx);
        let mut out = vec![];
        while let Some(e) = rx.recv().await {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn memory_sink_drops_oldest() {
        let sink = MemorySink::new(2);
        let records = (0..3)
            .map(|i| {
                let ev = Event::new(t0(), "k", ["s0", "s1", "s2"][i], serde_json::Value::Null);
                (["s0", "s1", "s2"][i], Record::Event(ev))
            })
            .collect();
        for env in envelopes(records).await {
            sink.record(&env);
        }
        let got: Vec<_> = sink.snapshot().into_iter().map(|e| e.source).collect();
        assert_eq!(got, ["s1", "s2"]);
        assert_eq!(sink.events().len(), 2);
    }

    #[tokio::test]
    async fn state_is_keyed_by_source() {
        let sink = MemorySink::new(16);
        let zone = || ZoneId::new("zone-a").unwrap();
        let a = ClimateState::new(zone(), t0(), ClimateMode::Schedule);
        let b = ClimateState::new(zone(), t0(), ClimateMode::Off);
        for env in envelopes(vec![("heat-a", a.into()), ("heat-b", b.into())]).await {
            sink.record(&env);
        }
        let states = sink.latest_states();
        assert_eq!(states.len(), 2, "same zone id, two instances, two states");
        let sources: Vec<_> = states.keys().map(|k| k.source.as_str()).collect();
        assert_eq!(sources, ["heat-a", "heat-b"]);
    }
}
