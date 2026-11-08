use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use evdev::{AttributeSetRef, KeyCode, PropType, raw_stream::RawDevice};
use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

use crate::input_events::{InputEventMessage, InputEventSender};
use crate::output::is_own_keyboard;

const INPUT_DIRECTORY: &str = "/dev/input";
const DEVICE_PREFIX: &[u8] = b"event";
type Devices = BTreeMap<PathBuf, (u64, u64)>;

pub(super) struct Monitor {
    watcher: Inotify,
    pub(super) keyboards: Devices,
}

impl Monitor {
    pub(super) fn new() -> io::Result<Self> {
        let watcher = Inotify::init(InitFlags::IN_CLOEXEC)?;
        watcher.add_watch(
            INPUT_DIRECTORY,
            AddWatchFlags::IN_CREATE
                | AddWatchFlags::IN_DELETE
                | AddWatchFlags::IN_MOVED_FROM
                | AddWatchFlags::IN_MOVED_TO
                | AddWatchFlags::IN_ATTRIB
                | AddWatchFlags::IN_DELETE_SELF
                | AddWatchFlags::IN_MOVE_SELF,
        )?;
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
            let events = match self.watcher.read_events() {
                Ok(events) => events,
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            };
            let mut is_relevant = false;
            for event in events {
                if event.mask.intersects(
                    AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF
                        | AddWatchFlags::IN_IGNORED,
                ) {
                    return Err(io::Error::other("input directory watch lost"));
                }
                let Some(name) = event.name else {
                    is_relevant |= event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW);
                    continue;
                };
                if !name.as_encoded_bytes().starts_with(DEVICE_PREFIX) {
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
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(DEVICE_PREFIX)
        {
            continue;
        }
        let path = entry.path();
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

fn probe(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let device = RawDevice::try_from(file)?;
    if is_keyboard(&device) {
        Ok(Some((metadata.rdev(), metadata.ino())))
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
