//! Opt-in metadata logging, isolated from forwarding and global tracing.
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use serde_json::{json, Value};

const CAPACITY: usize = 128;

#[derive(Default)]
pub(super) struct Counts {
    pub full: AtomicU64,
    pub closed: AtomicU64,
    pub oversize: AtomicU64,
    pub non_utf8: AtomicU64,
    pub diagnostic_drops: AtomicU64,
    stopped: AtomicBool,
}

/// Cloning is cheap; a disabled sink has no channel, worker, or counters.
#[derive(Clone, Default)]
pub(super) struct Diagnostics {
    sender: Option<mpsc::SyncSender<Value>>,
    counts: Option<Arc<Counts>>,
}

impl Diagnostics {
    pub fn open(path: Option<&Path>) -> (Self, Option<tokio::sync::oneshot::Receiver<()>>) {
        let Some(path) = path.filter(|p| !p.as_os_str().is_empty()) else {
            return (Self::default(), None);
        };
        let path = path.to_path_buf();
        let (sender, receiver) = mpsc::sync_channel::<Value>(CAPACITY);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let counts = Arc::new(Counts::default());
        let worker_counts = counts.clone();
        // All disk work lives on this thread, including open. Even a stuck
        // filesystem cannot hold up byte forwarding or process launch.
        let worker = std::thread::Builder::new()
            .name("claude-wrap-log".into())
            .spawn(move || {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                    .open(path);
                if let Ok(mut file) = file {
                    // Refuse devices/FIFOs, and never follow a symlink log target.
                    if file.metadata().is_ok_and(|m| m.is_file()) {
                        for record in receiver {
                            // Serialize the entire record before the append:
                            // multiple wrappers may share this file. Token-sized
                            // writes from to_writer could interleave their JSON.
                            let Ok(mut line) = serde_json::to_vec(&record) else {
                                break;
                            };
                            line.push(b'\n');
                            if file.write_all(&line).is_err() {
                                break;
                            }
                        }
                        let _ = file.flush();
                    }
                }
                worker_counts.stopped.store(true, Ordering::Relaxed);
                let _ = done_tx.send(());
            });
        if worker.is_err() {
            return (Self::default(), None);
        }
        (
            Self {
                sender: Some(sender),
                counts: Some(counts),
            },
            Some(done_rx),
        )
    }

    pub fn enabled(&self) -> bool {
        self.sender.is_some()
            && self
                .counts
                .as_ref()
                .is_some_and(|c| !c.stopped.load(Ordering::Relaxed))
    }

    pub fn counts(&self) -> Option<&Counts> {
        self.counts.as_deref()
    }

    /// Only call with allowlisted metadata; never stream lines or daemon errors.
    pub fn record(&self, record: impl FnOnce() -> Value) {
        if !self.enabled() {
            return;
        }
        if let Some(sender) = &self.sender {
            let mut record = record();
            if let Some(fields) = record.as_object_mut() {
                fields.insert("wrapper_pid".into(), json!(std::process::id()));
                fields.insert(
                    "timestamp_ms".into(),
                    json!(std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()),
                );
            }
            if sender.try_send(record).is_err() {
                if let Some(counts) = &self.counts {
                    increment(&counts.diagnostic_drops);
                }
            }
        }
    }

    pub fn summary(&self, tracker: crate::sessions::stream::StreamDiagnostics) {
        self.record(|| {
            let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            let c = self.counts.as_deref();
            json!({"event":"diagnostic_summary", "tracker":tracker,
                "tee_full":c.map(|c| load(&c.full)), "tee_closed":c.map(|c| load(&c.closed)),
                "tee_oversize":c.map(|c| load(&c.oversize)), "tee_non_utf8":c.map(|c| load(&c.non_utf8)),
                "diagnostic_drops":c.map(|c| load(&c.diagnostic_drops))})
        });
    }
}

pub(super) fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(1))
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_queue_saturation_is_bounded_and_counted() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let sink = Diagnostics {
            sender: Some(sender),
            counts: Some(Arc::new(Counts::default())),
        };
        for _ in 0..100 {
            sink.record(|| json!({"event":"test"}));
        }
        assert_eq!(
            sink.counts()
                .unwrap()
                .diagnostic_drops
                .load(Ordering::Relaxed),
            99
        );
    }

    #[tokio::test]
    async fn disabled_sink_does_not_evaluate_records() {
        let (sink, done) = Diagnostics::open(None);
        assert!(!sink.enabled());
        assert!(done.is_none());
        sink.record(|| panic!("disabled sink formatted a record"));
    }

    #[tokio::test]
    async fn sink_writes_metadata_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.jsonl");
        let (sink, done) = Diagnostics::open(Some(&path));
        sink.record(|| json!({"event":"process_exit", "code":0}));
        drop(sink);
        tokio::time::timeout(std::time::Duration::from_secs(2), done.unwrap())
            .await
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("process_exit"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn concurrent_sinks_append_complete_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.jsonl");
        let (a, a_done) = Diagnostics::open(Some(&path));
        let (b, b_done) = Diagnostics::open(Some(&path));
        for n in 0..50 {
            a.record(|| json!({"event":"a", "n":n}));
            b.record(|| json!({"event":"b", "n":n}));
        }
        drop(a);
        drop(b);
        a_done.unwrap().await.unwrap();
        b_done.unwrap().await.unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 100);
        for line in text.lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            assert!(value["wrapper_pid"].is_number());
            assert!(value["timestamp_ms"].is_number());
        }
    }

    #[tokio::test]
    async fn failed_sink_is_nonblocking() {
        let dir = tempfile::tempdir().unwrap();
        let (sink, done) = Diagnostics::open(Some(dir.path()));
        tokio::time::timeout(std::time::Duration::from_secs(2), done.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(!sink.enabled());
        sink.record(|| panic!("failed sink formatted a record"));
        assert_eq!(
            sink.counts()
                .unwrap()
                .diagnostic_drops
                .load(Ordering::Relaxed),
            0
        );
    }
}
