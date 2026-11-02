use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{
    AttributeSet, Device, EventType, InputEvent, KeyCode, SynchronizationCode,
    raw_stream::RawDevice, uinput::VirtualDevice,
};

const DEVICE_NAME: &str = "true-switcher-experiment-101";
const LOG_PREFIX: &str = "experiment-101";
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
    device: RawDevice,
    held: BTreeSet<u16>,
    injected_releases: BTreeSet<u16>,
}

fn open_keyboards(log: &mut Log) -> io::Result<Vec<Keyboard>> {
    let mut keyboards = Vec::new();
    for entry in fs::read_dir("/dev/input")? {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
            continue;
        }
        let path = entry.path();
        let device = RawDevice::open(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        if matches!(
            device.name(),
            Some(DEVICE_NAME | "true-switcher-keyboard" | "true-switcher-modifier-experiment")
        ) {
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
        let device = open_writable_keyboard(&path)?;
        let held = device.get_key_state()?.iter().map(|key| key.0).collect();
        log.write(format_args!(
            "OPEN {} {:?} held={held:?}",
            path.display(),
            device.name()
        ))?;
        keyboards.push(Keyboard {
            path,
            device,
            held,
            injected_releases: BTreeSet::new(),
        });
    }
    if keyboards.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no accessible keyboards",
        ));
    }
    Ok(keyboards)
}

fn open_writable_keyboard(path: &PathBuf) -> io::Result<RawDevice> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{} requires read/write access: {error}", path.display()),
            )
        })?;
    let configuration = Device::from_fd(file.try_clone()?.into())?;
    configuration.set_nonblocking(true)?; // Cloned descriptors share file status flags.
    RawDevice::try_from(file) // Preserve raw SYN_DROPPED instead of state recovery events.
}

struct Experiment {
    keyboards: Vec<Keyboard>,
    output: VirtualDevice,
    log: Log,
    is_shifted: bool,
    synthetic_held: BTreeSet<u16>, // Includes attempted presses: `emit` can partially fail.
}

struct Trigger {
    shift_sources: Vec<usize>,
    pause_source: usize,
}

impl Experiment {
    fn emit(&mut self, key: KeyCode, value: i32) -> io::Result<()> {
        self.log
            .write(format_args!("SYNTHETIC attempt {key:?} value={value}"))?;
        if value == 1 {
            self.synthetic_held.insert(key.0);
        }
        let result = self
            .output
            .emit(&[InputEvent::new(EventType::KEY.0, key.0, value)]);
        self.log.write(format_args!(
            "SYNTHETIC result {key:?} value={value}: {result:?}"
        ))?;
        result?;
        if value == 0 {
            self.synthetic_held.remove(&key.0);
        }
        self.log
            .write(format_args!("SYNTHETIC sent {key:?} value={value}"))
    }

    fn read_events(&mut self) -> io::Result<(Option<Trigger>, bool)> {
        let mut pause_source = None;
        let mut is_exit = false;
        for index in 0..self.keyboards.len() {
            let keyboard = &mut self.keyboards[index];
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
                self.record_event(index, event)?;
                if pause_source.is_none()
                    && event.event_type() == EventType::KEY
                    && event.code() == KeyCode::KEY_PAUSE.0
                    && event.value() == 1
                {
                    pause_source = Some(index);
                }
                is_exit |= event.event_type() == EventType::KEY
                    && event.code() == KeyCode::KEY_ESC.0
                    && event.value() == 1;
            }
        }
        let shift_sources: Vec<_> = self
            .keyboards
            .iter()
            .enumerate()
            .filter(|(_, keyboard)| keyboard.held.contains(&KeyCode::KEY_LEFTSHIFT.0))
            .map(|(index, _)| index)
            .collect();
        let trigger = pause_source
            .filter(|_| !shift_sources.is_empty())
            .map(|pause_source| Trigger {
                shift_sources,
                pause_source,
            });
        Ok((trigger, is_exit))
    }

    fn record_event(&mut self, index: usize, event: InputEvent) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_DROPPED.0
        {
            return Err(io::Error::other(format!(
                "{}: SYN_DROPPED; event history lost, aborting experiment",
                keyboard.path.display()
            )));
        }
        if event.event_type() != EventType::KEY {
            return Ok(());
        }
        update_held(&mut keyboard.held, event.code(), event.value());
        self.log.write(format_args!(
            "EVDEV {} origin=unknown kernel_time={:?} {:?} value={} grabbed={} held={:?}",
            keyboard.path.display(),
            event.timestamp(),
            KeyCode::new(event.code()),
            event.value(),
            keyboard.device.is_grabbed(),
            keyboard.held,
        ))?;
        if keyboard.injected_releases.contains(&event.code()) && matches!(event.value(), 1 | 2) {
            self.log.write(format_args!(
                "REASSERTED {} {:?} value={}; no corrective release will be injected",
                keyboard.path.display(),
                KeyCode::new(event.code()),
                event.value()
            ))?;
        }
        Ok(())
    }

    fn release_source(&mut self, index: usize, key: KeyCode) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        self.log.write(format_args!(
            "SOURCE_RELEASE attempt {} {key:?} value=0 + SYN_REPORT; before grab={}",
            keyboard.path.display(),
            keyboard.device.is_grabbed()
        ))?;
        let result = keyboard.device.send_events(&[
            InputEvent::new(EventType::KEY.0, key.0, 0),
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
        ]);
        self.log.write(format_args!(
            "SOURCE_RELEASE result {} {key:?}: {result:?}; write is not a KDE acknowledgement",
            keyboard.path.display()
        ))?;
        result?;
        keyboard.held.remove(&key.0); // The original hold must not block ungrab if its real up is filtered.
        keyboard.injected_releases.insert(key.0);
        Ok(())
    }

    fn release_sources(&mut self, trigger: Trigger) -> io::Result<()> {
        for index in trigger.shift_sources {
            self.release_source(index, KeyCode::KEY_LEFTSHIFT)?;
        }
        self.release_source(trigger.pause_source, KeyCode::KEY_PAUSE)
    }

    fn grab(&mut self) -> io::Result<()> {
        self.log.write("GRAB begin")?;
        for keyboard in &mut self.keyboards {
            let result = keyboard.device.grab();
            self.log.write(format_args!(
                "GRAB attempt {}: {result:?}",
                keyboard.path.display()
            ))?;
            result?;
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
            if self.read_events()?.1 {
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
        if self.is_shifted {
            self.emit(KeyCode::KEY_LEFTSHIFT, 0)?;
        }
        if !self.wait_for_releases()? {
            return Ok(false);
        }
        self.ungrab()?;
        self.log
            .write("SEQUENCE complete; original Shift and Pause are not restored")?;
        Ok(true)
    }

    fn wait_for_releases(&mut self) -> io::Result<bool> {
        self.log.write("WAIT_RELEASES begin; original injected releases excluded, new presses and repeats included")?;
        loop {
            if self.read_events()?.1 {
                self.log.write("ABORT Escape while waiting for releases")?;
                return Ok(false);
            }
            if self
                .keyboards
                .iter()
                .all(|keyboard| keyboard.held.is_empty())
            {
                self.log.write("WAIT_RELEASES complete")?;
                return Ok(true);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn run(&mut self) -> io::Result<()> {
        if !self.wait_until(Instant::now() + DISCOVERY_DELAY)? {
            return Ok(());
        }
        eprintln!(
            "Ready: hold LEFT Shift and press Pause. Escape exits, including during capture."
        );
        loop {
            let (trigger, is_exit) = self.read_events()?;
            if is_exit {
                return Ok(());
            }
            if let Some(trigger) = trigger {
                if self.keyboards.iter().any(|keyboard| {
                    keyboard.held.iter().any(|code| {
                        ![KeyCode::KEY_LEFTSHIFT.0, KeyCode::KEY_PAUSE.0].contains(code)
                    })
                }) {
                    self.log
                        .write("TRIGGER ignored: release other keys before Shift+Pause")?;
                    continue;
                }
                self.release_sources(trigger)?;
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
            if self.read_events()?.1 {
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
            "experiment-101 [--shifted]\nLog: {LOG_PREFIX}-<UTC date and time>-<counter>.log (a new file for each run)."
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
    let keyboards = open_keyboards(&mut log)?; // Enumerate before creating our virtual keyboard.
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
            eprintln!("experiment-101: {error}");
            ExitCode::FAILURE
        }
    }
}
