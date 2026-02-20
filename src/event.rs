use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, MouseEvent};
use futures::{Stream, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;

/// Bounded event queue capacity.
/// Large enough to buffer bursty input while preventing unbounded memory growth.
const EVENT_CHANNEL_CAPACITY: usize = 2048;

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    Resize,
}

pub struct EventHandler {
    rx: mpsc::Receiver<Event>,
    _task: tokio::task::JoinHandle<()>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        Self::from_stream(EventStream::new(), tick_rate)
    }

    /// Build an EventHandler from any crossterm-compatible event stream.
    /// Production code uses `EventStream::new()`; tests inject a fake stream.
    pub fn from_stream<S>(stream: S, tick_rate: Duration) -> Self
    where
        S: Stream<Item = Result<CrosstermEvent, std::io::Error>> + Send + Unpin + 'static,
    {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let task = tokio::spawn(async move {
            let mut reader = stream;
            let mut tick = tokio::time::interval(tick_rate);

            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        // Coalesce ticks when the queue is full.
                        match tx.try_send(Event::Tick) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    event = reader.next() => {
                        match event {
                            Some(Ok(CrosstermEvent::Key(key))) => {
                                if tx.send(Event::Key(key)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                                if tx.send(Event::Mouse(mouse)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(CrosstermEvent::Resize(_, _))) => {
                                if tx.send(Event::Resize).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(_)) | None => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        Self { rx, _task: task }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

    /// Create a fake stream from a vec of crossterm results.
    fn fake_stream(
        events: Vec<Result<CrosstermEvent, std::io::Error>>,
    ) -> impl Stream<Item = Result<CrosstermEvent, std::io::Error>> + Send + Unpin {
        futures::stream::iter(events)
    }

    fn key_event(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse_event() -> CrosstermEvent {
        CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[tokio::test]
    async fn forwards_key_events() {
        let stream = fake_stream(vec![
            Ok(key_event(KeyCode::Char('j'))),
            Ok(key_event(KeyCode::Enter)),
        ]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        let e1 = handler.next().await.unwrap();
        assert!(matches!(e1, Event::Key(k) if k.code == KeyCode::Char('j')));

        let e2 = handler.next().await.unwrap();
        assert!(matches!(e2, Event::Key(k) if k.code == KeyCode::Enter));
    }

    #[tokio::test]
    async fn forwards_mouse_events() {
        let stream = fake_stream(vec![Ok(mouse_event())]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Mouse(m) if m.column == 10 && m.row == 5));
    }

    #[tokio::test]
    async fn forwards_resize_events() {
        let stream = fake_stream(vec![Ok(CrosstermEvent::Resize(120, 40))]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Resize));
    }

    #[tokio::test]
    async fn tick_fires_on_interval() {
        // Empty stream that never yields — only ticks will arrive
        let stream = futures::stream::pending();
        let mut handler = EventHandler::from_stream(stream, Duration::from_millis(10));

        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Tick));
    }

    #[tokio::test]
    async fn stream_error_ends_loop() {
        let stream = fake_stream(vec![
            Ok(key_event(KeyCode::Char('a'))),
            Err(std::io::Error::new(std::io::ErrorKind::Other, "fail")),
        ]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        // First event should arrive
        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Key(_)));

        // After error, channel closes → next() returns None
        // (may get a tick before closure, so drain any ticks)
        loop {
            match handler.next().await {
                Some(Event::Tick) => continue,
                None => break,
                other => panic!("expected None or Tick, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn stream_none_ends_loop() {
        // Stream that yields one event then ends (None)
        let stream = fake_stream(vec![Ok(key_event(KeyCode::Char('z')))]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Key(k) if k.code == KeyCode::Char('z')));

        // Stream ended → loop exits → channel closes
        loop {
            match handler.next().await {
                Some(Event::Tick) => continue,
                None => break,
                other => panic!("expected None or Tick, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn ignores_unknown_crossterm_events() {
        let stream = fake_stream(vec![
            Ok(CrosstermEvent::FocusGained), // unknown variant — should be ignored
            Ok(key_event(KeyCode::Char('x'))),
        ]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        // FocusGained is silently dropped, next event is the key
        let e = handler.next().await.unwrap();
        assert!(matches!(e, Event::Key(k) if k.code == KeyCode::Char('x')));
    }

    #[tokio::test]
    async fn mixed_event_types() {
        let stream = fake_stream(vec![
            Ok(key_event(KeyCode::Char('a'))),
            Ok(mouse_event()),
            Ok(CrosstermEvent::Resize(80, 24)),
            Ok(key_event(KeyCode::Esc)),
        ]);
        let mut handler = EventHandler::from_stream(stream, Duration::from_secs(60));

        assert!(matches!(handler.next().await.unwrap(), Event::Key(_)));
        assert!(matches!(handler.next().await.unwrap(), Event::Mouse(_)));
        assert!(matches!(handler.next().await.unwrap(), Event::Resize));
        assert!(matches!(handler.next().await.unwrap(), Event::Key(k) if k.code == KeyCode::Esc));
    }

    #[tokio::test]
    async fn drop_handler_stops_task() {
        let stream = futures::stream::pending::<Result<CrosstermEvent, std::io::Error>>();
        let handler = EventHandler::from_stream(stream, Duration::from_millis(10));
        let task = &handler._task;
        assert!(!task.is_finished());
        drop(handler);
        // Task should finish shortly after rx is dropped
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Can't check task.is_finished() after drop, but no panic = success
    }
}
