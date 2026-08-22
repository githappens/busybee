use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tokio::signal::unix::{signal, SignalKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEvent {
    /// First SIGINT/SIGTERM — soft cancel (send SIGTERM to task).
    SoftCancel,
    /// Second terminating signal — hard kill (send SIGKILL, exit 130).
    HardKill,
}

/// Spawns a background task that converts SIGINT/SIGTERM into a stream of
/// SignalEvents on the returned receiver. First signal emits SoftCancel;
/// any subsequent terminating signal emits HardKill.
pub fn install() -> tokio::sync::mpsc::UnboundedReceiver<SignalEvent> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let seen = Arc::new(AtomicU8::new(0));
    spawn_signal_task(SignalKind::interrupt(), tx.clone(), seen.clone());
    spawn_signal_task(SignalKind::terminate(), tx, seen);
    rx
}

fn spawn_signal_task(
    kind: SignalKind,
    tx: tokio::sync::mpsc::UnboundedSender<SignalEvent>,
    seen: Arc<AtomicU8>,
) {
    tokio::spawn(async move {
        let Ok(mut s) = signal(kind) else { return };
        while s.recv().await.is_some() {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let event = if n == 0 {
                SignalEvent::SoftCancel
            } else {
                SignalEvent::HardKill
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_signal_is_soft_second_is_hard() {
        let mut rx = install();
        // Give the signal task a moment to register its handler.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Fire our own process a SIGINT twice.
        unsafe { libc::raise(libc::SIGINT) };
        assert_eq!(rx.recv().await, Some(SignalEvent::SoftCancel));
        unsafe { libc::raise(libc::SIGINT) };
        assert_eq!(rx.recv().await, Some(SignalEvent::HardKill));
    }
}
