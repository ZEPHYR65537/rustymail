//! Bounded diagnostics. Slow stderr must not block the network executor.
use std::{
    io::{self, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

const RECORD_LIMIT: usize = 8192;
const QUEUE_LIMIT: usize = 128;
static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

enum Entry {
    Record(String),
    Flush(oneshot::Sender<()>),
}

struct Logger {
    sender: mpsc::Sender<Entry>,
    dropped: Arc<AtomicU64>,
}

impl Logger {
    fn new(mut output: impl Write + Send + 'static) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel(QUEUE_LIMIT);
        let dropped = Arc::new(AtomicU64::new(0));
        let losses = dropped.clone();
        std::thread::Builder::new()
            .name("rustymail-log".into())
            .spawn(move || {
                while let Some(entry) = receiver.blocking_recv() {
                    let lost = losses.swap(0, Ordering::Relaxed);
                    if lost > 0
                        && writeln!(
                            output,
                            "{{\"event\":\"logs_dropped\",\"fields\":{{\"count\":{lost}}}}}"
                        )
                        .is_err()
                    {
                        losses.fetch_add(lost, Ordering::Relaxed);
                    }
                    match entry {
                        Entry::Record(record) => {
                            if writeln!(output, "{record}").is_err() {
                                losses.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Entry::Flush(done) => {
                            let _ = output.flush();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { sender, dropped })
    }

    fn record(&self, record: String) {
        if record.len() > RECORD_LIMIT || self.sender.try_send(Entry::Record(record)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn flush(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let (done, finished) = oneshot::channel();
            if self.sender.send(Entry::Flush(done)).await.is_ok() {
                let _ = finished.await;
            }
        })
        .await;
    }
}

pub fn start_logging() -> io::Result<()> {
    if LOGGER
        .get_or_init(|| Logger::new(io::stderr()).ok())
        .is_none()
    {
        return Err(io::Error::other("cannot start diagnostic logger"));
    }
    Ok(())
}

pub fn log_event(event: &str, fields: serde_json::Value) {
    if start_logging().is_ok()
        && let Some(Some(logger)) = LOGGER.get()
    {
        logger.record(serde_json::json!({"event":event,"fields":fields}).to_string());
    }
}

pub async fn flush_logs() {
    if let Some(Some(logger)) = LOGGER.get() {
        logger.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlockedOutput {
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Write for BlockedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn blocked_output_drops_excess_records_without_blocking_producers() {
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let logger = Logger::new(BlockedOutput {
            entered: Some(entered),
            release: wait,
        })
        .unwrap();
        logger.record("first".into());
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..QUEUE_LIMIT {
            logger.record("queued".into());
        }
        logger.record("overflow".into());
        logger.record("x".repeat(RECORD_LIMIT + 1));
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 2);
        release.send(()).unwrap();
        logger.flush().await;
    }
}
