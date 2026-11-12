use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode, raw_stream::RawDevice};

use super::{is_shift, start_reader};
use crate::hold::HoldDetector;
use crate::input_events::InputEventSender;

struct Keyboard {
    path: PathBuf,
    device: Box<dyn Source>,
    held: BTreeSet<u16>,
}

pub(super) trait Source {
    fn keys(&self) -> io::Result<BTreeSet<u16>>;
    fn send(&mut self, events: &[InputEvent]) -> io::Result<()>;
    fn grab(&mut self) -> io::Result<()>;
    fn ungrab(&mut self) -> io::Result<()>;
    fn is_grabbed(&self) -> bool;
}

impl Source for RawDevice {
    fn keys(&self) -> io::Result<BTreeSet<u16>> {
        Ok(self.get_key_state()?.iter().map(|key| key.0).collect())
    }

    fn send(&mut self, events: &[InputEvent]) -> io::Result<()> {
        self.send_events(events)
    }

    fn grab(&mut self) -> io::Result<()> {
        RawDevice::grab(self)
    }

    fn ungrab(&mut self) -> io::Result<()> {
        RawDevice::ungrab(self)
    }

    fn is_grabbed(&self) -> bool {
        RawDevice::is_grabbed(self)
    }
}

#[derive(Default)]
pub(super) struct KeyboardInput {
    keyboards: Vec<Keyboard>,
    holds: HoldDetector,
}

impl KeyboardInput {
    pub(super) fn open(&mut self, path: PathBuf, sender: &InputEventSender) -> io::Result<()> {
        let device = open_device(&path)?;
        let held: BTreeSet<_> = device.get_key_state()?.iter().map(|key| key.0).collect();
        self.holds
            .snapshot(self.keyboards.len(), &held, Instant::now());
        log::debug!("OPEN {} {:?}", path.display(), device.name());
        log::trace!("OPEN {} held={held:?}", path.display());
        let reader = RawDevice::from_fd(device.as_fd().try_clone_to_owned()?)?;
        start_reader(reader, path.clone(), self.keyboards.len(), sender.clone())?;
        self.keyboards.push(Keyboard {
            path,
            device: Box::new(device),
            held,
        });
        Ok(())
    }

    fn track_key(&mut self, index: usize, event: InputEvent) {
        let keyboard = &mut self.keyboards[index];
        let key = KeyCode::new(event.code());
        let value = event.value();
        log::trace!(
            "EVDEV {} {key:?} value={value} kernel_time={:?} grabbed={}",
            keyboard.path.display(),
            event.timestamp(),
            keyboard.device.is_grabbed()
        );
        match value {
            0 => {
                keyboard.held.remove(&key.0);
            }
            1 | 2 => {
                keyboard.held.insert(key.0);
            }
            _ => {}
        }
    }

    pub(super) fn write_source(
        &mut self,
        index: usize,
        key: KeyCode,
        value: i32,
    ) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        log::trace!(
            "SOURCE_WRITE attempt {} {key:?} value={value} grabbed={}",
            keyboard.path.display(),
            keyboard.device.is_grabbed()
        );
        let result = keyboard.device.send(&[
            InputEvent::new(EventType::KEY.0, key.0, value),
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
        ]);
        log::trace!(
            "SOURCE_WRITE result {} {key:?} value={value}: {result:?}",
            keyboard.path.display()
        );
        result?;
        self.holds.update(index, key.0, value, Instant::now());
        let keyboard = &mut self.keyboards[index];
        if value == 0 {
            keyboard.held.remove(&key.0);
        } else {
            keyboard.held.insert(key.0);
        }
        Ok(())
    }

    pub(super) fn has_shortcut_modifier(&self) -> bool {
        self.keyboards.iter().any(|keyboard| {
            keyboard.held.iter().any(|&code| {
                matches!(
                    KeyCode::new(code),
                    KeyCode::KEY_LEFTCTRL
                        | KeyCode::KEY_RIGHTCTRL
                        | KeyCode::KEY_LEFTALT
                        | KeyCode::KEY_RIGHTALT
                        | KeyCode::KEY_LEFTMETA
                        | KeyCode::KEY_RIGHTMETA
                )
            })
        })
    }

    pub(super) fn len(&self) -> usize {
        self.keyboards.len()
    }

    pub(super) fn path(&self, index: usize) -> &Path {
        &self.keyboards[index].path
    }

    pub(super) fn held_keys(&self) -> impl Iterator<Item = (usize, u16)> + '_ {
        self.keyboards
            .iter()
            .enumerate()
            .flat_map(|(index, keyboard)| keyboard.held.iter().map(move |&code| (index, code)))
    }

    pub(super) fn is_key_held(&self, index: usize, code: u16) -> bool {
        self.keyboards[index].held.contains(&code)
    }

    pub(super) fn all_released(&self) -> bool {
        self.keyboards
            .iter()
            .all(|keyboard| keyboard.held.is_empty())
    }

    pub(super) fn is_grabbed(&self, index: usize) -> bool {
        self.keyboards[index].device.is_grabbed()
    }

    #[cfg(test)]
    pub(super) fn is_any_grabbed(&self) -> bool {
        self.keyboards
            .iter()
            .any(|keyboard| keyboard.device.is_grabbed())
    }

    pub(super) fn hold_deadline(&self) -> Option<Instant> {
        self.holds.deadline()
    }

    pub(super) fn expire_holds(&mut self, now: Instant) -> bool {
        self.holds.expire(now)
    }

    pub(super) fn update(&mut self, index: usize, event: InputEvent, now: Instant) -> bool {
        self.holds.update(index, event.code(), event.value(), now);
        self.track_key(index, event);
        event.value() == 2 && !is_shift(KeyCode::new(event.code()))
    }

    pub(super) fn snapshot(&mut self, index: usize) -> io::Result<&BTreeSet<u16>> {
        let keyboard = &mut self.keyboards[index];
        keyboard.held = keyboard.device.keys()?;
        self.holds.snapshot(index, &keyboard.held, Instant::now());
        Ok(&keyboard.held)
    }

    pub(super) fn grab(&mut self, index: usize) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        keyboard.device.grab()?;
        log::debug!("GRAB {}", keyboard.path.display());
        Ok(())
    }

    pub(super) fn ungrab(&mut self) -> io::Result<()> {
        let mut error = None;
        for keyboard in &mut self.keyboards {
            if keyboard.device.is_grabbed() {
                let result = keyboard.device.ungrab();
                if result.is_ok() {
                    log::debug!("UNGRAB {}", keyboard.path.display());
                }
                if let Err(err) = result {
                    log::error!("UNGRAB {}: {err}", keyboard.path.display());
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    pub(super) fn from_sources(sources: Vec<(PathBuf, Box<dyn Source>, BTreeSet<u16>)>) -> Self {
        let mut input = Self::default();
        for (path, device, held) in sources {
            input.holds.snapshot(input.len(), &held, Instant::now());
            input.keyboards.push(Keyboard { path, device, held });
        }
        input
    }
}
fn open_device(path: &Path) -> io::Result<RawDevice> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .inspect_err(|error| {
            log::warn!("{} requires read/write access: {error}", path.display());
        })?;
    RawDevice::try_from(file)
}
