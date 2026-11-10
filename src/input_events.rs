use std::io;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use evdev::InputEvent;

use crate::layout::Layout;
use crate::lifecycle::ShutdownReason;

pub enum InputEventMessage {
    Keyboard(usize, InputEvent),
    DeviceError(io::Error),
    DeviceDisconnected(usize, Instant),
    DevicesChanged(Instant),
    Shutdown(ShutdownReason),
    FocusChanged,
    PointerPressed,
    PointerEventsLost,
    LayoutChanged(Option<Arc<Layout>>),
    CorrectionFinished {
        result: io::Result<()>,
        layout: Option<Arc<Layout>>,
    },
}

#[derive(Clone)]
pub struct InputEventSender(mpsc::Sender<InputEventMessage>);

impl InputEventSender {
    pub fn send(&self, event: InputEventMessage) -> bool {
        self.0.send(event).is_ok()
    }

    pub fn shutdown(&self, reason: ShutdownReason) -> io::Result<()> {
        self.0
            .send(InputEventMessage::Shutdown(reason))
            .map_err(|_| closed())
    }
}

pub struct InputEvents(mpsc::Receiver<InputEventMessage>);

impl InputEvents {
    pub fn new() -> (InputEventSender, Self) {
        let (sender, receiver) = mpsc::channel();
        (InputEventSender(sender), Self(receiver))
    }

    pub fn available(&self) -> io::Result<Option<InputEventMessage>> {
        match self.0.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(closed()),
        }
    }

    pub fn wait(&self, deadline: Option<Instant>) -> io::Result<Option<InputEventMessage>> {
        match deadline {
            Some(deadline) => match self
                .0
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(event) => Ok(Some(event)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(closed()),
            },
            None => self.0.recv().map(Some).map_err(|_| closed()),
        }
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "input event channel closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn writers_wake_reader_and_keep_publication_order() {
        let (sender, events) = InputEvents::new();
        assert!(events.available().unwrap().is_none());
        let first = sender.clone();
        let worker = thread::spawn(move || {
            assert!(first.send(InputEventMessage::FocusChanged));
            assert!(sender.send(InputEventMessage::LayoutChanged(None)));
        });
        let deadline = Some(Instant::now() + TEST_TIMEOUT);
        assert!(matches!(
            events.wait(deadline).unwrap(),
            Some(InputEventMessage::FocusChanged)
        ));
        assert!(matches!(
            events.wait(deadline).unwrap(),
            Some(InputEventMessage::LayoutChanged(None))
        ));
        worker.join().unwrap();
        assert_eq!(
            events.wait(deadline).err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn deadline_expires_without_a_writer_and_available_never_waits() {
        let (_sender, events) = InputEvents::new();
        assert!(events.wait(Some(Instant::now())).unwrap().is_none());
        assert!(events.available().unwrap().is_none());
    }
}
