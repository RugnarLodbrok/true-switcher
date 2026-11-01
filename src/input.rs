use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use evdev::{Device, EventType, InputEvent, KeyCode};

type KeyboardEvent = io::Result<(PathBuf, InputEvent)>;

/// Opens keyboards present at startup. Each device has a blocking reader so
/// an idle keyboard cannot prevent events arriving from another keyboard.
pub fn listen() -> io::Result<Receiver<KeyboardEvent>> {
    let entries = fs::read_dir("/dev/input")
        .map_err(|error| io::Error::new(error.kind(), format!("/dev/input: {error}")))?;
    let mut keyboards = Vec::new();

    for entry in entries {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
            continue;
        }

        let path = entry.path();
        let device = match Device::open(&path) {
            Ok(device) => device,
            Err(error) => {
                eprintln!("Cannot open {}: {error}", path.display());
                continue;
            }
        };

        // EV_KEY alone also describes mice, gamepads and power buttons.
        let is_keyboard = device.supported_keys().is_some_and(|keys| {
            keys.contains(KeyCode::KEY_A)
                && keys.contains(KeyCode::KEY_SPACE)
                && keys.contains(KeyCode::KEY_ENTER)
        });
        if is_keyboard {
            eprintln!(
                "Listening to {} ({})",
                path.display(),
                device.name().unwrap_or("unnamed keyboard")
            );
            keyboards.push((path, device));
        }
    }

    if keyboards.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no accessible keyboards found in /dev/input",
        ));
    }

    // Bound the queue to avoid accumulating events if stdout is slow.
    let (sender, receiver) = mpsc::sync_channel(256);
    for (path, device) in keyboards {
        let sender = sender.clone();
        thread::Builder::new()
            .name(format!("keyboard-{}", path.display()))
            .spawn(move || read_keyboard(path, device, sender))?;
    }

    Ok(receiver)
}

fn read_keyboard(path: PathBuf, mut device: Device, sender: mpsc::SyncSender<KeyboardEvent>) {
    loop {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if event.event_type() == EventType::KEY
                        && sender.send(Ok((path.clone(), event))).is_err()
                    {
                        return;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(Err(io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", path.display()),
                )));
                return;
            }
        }
    }
}
