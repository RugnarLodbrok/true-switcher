use std::fmt;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, Signal};

use crate::input_events::{InputEventMessage, InputEventSender};

pub const RESTART_EXIT_CODE: u8 = 75;
const RESTART_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
    Stop,
    Restart,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lifecycle {
    #[default]
    Running,
    PreparingForRestart(Instant),
    WaitingForRestart(Instant),
    Shutdown(ShutdownReason),
}

impl Lifecycle {
    pub fn devices_changed(&mut self, at: Instant) {
        let deadline = at + RESTART_DELAY;
        *self = match *self {
            Self::Shutdown(_) => return,
            Self::WaitingForRestart(previous) => Self::WaitingForRestart(previous.max(deadline)),
            Self::PreparingForRestart(previous) => {
                Self::PreparingForRestart(previous.max(deadline))
            }
            Self::Running => Self::PreparingForRestart(deadline),
        };
    }

    pub fn shutdown(&mut self, reason: ShutdownReason) {
        if *self != Self::Shutdown(ShutdownReason::Stop) {
            *self = Self::Shutdown(reason);
        }
    }

    pub fn check(&self) -> io::Result<()> {
        if *self == Self::Running {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Interrupted, ControlInterrupt))
        }
    }
}

#[derive(Debug)]
struct ControlInterrupt;

impl fmt::Display for ControlInterrupt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("input processing interrupted by lifecycle request")
    }
}

impl std::error::Error for ControlInterrupt {}

pub fn is_control_interrupt(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<ControlInterrupt>())
}

pub fn start_signals(sender: InputEventSender) -> io::Result<()> {
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.add(Signal::SIGINT);
    signals.thread_block()?;  // Все последующие потоки наследуют маску для `sigwait`.
    thread::Builder::new()
        .name("shutdown-signals".into())
        .spawn(move || {
            loop {
                match signals.wait() {
                    Ok(_) => {
                        if !sender.send(InputEventMessage::Shutdown(ShutdownReason::Stop)) {
                            break;
                        }
                    }
                    Err(error) => {
                        sender.send(InputEventMessage::DeviceError(io::Error::other(error)));
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_extend_deadline_and_stop_overrides_restart() {
        let now = Instant::now();
        let mut lifecycle = Lifecycle::Running;
        lifecycle.devices_changed(now);
        assert_eq!(
            lifecycle,
            Lifecycle::PreparingForRestart(now + RESTART_DELAY)
        );
        lifecycle = Lifecycle::WaitingForRestart(now + RESTART_DELAY);
        lifecycle.devices_changed(now + RESTART_DELAY);
        lifecycle.devices_changed(now);
        assert_eq!(
            lifecycle,
            Lifecycle::WaitingForRestart(now + RESTART_DELAY * 2)
        );
        lifecycle.shutdown(ShutdownReason::Restart);
        lifecycle.shutdown(ShutdownReason::Stop);
        lifecycle.devices_changed(now);
        lifecycle.shutdown(ShutdownReason::Restart);
        assert_eq!(lifecycle, Lifecycle::Shutdown(ShutdownReason::Stop));
    }

    #[test]
    fn control_interrupt_is_distinct_from_io_failure() {
        let error = Lifecycle::Shutdown(ShutdownReason::Stop)
            .check()
            .unwrap_err();
        assert!(is_control_interrupt(&error));
        assert!(!is_control_interrupt(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
    }
}
