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

const DEVICE_NAME: &str = "true-switcher-experiment-102";
const LOG_PREFIX: &str = "experiment-102";
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
const SHIFT_KEYS: [KeyCode; 2] = [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT];
const MODIFIER_KEYS: [KeyCode; 8] = [
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_RIGHTMETA,
];
const DEFAULT_ENTRY_DELAY: Duration = Duration::from_secs(2);
const HOLD_PROMPT: &str = "[NOW!] Hold A";
const RELEASE_PROMPT: &str = "[NOW!] Release A";
const IDLE_PROMPT: &str = "[NOW!] Do not press any keys; waiting for a clean capture";
const GUIDED_ATTEMPTS: u64 = 2;

struct Options {
    is_shifted: bool,
    before_grab: Duration,
    after_grab: Duration,
    rollback_gap: Duration,
}

#[derive(Default)]
struct EntryRecording {
    is_active: bool,
    presses: BTreeSet<(usize, u16)>,
}

impl EntryRecording {
    fn begin(&mut self) {
        self.presses.clear();
        self.is_active = true;
    }

    fn record(&mut self, index: usize, code: u16, value: i32) {
        if self.is_active && matches!(value, 1 | 2) {
            self.presses.insert((index, code));
        }
    }
}

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
            Some(
                DEVICE_NAME
                    | "true-switcher-experiment-101"
                    | "true-switcher-keyboard"
                    | "true-switcher-modifier-experiment"
            )
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
    options: Options,
    synthetic_held: BTreeSet<u16>, // Includes attempted presses: `emit` can partially fail.
    recording: EntryRecording,
    repair_pending: BTreeSet<(usize, u16)>, // Includes attempted source presses until released outside capture.
    event_count: u64,
    attempt: u64,
}

#[derive(Clone, Copy)]
struct Trigger {
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
            .map(|pause_source| Trigger { pause_source });
        Ok((trigger, is_exit))
    }

    fn record_event(&mut self, index: usize, event: InputEvent) -> io::Result<()> {
        self.event_count = self.event_count.wrapping_add(1);
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
        self.recording.record(index, event.code(), event.value());
        self.log.write(format_args!(
            "EVDEV {} origin=unknown kernel_time={:?} {:?} value={} grabbed={} held={:?} recording={} attempt={}",
            keyboard.path.display(),
            event.timestamp(),
            KeyCode::new(event.code()),
            event.value(),
            keyboard.device.is_grabbed(),
            keyboard.held,
            self.recording.is_active,
            self.attempt,
        ))?;
        if keyboard.injected_releases.contains(&event.code()) && matches!(event.value(), 1 | 2) {
            self.log.write(format_args!(
                "REASSERTED {} {:?} value={}; observed after source release",
                keyboard.path.display(),
                KeyCode::new(event.code()),
                event.value()
            ))?;
        }
        Ok(())
    }

    fn write_source(&mut self, index: usize, key: KeyCode, value: i32) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        self.log.write(format_args!(
            "SOURCE_WRITE attempt {} {key:?} value={value} + SYN_REPORT; grabbed={}",
            keyboard.path.display(),
            keyboard.device.is_grabbed()
        ))?;
        let result = keyboard.device.send_events(&[
            InputEvent::new(EventType::KEY.0, key.0, value),
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
        ]);
        self.log.write(format_args!(
            "SOURCE_WRITE result {} {key:?} value={value}: {result:?}; write is not a KDE acknowledgement",
            keyboard.path.display()
        ))?;
        result?;
        update_held(&mut keyboard.held, key.0, value);
        Ok(())
    }

    fn release_source(&mut self, index: usize, key: KeyCode) -> io::Result<()> {
        self.write_source(index, key, 0)?;
        let keyboard = &mut self.keyboards[index];
        keyboard.injected_releases.insert(key.0);
        Ok(())
    }

    fn release_sources(&mut self, trigger: Trigger) -> io::Result<()> {
        for index in 0..self.keyboards.len() {
            for key in MODIFIER_KEYS {
                if self.keyboards[index].held.contains(&key.0) {
                    self.release_source(index, key)?;
                }
            }
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
        if self.options.is_shifted {
            self.emit(KeyCode::KEY_LEFTSHIFT, 1)?;
        }
        for (index, key) in TEST_KEYS.into_iter().enumerate() {
            if index > 0 && !self.wait_until(Instant::now() + KEY_INTERVAL)? {
                return Ok(false);
            }
            self.emit(key, 1)?;
            self.emit(key, 0)?;
        }
        if self.options.is_shifted {
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
        self.log.write("WAIT_RELEASES begin; held Shifts reset through source devices, other new presses and repeats awaited")?;
        loop {
            if self.read_events()?.1 {
                self.log.write("ABORT Escape while waiting for releases")?;
                return Ok(false);
            }
            if self.release_captured_shifts()? {
                continue;
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

    fn release_captured_shifts(&mut self) -> io::Result<bool> {
        let mut is_reset = false;
        for index in 0..self.keyboards.len() {
            if !self.keyboards[index].device.is_grabbed() {
                continue;
            }
            let state = self.keyboards[index].device.get_key_state()?;
            for key in SHIFT_KEYS {
                if state.contains(key) || self.keyboards[index].held.contains(&key.0) {
                    self.log.write(format_args!(
                        "CAPTURE_SHIFT_RESET {} {key:?}; before ungrab",
                        self.keyboards[index].path.display()
                    ))?;
                    self.release_source(index, key)?;
                    is_reset = true;
                }
            }
        }
        Ok(is_reset)
    }

    fn drain_events(&mut self) -> io::Result<bool> {
        loop {
            let previous_count = self.event_count;
            if self.read_events()?.1 {
                return Ok(false);
            }
            if self.event_count == previous_count {
                return Ok(true);
            }
        }
    }

    fn snapshot_keys(&mut self) -> io::Result<()> {
        for (index, keyboard) in self.keyboards.iter_mut().enumerate() {
            let held: BTreeSet<_> = keyboard
                .device
                .get_key_state()?
                .iter()
                .map(|key| key.0)
                .collect();
            self.log.write(format_args!(
                "STATE {} held={held:?} recording={}",
                keyboard.path.display(),
                self.recording.is_active
            ))?;
            for &code in &held {
                self.recording.record(index, code, 1);
            }
            keyboard.held = held;
        }
        Ok(())
    }

    fn wait_for_ordinary_keys(&mut self) -> io::Result<bool> {
        self.log
            .write("WAIT_ORDINARY begin; modifiers and original Pause do not block entry")?;
        loop {
            if !self.drain_events()? {
                return Ok(false);
            }
            self.snapshot_keys()?;
            let is_ordinary_held = self.keyboards.iter().any(|keyboard| {
                keyboard.held.iter().any(|&code| {
                    code != KeyCode::KEY_PAUSE.0 && !MODIFIER_KEYS.contains(&KeyCode::new(code))
                })
            });
            if !is_ordinary_held {
                return self.log.write("WAIT_ORDINARY complete").map(|()| true);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn prompt_entry(&mut self, is_grabbed: bool) -> io::Result<()> {
        let (phase, delay) = if is_grabbed {
            ("GRABBED", self.options.after_grab)
        } else {
            ("RECORDING, direct input", self.options.before_grab)
        };
        let prompt = if self.attempt > GUIDED_ATTEMPTS {
            IDLE_PROMPT
        } else if is_grabbed {
            RELEASE_PROMPT
        } else {
            HOLD_PROMPT
        };
        eprintln!("Attempt {}: {phase} for {delay:?}.", self.attempt);
        println!("{prompt}");
        io::stdout().flush()?;
        self.log
            .write(format_args!("PROMPT attempt={} {prompt}", self.attempt))
    }

    fn enter_capture(&mut self, trigger: Trigger) -> io::Result<bool> {
        loop {
            if !self.wait_for_ordinary_keys()? {
                return Ok(false);
            }
            self.attempt += 1;
            self.recording.begin();
            self.log
                .write(format_args!("RECORD_BEGIN attempt={}", self.attempt))?;
            self.release_sources(trigger)?;
            self.prompt_entry(false)?;
            if !self.wait_until(Instant::now() + self.options.before_grab)? {
                return Ok(false);
            }
            self.grab()?;
            self.prompt_entry(true)?;
            if !self.wait_until(Instant::now() + self.options.after_grab)?
                || !self.drain_events()?
            {
                return Ok(false);
            }
            self.snapshot_keys()?;
            if !self.drain_events()? {
                return Ok(false);
            }
            self.recording.is_active = false;
            self.log.write(format_args!(
                "RECORD_END attempt={} presses={:?}",
                self.attempt, self.recording.presses
            ))?;
            if self.recording.presses.is_empty() {
                self.log.write("ENTRY_ACCEPTED; beginning sequence 101")?;
                return Ok(true);
            }
            self.rollback_entry()?;
            eprintln!(
                "Entry cancelled and capture released. Retrying; follow the next [NOW!] instruction."
            );
        }
    }

    fn prepare_repair(&mut self) -> io::Result<()> {
        self.recording.is_active = false;
        let presses = std::mem::take(&mut self.recording.presses);
        let mut first_error = None;
        for (index, code) in presses {
            if !self.keyboards[index].device.is_grabbed() {
                continue;
            }
            if !is_repair_key(KeyCode::new(code)) {
                first_error.get_or_insert_with(|| {
                    io::Error::other(format!(
                        "unsupported entry key {:?}; hidden down deliberately not injected",
                        KeyCode::new(code)
                    ))
                });
                continue;
            }
            self.repair_pending.insert((index, code));
        }
        for (index, code) in self.repair_pending.clone() {
            if !self.keyboards[index].device.is_grabbed() {
                continue;
            }
            if let Err(error) = self.write_source(index, KeyCode::new(code), 1) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish_repair(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for (index, code) in self.repair_pending.clone() {
            if self.keyboards[index].device.is_grabbed() {
                first_error.get_or_insert_with(|| {
                    io::Error::other("cannot deliver repair up while still grabbed")
                });
                continue;
            }
            match self.release_source(index, KeyCode::new(code)) {
                Ok(()) => {
                    self.repair_pending.remove(&(index, code));
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn rollback_entry(&mut self) -> io::Result<()> {
        self.log
            .write("ROLLBACK begin; hidden source down, ungrab, visible source up")?;
        let mut first_error = self.prepare_repair().err();
        if first_error.is_none() && self.options.rollback_gap != Duration::ZERO {
            eprintln!(
                "ROLLBACK hidden downs sent; experimental gap {:?} before ungrab.",
                self.options.rollback_gap
            );
            match self.wait_until(Instant::now() + self.options.rollback_gap) {
                Ok(true) => {}
                Ok(false) => {
                    first_error.get_or_insert_with(|| {
                        io::Error::new(io::ErrorKind::Interrupted, "Escape during rollback gap")
                    });
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Err(error) = self.ungrab() {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.finish_repair() {
            first_error.get_or_insert(error);
        }
        self.log
            .write(format_args!("ROLLBACK end error={first_error:?}"))?;
        first_error.map_or(Ok(()), Err)
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
                if !self.enter_capture(trigger)? {
                    return Ok(());
                }
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
        let is_entry_incomplete = self.recording.is_active
            || !self.recording.presses.is_empty()
            || !self.repair_pending.is_empty();
        if is_entry_incomplete {
            let _ = self.read_events();
            if self.recording.is_active {
                let _ = self.snapshot_keys();
            }
            if let Err(error) = self.prepare_repair() {
                eprintln!("Cleanup entry repair failed: {error}");
                let _ = self
                    .log
                    .write(format_args!("CLEANUP entry repair: {error}"));
            }
        } else if let Err(error) = self.release_captured_shifts() {
            eprintln!("Cleanup source Shift release failed: {error}");
            let _ = self
                .log
                .write(format_args!("CLEANUP source Shift release: {error}"));
        }
        if let Err(error) = self.ungrab() {
            eprintln!("Cleanup ungrab failed: {error}");
        }
        if let Err(error) = self.finish_repair() {
            eprintln!("Cleanup source release failed: {error}");
            let _ = self
                .log
                .write(format_args!("CLEANUP source release: {error}"));
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

fn is_repair_key(key: KeyCode) -> bool {
    let is_standard_key = (KeyCode::KEY_ESC.0..=KeyCode::KEY_KPDOT.0).contains(&key.0)
        && ![
            KeyCode::KEY_CAPSLOCK,
            KeyCode::KEY_NUMLOCK,
            KeyCode::KEY_SCROLLLOCK,
        ]
        .contains(&key);
    is_standard_key
        || MODIFIER_KEYS.contains(&key)
        || [
            KeyCode::KEY_102ND,
            KeyCode::KEY_KPENTER,
            KeyCode::KEY_KPSLASH,
            KeyCode::KEY_HOME,
            KeyCode::KEY_UP,
            KeyCode::KEY_PAGEUP,
            KeyCode::KEY_LEFT,
            KeyCode::KEY_RIGHT,
            KeyCode::KEY_END,
            KeyCode::KEY_DOWN,
            KeyCode::KEY_PAGEDOWN,
            KeyCode::KEY_INSERT,
            KeyCode::KEY_DELETE,
            KeyCode::KEY_PAUSE,
            KeyCode::KEY_F11,
            KeyCode::KEY_F12,
        ]
        .contains(&key)
}

const MAX_DELAY_MS: u64 = 60_000;
const HELP: &str =
    "experiment-102 [--shifted] [--before-grab-ms N] [--after-grab-ms N] [--rollback-gap-ms N]
Defaults: 2000 ms before grab, 2000 ms captured recording, 0 ms rollback gap.
Each delay must be in 0..=60000 ms. Escape exits. Trigger: LEFT Shift + Pause.
First two attempts: hold A during RECORDING, release after GRABBED. From attempt 3, press nothing.
Log: experiment-102-<UTC date and time>-<counter>.log.
Rollback gap exposes the remaining race between hidden down and ungrab; it is not a fix.";

fn parse_options(args: &[String]) -> io::Result<Options> {
    let mut options = Options {
        is_shifted: false,
        before_grab: DEFAULT_ENTRY_DELAY,
        after_grab: DEFAULT_ENTRY_DELAY,
        rollback_gap: Duration::ZERO,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--shifted" {
            options.is_shifted = true;
            continue;
        }
        let delay = match arg.as_str() {
            "--before-grab-ms" => &mut options.before_grab,
            "--after-grab-ms" => &mut options.after_grab,
            "--rollback-gap-ms" => &mut options.rollback_gap,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {arg}; use --help"),
                ));
            }
        };
        let milliseconds = args
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|&value| value <= MAX_DELAY_MS)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{arg} requires milliseconds in 0..={MAX_DELAY_MS}"),
                )
            })?;
        *delay = Duration::from_millis(milliseconds);
    }
    Ok(options)
}

fn run() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    let options = parse_options(&args)?;
    let mut log = Log::create()?;
    log.write(format_args!(
        "START shifted={} before_grab={:?} after_grab={:?} rollback_gap={:?}; timestamps are monotonic observation times",
        options.is_shifted, options.before_grab, options.after_grab, options.rollback_gap
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
        options,
        synthetic_held: BTreeSet::new(),
        recording: EntryRecording::default(),
        repair_pending: BTreeSet::new(),
        event_count: 0,
        attempt: 0,
    };
    eprintln!("Use English layout, Caps Lock off; stop the main switcher first.");
    eprintln!(
        "Repeat the experiment typing during capture: those physical keys must not appear during or after capture."
    );
    let result = experiment.run();
    let _ = experiment.log.write(format_args!("FINISH {result:?}"));
    drop(experiment);
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("experiment-102: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_keeps_completed_presses_and_device_identity() {
        let mut recording = EntryRecording::default();
        recording.begin();
        recording.record(0, KeyCode::KEY_A.0, 1);
        recording.record(0, KeyCode::KEY_A.0, 0);
        recording.record(1, KeyCode::KEY_A.0, 1);
        assert_eq!(
            recording.presses,
            BTreeSet::from([(0, KeyCode::KEY_A.0), (1, KeyCode::KEY_A.0)])
        );
    }

    #[test]
    fn repeats_cancel_entry_but_own_releases_do_not() {
        let mut recording = EntryRecording::default();
        recording.begin();
        recording.record(0, KeyCode::KEY_LEFTSHIFT.0, 0);
        recording.record(0, KeyCode::KEY_PAUSE.0, 0);
        assert!(recording.presses.is_empty());
        recording.record(0, KeyCode::KEY_PAUSE.0, 2);
        assert_eq!(
            recording.presses,
            BTreeSet::from([(0, KeyCode::KEY_PAUSE.0)])
        );
    }

    #[test]
    fn hidden_repair_presses_are_outside_recording_and_retry_starts_empty() {
        let mut recording = EntryRecording::default();
        recording.begin();
        recording.record(0, KeyCode::KEY_A.0, 1);
        recording.is_active = false;
        recording.record(0, KeyCode::KEY_B.0, 1);
        assert_eq!(recording.presses.len(), 1);
        recording.begin();
        assert!(recording.presses.is_empty());
    }

    #[test]
    fn rollback_does_not_reassert_toggle_or_power_keys() {
        for key in [
            KeyCode::KEY_CAPSLOCK,
            KeyCode::KEY_NUMLOCK,
            KeyCode::KEY_SCROLLLOCK,
            KeyCode::KEY_POWER,
        ] {
            assert!(!is_repair_key(key));
        }
        for key in [
            KeyCode::KEY_A,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_PAUSE,
            KeyCode::KEY_LEFT,
        ] {
            assert!(is_repair_key(key));
        }
    }

    #[test]
    fn delays_accept_zero_and_reject_missing_negative_or_excessive_values() {
        let args = [
            "--before-grab-ms",
            "0",
            "--after-grab-ms",
            "500",
            "--shifted",
        ]
        .map(String::from);
        let options = parse_options(&args).unwrap();
        assert_eq!(options.before_grab, Duration::ZERO);
        assert_eq!(options.after_grab, Duration::from_millis(500));
        assert!(options.is_shifted);
        for args in [
            vec!["--after-grab-ms"],
            vec!["--before-grab-ms", "-1"],
            vec!["--rollback-gap-ms", "60001"],
        ] {
            assert!(
                parse_options(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
    }
}
