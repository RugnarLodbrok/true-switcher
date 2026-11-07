use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::SystemTime;

use evdev::{EventType, InputEvent, KeyCode, raw_stream::RawDevice};

const INPUT_DIRECTORY: &str = "/dev/input";
const DEVICE_PREFIX: &str = "event";

fn start_reader(
    path: PathBuf,
    mut device: RawDevice,
    sender: mpsc::Sender<io::Result<(PathBuf, InputEvent)>>,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("input-{}", path.display()))
        .spawn(move || {
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            if event.event_type() == EventType::KEY
                                && matches!(event.value(), 0 | 1)
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
                            format!("{}: {error}", path.display()),
                        )));
                        return;
                    }
                }
            }
        })?;
    Ok(())
}

fn run() -> io::Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut count = 0;
    for entry in fs::read_dir(INPUT_DIRECTORY)? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(DEVICE_PREFIX.as_bytes())
        {
            continue;
        }
        let path = entry.path();
        let device = File::open(&path).and_then(RawDevice::try_from);
        let device = match device {
            Ok(device) => device,
            Err(error) => {
                eprintln!("Cannot open {}: {error}", path.display());
                continue;
            }
        };
        if device
            .supported_keys()
            .is_some_and(|keys| keys.iter().next().is_some())
        {
            eprintln!("Listening to {} {:?}", path.display(), device.name());
            start_reader(path, device, sender.clone())?;
            count += 1;
        }
    }
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no accessible devices with buttons",
        ));
    }
    drop(sender);
    let mut previous: Option<SystemTime> = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for message in receiver {
        let (path, event) = message?;
        let time = event.timestamp();
        let elapsed = previous
            .and_then(|last| time.duration_since(last).ok())
            .map_or_else(
                || "-".to_owned(),
                |duration| format!("{:.3}", duration.as_secs_f64() * 1000.0),
            );
        let action = if event.value() == 1 { "down" } else { "up" };
        writeln!(
            output,
            "{elapsed:>28} ms  {} {:?} {action}",
            path.display(),
            KeyCode::new(event.code())
        )?;
        output.flush()?;
        previous = Some(time);
    }
    Ok(())
}

fn main() -> io::Result<()> {
    run()
}
