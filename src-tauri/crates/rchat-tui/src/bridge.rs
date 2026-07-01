use rchat_core::events::{CoreEvent, CoreEventSink};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum TuiEvent {
    Core(CoreEvent),
}

#[derive(Clone)]
pub struct TuiEventSink {
    tx: mpsc::Sender<TuiEvent>,
    dropped_events: Arc<AtomicU64>,
}

impl TuiEventSink {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<TuiEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                dropped_events: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }
}

impl CoreEventSink for TuiEventSink {
    fn emit(&self, event: CoreEvent) {
        if self.tx.try_send(TuiEvent::Core(event)).is_err() {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tui_event_sink_forwards_core_events() {
        let (sink, mut rx) = TuiEventSink::channel(4);

        sink.emit(CoreEvent::ConnectionWaiting("peer-1".to_string()));

        match rx.recv().await.expect("event is forwarded") {
            TuiEvent::Core(CoreEvent::ConnectionWaiting(peer_id)) => {
                assert_eq!(peer_id, "peer-1");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn tui_event_sink_drops_without_blocking_when_full() {
        let (sink, _rx) = TuiEventSink::channel(1);

        sink.emit(CoreEvent::ConnectionWaiting("first".to_string()));
        sink.emit(CoreEvent::ConnectionWaiting("second".to_string()));

        assert_eq!(sink.dropped_events(), 1);
    }
}
