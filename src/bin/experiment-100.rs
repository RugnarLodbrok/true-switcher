use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, uinput::VirtualDevice};

const DEVICE_NAME: &str = "true-switcher-modifier-experiment";
const LOG_PREFIX: &str = "modifier-experiment";
const LOG_DATE_FORMAT: &str = "+%Y-%m-%d_%H-%M-%S.%NZ";
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const DISCOVERY_DELAY: Duration = Duration::from_secs(1);
const KEY_INTERVAL: Duration = Duration::from_secs(1);
const TEST_KEYS: [KeyCode; 6] = [
    KeyCode::KEY_1,
    KeyCode::KEY_2,
    KeyCode::KEY_3,
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
];

struct Log {
    file: File,
    start: Instant,
}

impl Log {
    fn create() -> io::Result<Self> {
        let output = Command::new("date")
            .args(["-u", LOG_DATE_FORMAT])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "cannot format log timestamp: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let timestamp = String::from_utf8(output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        for index in 0_u64.. {
            let path = format!("{LOG_PREFIX}-{}-{index}.log", timestamp.trim());
            match File::create_new(&path) {
                Ok(file) => {
                    eprintln!("Log: {path}");
                    return Ok(Self {
                        file,
                        start: Instant::now(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    fn write(&mut self, message: impl std::fmt::Display) -> io::Result<()> {
        writeln!(
            self.file,
            "{:>12.6} {message}",
            self.start.elapsed().as_secs_f64()
        )?;
        self.file.flush()
    }
}

struct Keyboard {
    path: PathBuf,
    device: Device,
    held: BTreeSet<u16>,
}

fn open_keyboards(log: &mut Log) -> io::Result<Vec<Keyboard>> {
    let mut keyboards = Vec::new();
    for entry in fs::read_dir("/dev/input")? {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
            continue;
        }
        let path = entry.path();
        let device = Device::open(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        if matches!(device.name(), Some(DEVICE_NAME | "true-switcher-keyboard")) {
            continue;
        }
        let is_keyboard = device.supported_keys().is_some_and(|keys| {
            [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
                .iter()
                .all(|key| keys.contains(*key))
        });
        if !is_keyboard {
            continue;
        }
        device.set_nonblocking(true)?;
        let held = device.get_key_state()?.iter().map(|key| key.0).collect();
        log.write(format_args!(
            "OPEN {} {:?} held={held:?}",
            path.display(),
            device.name()
        ))?;
        keyboards.push(Keyboard { path, device, held });
    }
    if keyboards.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no accessible keyboards",
        ));
    }
    Ok(keyboards)
}

struct Experiment {
    keyboards: Vec<Keyboard>,
    output: VirtualDevice,
    log: Log,
    is_shifted: bool,
    // Include attempted presses: emit may fail after a partial write.
    synthetic_held: BTreeSet<u16>,
}

impl Experiment {
    fn emit(&mut self, key: KeyCode, value: i32) -> io::Result<()> {
        self.log
            .write(format_args!("SYNTHETIC attempt {key:?} value={value}"))?;
        if value == 1 {
            self.synthetic_held.insert(key.0);
        }
        self.output
            .emit(&[InputEvent::new(EventType::KEY.0, key.0, value)])?;
        if value == 0 {
            self.synthetic_held.remove(&key.0);
        }
        self.log
            .write(format_args!("SYNTHETIC sent {key:?} value={value}"))
    }

    fn read_physical(&mut self) -> io::Result<(bool, bool)> {
        let mut is_triggered = false;
        let mut is_exit = false;
        for keyboard in &mut self.keyboards {
            let events = match keyboard.device.fetch_events() {
                Ok(events) => events.collect::<Vec<_>>(),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{}: {error}", keyboard.path.display()),
                    ));
                }
            };
            for event in events {
                if event.event_type() != EventType::KEY {
                    continue;
                }
                update_held(&mut keyboard.held, event.code(), event.value());
                self.log.write(format_args!(
                    "PHYSICAL {} kernel_time={:?} {:?} value={} grabbed={} held={:?}",
                    keyboard.path.display(),
                    event.timestamp(),
                    KeyCode::new(event.code()),
                    event.value(),
                    keyboard.device.is_grabbed(),
                    keyboard.held,
                ))?;
                is_triggered |= event.code() == KeyCode::KEY_PAUSE.0 && event.value() == 1;
                is_exit |= event.code() == KeyCode::KEY_ESC.0 && event.value() == 1;
            }
        }
        Ok((is_triggered && self.is_shift_held(), is_exit))
    }

    fn is_shift_held(&self) -> bool {
        self.keyboards
            .iter()
            .any(|keyboard| keyboard.held.contains(&KeyCode::KEY_LEFTSHIFT.0))
    }

    fn grab(&mut self) -> io::Result<()> {
        self.log.write("GRAB begin")?;
        for keyboard in &mut self.keyboards {
            keyboard.device.grab()?;
            self.log
                .write(format_args!("GRAB acquired {}", keyboard.path.display()))?;
        }
        self.log.write("GRAB complete")
    }

    fn ungrab(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for keyboard in &mut self.keyboards {
            if !keyboard.device.is_grabbed() {
                continue;
            }
            let result = keyboard.device.ungrab().and_then(|()| {
                self.log
                    .write(format_args!("UNGRAB {}", keyboard.path.display()))
            });
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            if self.read_physical()?.1 {
                self.log.write("ABORT Escape")?;
                return Ok(false);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(true);
            }
            thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    fn type_sequence(&mut self) -> io::Result<bool> {
        self.emit(KeyCode::KEY_LEFTSHIFT, 0)?;
        if self.is_shifted {
            self.emit(KeyCode::KEY_LEFTSHIFT, 1)?;
        }
        for (index, key) in TEST_KEYS.into_iter().enumerate() {
            if index > 0 && !self.wait_until(Instant::now() + KEY_INTERVAL)? {
                return Ok(false);
            }
            self.emit(key, 1)?;
            self.emit(key, 0)?;
        }
        // Restore immediately after C up; observing the later physical Shift up
        // is part of the experiment, so leave this virtual press until cleanup.
        self.emit(KeyCode::KEY_LEFTSHIFT, i32::from(self.is_shift_held()))?;
        self.ungrab()?;
        self.log
            .write("SEQUENCE complete; virtual Shift retained until exit")?;
        Ok(true)
    }

    fn run(&mut self) -> io::Result<()> {
        if !self.wait_until(Instant::now() + DISCOVERY_DELAY)? {
            return Ok(());
        }
        eprintln!(
            "Ready: hold LEFT Shift and press Pause. Escape exits, including during capture."
        );
        loop {
            let (is_triggered, is_exit) = self.read_physical()?;
            if is_exit {
                return Ok(());
            }
            if is_triggered {
                self.grab()?;
                if !self.type_sequence()? {
                    return Ok(());
                }
                eprintln!(
                    "Capture released. Keep Shift held and type; then release it and test lowercase. Escape exits."
                );
                return self.observe_after_sequence();
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn observe_after_sequence(&mut self) -> io::Result<()> {
        loop {
            if self.read_physical()?.1 {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Experiment {
    fn drop(&mut self) {
        let _ = self.log.write("CLEANUP begin");
        for code in std::mem::take(&mut self.synthetic_held) {
            let result = self
                .output
                .emit(&[InputEvent::new(EventType::KEY.0, code, 0)]);
            let _ = self.log.write(format_args!(
                "CLEANUP release {:?}: {result:?}",
                KeyCode::new(code)
            ));
        }
        if let Err(error) = self.ungrab() {
            eprintln!("Cleanup ungrab failed: {error}");
        }
        let _ = self.log.write("CLEANUP end; closing device descriptors");
    }
}

fn update_held(held: &mut BTreeSet<u16>, code: u16, value: i32) {
    match value {
        1 | 2 => {
            held.insert(code);
        }
        0 => {
            held.remove(&code);
        }
        _ => {}
    }
}

fn run() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "experiment-100 [--shifted]\nLog: {LOG_PREFIX}-<UTC date and time>-<counter>.log (a new file for each run)."
        );
        return Ok(());
    }
    let is_shifted = match args.as_slice() {
        [] => false,
        [arg] if arg == "--shifted" => true,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected [--shifted] or --help",
            ));
        }
    };
    let mut log = Log::create()?;
    log.write(format_args!(
        "START shifted={is_shifted}; timestamps are monotonic observation times"
    ))?;
    // Enumerate before creating our virtual keyboard, so it cannot be grabbed.
    let keyboards = open_keyboards(&mut log)?;
    let mut keys: AttributeSet<KeyCode> = TEST_KEYS.into_iter().collect();
    keys.insert(KeyCode::KEY_LEFTSHIFT);
    let output = VirtualDevice::builder()?
        .name(DEVICE_NAME)
        .with_keys(&keys)?
        .build()?;
    let mut experiment = Experiment {
        keyboards,
        output,
        log,
        is_shifted,
        synthetic_held: BTreeSet::new(),
    };
    eprintln!("Use English layout, Caps Lock off; stop the main switcher first.");
    eprintln!(
        "Repeat the experiment typing during capture: those physical keys must not appear during or after capture."
    );
    let result = experiment.run();
    let _ = experiment.log.write(format_args!("FINISH {result:?}"));
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("experiment-100: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_repeat_keeps_key_held_until_release() {
        let mut held = BTreeSet::new();
        update_held(&mut held, KeyCode::KEY_LEFTSHIFT.0, 1);
        update_held(&mut held, KeyCode::KEY_A.0, 1);
        update_held(&mut held, KeyCode::KEY_A.0, 2);
        update_held(&mut held, KeyCode::KEY_A.0, 0);
        assert_eq!(held, BTreeSet::from([KeyCode::KEY_LEFTSHIFT.0]));
        update_held(&mut held, KeyCode::KEY_LEFTSHIFT.0, 0);
        assert!(held.is_empty());
    }
}
