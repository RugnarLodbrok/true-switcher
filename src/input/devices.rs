use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use evdev::{AttributeSetRef, KeyCode, PropType, raw_stream::RawDevice};
use nix::sys::inotify::AddWatchFlags;

use super::device_watch::{self, DirectoryWatcher, INPUT_DIRECTORY, Identity};

use crate::input_events::{InputEventMessage, InputEventSender};
use crate::output::is_own_keyboard;

type Devices = BTreeMap<PathBuf, Identity>;

pub(super) struct Monitor {
    watcher: DirectoryWatcher,
    pub(super) keyboards: Devices,
}

impl Monitor {
    pub(super) fn new() -> io::Result<Self> {
        let watcher = DirectoryWatcher::new(Path::new(INPUT_DIRECTORY))?;
        Ok(Self {
            watcher,
            keyboards: discover(Path::new(INPUT_DIRECTORY))?,
        })
    }

    pub(super) fn start(mut self, sender: InputEventSender) -> io::Result<()> {
        thread::Builder::new()
            .name("input-devices".into())
            .spawn(move || {
                if let Err(error) = self.watch(&sender) {
                    sender.send(InputEventMessage::DeviceError(error));
                }
            })?;
        Ok(())
    }

    fn watch(&mut self, sender: &InputEventSender) -> io::Result<()> {
        loop {
            let events = self.watcher.read_events()?;
            let mut is_relevant = false;
            for event in events {
                if device_watch::is_watch_lost(event.mask) {
                    return Err(io::Error::other("input directory watch lost"));
                }
                let Some(name) = event.name else {
                    is_relevant |= event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW);
                    continue;
                };
                if !device_watch::is_device_name(&name) {
                    continue;
                }
                is_relevant = true;
                if event
                    .mask
                    .intersects(AddWatchFlags::IN_DELETE | AddWatchFlags::IN_MOVED_FROM)
                    && self
                        .keyboards
                        .remove(&Path::new(INPUT_DIRECTORY).join(name))
                        .is_some()
                    && !sender.send(InputEventMessage::DevicesChanged(Instant::now()))
                {
                    return Ok(());
                }
            }
            if is_relevant {
                let keyboards = discover(Path::new(INPUT_DIRECTORY))?;
                if keyboards != self.keyboards
                    && !sender.send(InputEventMessage::DevicesChanged(Instant::now()))
                {
                    return Ok(());
                }
                self.keyboards = keyboards;
            }
        }
    }
}

fn discover(directory: &Path) -> io::Result<Devices> {
    let mut keyboards = Devices::new();
    for path in device_watch::device_paths(directory)? {
        let path = path?;
        match probe(&path) {
            Ok(Some(identity)) => {
                keyboards.insert(path, identity);
            }
            Ok(None) => {}
            Err(error) => log::debug!("Cannot probe {}: {error}", path.display()),
        }
    }
    Ok(keyboards)
}

fn probe(path: &Path) -> io::Result<Option<Identity>> {
    let (identity, device) = device_watch::open_device(path)?;
    if is_keyboard(&device) {
        Ok(Some(identity))
    } else {
        Ok(None)
    }
}

pub(super) fn is_keyboard(device: &RawDevice) -> bool {
    !is_own_keyboard(device.physical_path(), device.name())
        && device.supported_keys().is_some_and(|keys| {
            is_keyboard_keys(keys)
                && !device.properties().contains(PropType::POINTER)
                && !device.properties().contains(PropType::DIRECT)
        })
}

fn is_keyboard_keys(keys: &AttributeSetRef<KeyCode>) -> bool {
    let is_pointer = (KeyCode::BTN_LEFT.0..KeyCode::BTN_TRIGGER.0)
        .any(|code| keys.contains(KeyCode::new(code)))
        || keys.contains(KeyCode::BTN_TOUCH);
    let is_gamepad = (KeyCode::BTN_TRIGGER.0..KeyCode::BTN_TOOL_PEN.0)
        .any(|code| keys.contains(KeyCode::new(code)));
    !is_pointer
        && !is_gamepad
        && [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
            .iter()
            .all(|key| keys.contains(*key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::AttributeSet;

    #[test]
    fn hybrid_pointers_and_gamepads_are_excluded_before_keyboards() {
        let mut keys = AttributeSet::new();
        for key in [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER] {
            keys.insert(key);
        }
        assert!(is_keyboard_keys(&keys));
        for key in [
            KeyCode::BTN_LEFT,
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_TRIGGER,
            KeyCode::BTN_SOUTH,
        ] {
            keys.insert(key);
            assert!(!is_keyboard_keys(&keys));
            keys.remove(key);
        }
    }
}
