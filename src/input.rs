use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use evdev::{Device, EventType, InputEvent, KeyCode, SynchronizationCode, raw_stream::RawDevice};

use crate::history::History;
use crate::output::is_own_keyboard;

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MODIFIERS: [KeyCode; 8] = [
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_RIGHTMETA,
];
const LOCK_KEYS: [KeyCode; 3] = [
    KeyCode::KEY_CAPSLOCK,
    KeyCode::KEY_NUMLOCK,
    KeyCode::KEY_SCROLLLOCK,
];

#[derive(Default)]
struct EntryRecording {
    presses: BTreeSet<(usize, u16)>,
}

impl EntryRecording {
    fn record(&mut self, index: usize, code: u16, value: i32) {
        if matches!(value, 1 | 2) {
            self.presses.insert((index, code));
        }
    }
}

struct Keyboard {
    path: PathBuf,
    device: Box<dyn Source>,
    held: BTreeSet<u16>,
}

trait Source {
    fn events(&mut self) -> io::Result<Vec<InputEvent>>;
    fn keys(&self) -> io::Result<BTreeSet<u16>>;
    fn send(&mut self, events: &[InputEvent]) -> io::Result<()>;
    fn grab(&mut self) -> io::Result<()>;
    fn ungrab(&mut self) -> io::Result<()>;
    fn is_grabbed(&self) -> bool;
}

impl Source for RawDevice {
    fn events(&mut self) -> io::Result<Vec<InputEvent>> {
        Ok(self.fetch_events()?.collect())
    }

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

pub struct Input {
    keyboards: Vec<Keyboard>,
    history: History,
    pending: Option<usize>,
    is_original_pause_held: bool,
    recording: Option<EntryRecording>,
    repair: BTreeSet<(usize, u16)>,
    repair_prepared: BTreeSet<(usize, u16)>,
    is_captured: bool,
}

impl Input {
    pub fn open() -> io::Result<Self> {
        let mut keyboards = Vec::new();
        for entry in fs::read_dir("/dev/input")? {
            let entry = entry?;
            if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
                continue;
            }
            let path = entry.path();
            let probe = match RawDevice::open(&path) {
                Ok(device) => device,
                Err(error) => {
                    eprintln!("Cannot open {}: {error}", path.display());
                    continue;
                }
            };
            if is_own_keyboard(probe.physical_path(), probe.name()) || !is_keyboard(&probe) {
                continue;
            }
            let device = open_device(&path)?;
            let held = device.get_key_state()?.iter().map(|key| key.0).collect();
            eprintln!("OPEN {} {:?} held={held:?}", path.display(), device.name());
            keyboards.push(Keyboard {
                path,
                device: Box::new(device),
                held,
            });
        }
        if keyboards.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no accessible keyboards",
            ));
        }
        let mut input = Self {
            keyboards,
            history: History::default(),
            pending: None,
            is_original_pause_held: false,
            recording: None,
            repair: BTreeSet::new(),
            repair_prepared: BTreeSet::new(),
            is_captured: false,
        };
        for keyboard in &input.keyboards {
            for &code in &keyboard.held {
                if MODIFIERS.contains(&KeyCode::new(code)) {
                    input.history.process(&keyboard.path, KeyCode::new(code), 1);
                }
            }
        }
        Ok(input)
    }

    pub fn wait_for_request(&mut self) -> io::Result<()> {
        while self.pending.is_none() {
            self.drain()?;
            thread::sleep(POLL_INTERVAL);
        }
        Ok(())
    }

    pub fn enter(&mut self) -> io::Result<()> {
        let mut is_pause_reset = false;
        let mut attempt = 0_u64;
        loop {
            self.wait_ordinary()?;
            attempt += 1;
            eprintln!("ENTRY attempt={attempt} begin");
            self.recording = Some(EntryRecording::default());
            self.release_locks()?;
            self.release_modifiers()?;
            if !is_pause_reset {
                self.write_source(self.pending.unwrap(), KeyCode::KEY_PAUSE, 0)?;
                is_pause_reset = true;
                self.is_original_pause_held = false;
            }
            self.drain()?;
            self.grab()?;
            self.drain()?;
            self.snapshot()?;
            self.drain()?;
            let presses = self.recording.take().unwrap().presses;
            eprintln!("ENTRY attempt={attempt} recorded {presses:?}");
            if presses.is_empty() {
                self.is_captured = true;
                self.pending = None;
                eprintln!("ENTRY accepted");
                return Ok(());
            }
            self.repair = presses;
            self.rollback()?;
        }
    }

    pub fn latest_word(&self) -> Option<&crate::history::Word> {
        self.history.latest_word()
    }

    pub fn read_captured(&mut self) -> io::Result<()> {
        self.drain()
    }

    pub fn leave(&mut self) -> io::Result<()> {
        eprintln!("WAIT_RELEASES begin");
        loop {
            self.drain()?;
            self.release_modifiers()?;
            self.drain()?;
            if self
                .keyboards
                .iter()
                .all(|keyboard| keyboard.held.is_empty())
            {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        self.ungrab()?;
        self.is_captured = false;
        self.release_locks()?;
        self.drain()?;
        eprintln!("WAIT_RELEASES complete");
        Ok(())
    }

    fn wait_ordinary(&mut self) -> io::Result<()> {
        eprintln!("WAIT_ORDINARY begin");
        loop {
            self.drain()?;
            let pause_source = self.pending.unwrap();
            let is_held = self.keyboards.iter().enumerate().any(|(index, keyboard)| {
                keyboard.held.iter().any(|&code| {
                    !MODIFIERS.contains(&KeyCode::new(code))
                        && !(self.is_original_pause_held
                            && index == pause_source
                            && code == KeyCode::KEY_PAUSE.0)
                })
            });
            if !is_held {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn drain(&mut self) -> io::Result<()> {
        loop {
            let mut count = 0;
            for index in 0..self.keyboards.len() {
                let events = match self.keyboards[index].device.events() {
                    Ok(events) => events,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                count += events.len();
                for event in events {
                    self.process(index, event)?;
                }
            }
            if count == 0 {
                return Ok(());
            }
        }
    }

    fn process(&mut self, index: usize, event: InputEvent) -> io::Result<()> {
        if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_DROPPED.0
        {
            return Err(io::Error::other(format!(
                "{}: SYN_DROPPED",
                self.keyboards[index].path.display()
            )));
        }
        if event.event_type() != EventType::KEY {
            return Ok(());
        }
        let key = KeyCode::new(event.code());
        let value = event.value();
        let keyboard = &mut self.keyboards[index];
        eprintln!(
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
        if let Some(recording) = &mut self.recording {
            recording.record(index, key.0, value);
        }
        if self.is_captured || keyboard.device.is_grabbed() {
            return Ok(());
        }
        if key == KeyCode::KEY_PAUSE {
            if value == 0 && self.pending == Some(index) {
                self.is_original_pause_held = false;
            }
            if value == 1
                && self.pending.is_none()
                && !self.keyboards.iter().any(|keyboard| {
                    keyboard
                        .held
                        .iter()
                        .any(|&code| MODIFIERS.contains(&KeyCode::new(code)))
                })
            {
                self.pending = Some(index);
                self.is_original_pause_held = true;
            }
            return Ok(());
        }
        let lines = self.history.process(&keyboard.path, key, value);
        if !lines.is_empty() {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            for line in lines {
                writeln!(output, "{line}")?;
            }
            output.flush()?;
        }
        Ok(())
    }

    fn write_source(&mut self, index: usize, key: KeyCode, value: i32) -> io::Result<()> {
        let keyboard = &mut self.keyboards[index];
        eprintln!(
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
        eprintln!(
            "SOURCE_WRITE result {} {key:?} value={value}: {result:?}",
            keyboard.path.display()
        );
        result?;
        if value == 0 {
            keyboard.held.remove(&key.0);
            if MODIFIERS.contains(&key) {
                self.history.process(&keyboard.path, key, 0);
            }
        } else {
            keyboard.held.insert(key.0);
        }
        Ok(())
    }

    fn release_modifiers(&mut self) -> io::Result<()> {
        for index in 0..self.keyboards.len() {
            for key in MODIFIERS {
                if self.keyboards[index].held.contains(&key.0) {
                    self.write_source(index, key, 0)?;
                }
            }
        }
        Ok(())
    }

    fn release_locks(&mut self) -> io::Result<()> {
        for index in 0..self.keyboards.len() {
            for key in LOCK_KEYS {
                self.write_source(index, key, 0)?;
            }
        }
        Ok(())
    }

    fn grab(&mut self) -> io::Result<()> {
        for keyboard in &mut self.keyboards {
            let result = keyboard.device.grab();
            eprintln!("GRAB {} {result:?}", keyboard.path.display());
            result?;
        }
        Ok(())
    }

    fn ungrab(&mut self) -> io::Result<()> {
        let mut error = None;
        for keyboard in &mut self.keyboards {
            if keyboard.device.is_grabbed() {
                let result = keyboard.device.ungrab();
                eprintln!("UNGRAB {} {result:?}", keyboard.path.display());
                if let Err(err) = result {
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn snapshot(&mut self) -> io::Result<()> {
        for (index, keyboard) in self.keyboards.iter_mut().enumerate() {
            keyboard.held = keyboard.device.keys()?;
            if let Some(recording) = &mut self.recording {
                recording
                    .presses
                    .extend(keyboard.held.iter().map(|&code| (index, code)));
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> io::Result<()> {
        eprintln!("ROLLBACK begin {:?}", self.repair);
        let mut error = None;
        for (index, code) in self.repair.clone() {
            if !is_repair_key(KeyCode::new(code)) {
                self.repair.remove(&(index, code));
                continue;
            }
            if !self.keyboards[index].device.is_grabbed() {
                if !self.repair_prepared.contains(&(index, code)) {
                    self.repair.remove(&(index, code));
                }
                continue;
            }
            self.repair_prepared.insert((index, code));
            if let Err(err) = self.write_source(index, KeyCode::new(code), 1) {
                error.get_or_insert(err);
            }
        }
        if let Err(err) = self.drain() {
            error.get_or_insert(err);
        }
        if let Err(err) = self.ungrab() {
            error.get_or_insert(err);
        }
        for (index, code) in self.repair.clone() {
            if !self.keyboards[index].device.is_grabbed() {
                match self.write_source(index, KeyCode::new(code), 0) {
                    Ok(()) => {
                        self.repair.remove(&(index, code));
                        self.repair_prepared.remove(&(index, code));
                    }
                    Err(err) => {
                        error.get_or_insert(err);
                    }
                }
            }
        }
        if let Err(err) = self.release_locks() {
            error.get_or_insert(err);
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        eprintln!("CLEANUP begin");
        if self.recording.is_some() {
            let _ = self.drain();
            let _ = self.snapshot();
            self.repair
                .extend(self.recording.take().unwrap_or_default().presses);
        }
        if !self.repair.is_empty()
            && let Err(error) = self.rollback()
        {
            eprintln!("CLEANUP rollback: {error}");
        }
        if self.is_captured
            && let Err(error) = self.release_modifiers()
        {
            eprintln!("CLEANUP modifiers: {error}");
        }
        if let Err(error) = self.ungrab() {
            eprintln!("CLEANUP ungrab: {error}");
        }
        if let Err(error) = self.release_locks() {
            eprintln!("CLEANUP locks: {error}");
        }
        eprintln!("CLEANUP end");
    }
}

fn is_keyboard(device: &RawDevice) -> bool {
    device.supported_keys().is_some_and(|keys| {
        [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
            .iter()
            .all(|key| keys.contains(*key))
    })
}

fn open_device(path: &Path) -> io::Result<RawDevice> {
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
    configuration.set_nonblocking(true)?;
    RawDevice::try_from(file)
}

fn is_repair_key(key: KeyCode) -> bool {
    ((KeyCode::KEY_ESC.0..=KeyCode::KEY_KPDOT.0).contains(&key.0) && !LOCK_KEYS.contains(&key))
        || MODIFIERS.contains(&key)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct SimulatedKeyboard {
        events: Vec<InputEvent>,
        held: BTreeSet<u16>,
        writes: Vec<(KeyCode, i32, bool)>,
        is_grabbed: bool,
        grab_count: usize,
        is_entry_interrupted: bool,
        is_grab_failed: bool,
        is_repair_release_failed: bool,
    }

    struct SimulatedSource(Rc<RefCell<SimulatedKeyboard>>);

    impl Source for SimulatedSource {
        fn events(&mut self) -> io::Result<Vec<InputEvent>> {
            Ok(std::mem::take(&mut self.0.borrow_mut().events))
        }

        fn keys(&self) -> io::Result<BTreeSet<u16>> {
            Ok(self.0.borrow().held.clone())
        }

        fn send(&mut self, events: &[InputEvent]) -> io::Result<()> {
            let mut keyboard = self.0.borrow_mut();
            let event = events[0];
            let code = event.code();
            let value = event.value();
            let is_grabbed = keyboard.is_grabbed;
            keyboard
                .writes
                .push((KeyCode::new(code), value, is_grabbed));
            if code == KeyCode::KEY_A.0
                && value == 0
                && !is_grabbed
                && keyboard.is_repair_release_failed
            {
                keyboard.is_repair_release_failed = false;
                return Err(io::Error::other("release failed"));
            }
            let changed = if value == 0 {
                keyboard.held.remove(&code)
            } else {
                keyboard.held.insert(code)
            };
            if changed {
                keyboard.events.push(event);
            }
            Ok(())
        }

        fn grab(&mut self) -> io::Result<()> {
            let mut keyboard = self.0.borrow_mut();
            if keyboard.is_grab_failed {
                return Err(io::Error::other("grab failed"));
            }
            keyboard.is_grabbed = true;
            keyboard.grab_count += 1;
            if keyboard.is_entry_interrupted {
                keyboard.is_entry_interrupted = false;
                for value in [1, 0] {
                    keyboard.events.push(InputEvent::new(
                        EventType::KEY.0,
                        KeyCode::KEY_A.0,
                        value,
                    ));
                }
            }
            Ok(())
        }

        fn ungrab(&mut self) -> io::Result<()> {
            self.0.borrow_mut().is_grabbed = false;
            Ok(())
        }

        fn is_grabbed(&self) -> bool {
            self.0.borrow().is_grabbed
        }
    }

    fn simulated_input(sources: &[Rc<RefCell<SimulatedKeyboard>>]) -> Input {
        let keyboards = sources
            .iter()
            .enumerate()
            .map(|(index, source)| Keyboard {
                path: PathBuf::from(format!("keyboard-{index}")),
                device: Box::new(SimulatedSource(source.clone())),
                held: source.borrow().held.clone(),
            })
            .collect();
        Input {
            keyboards,
            history: History::default(),
            pending: Some(0),
            is_original_pause_held: true,
            recording: None,
            repair: BTreeSet::new(),
            repair_prepared: BTreeSet::new(),
            is_captured: false,
        }
    }

    #[test]
    fn interrupted_entry_repairs_hidden_key_and_retries() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            held: BTreeSet::from([KeyCode::KEY_PAUSE.0]),
            is_entry_interrupted: true,
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        input.enter().unwrap();
        let keyboard = source.borrow();
        assert_eq!(keyboard.grab_count, 2);
        assert!(keyboard.is_grabbed);
        let repairs: Vec<_> = keyboard
            .writes
            .iter()
            .filter(|(key, _, _)| *key == KeyCode::KEY_A)
            .copied()
            .collect();
        assert_eq!(
            repairs,
            [(KeyCode::KEY_A, 1, true), (KeyCode::KEY_A, 0, false)]
        );
        assert_eq!(
            keyboard
                .writes
                .iter()
                .filter(|(key, _, _)| *key == KeyCode::KEY_PAUSE)
                .count(),
            1
        );
    }

    #[test]
    fn exit_resets_reasserted_modifiers_on_their_source() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        input.is_captured = true;
        for source in [&first, &second] {
            source.borrow_mut().is_grabbed = true;
        }
        second.borrow_mut().held.insert(KeyCode::KEY_RIGHTCTRL.0);
        second.borrow_mut().events.push(InputEvent::new(
            EventType::KEY.0,
            KeyCode::KEY_RIGHTCTRL.0,
            2,
        ));
        input.leave().unwrap();
        assert!(
            second
                .borrow()
                .writes
                .contains(&(KeyCode::KEY_RIGHTCTRL, 0, true))
        );
        assert!(
            !first
                .borrow()
                .writes
                .iter()
                .any(|(key, _, _)| *key == KeyCode::KEY_RIGHTCTRL)
        );
        assert!(!second.borrow().is_grabbed);
    }

    #[test]
    fn failed_partial_grab_repairs_and_releases_acquired_device() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            ..Default::default()
        }));
        let second = Rc::new(RefCell::new(SimulatedKeyboard {
            is_grab_failed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        assert!(input.enter().is_err());
        drop(input);
        let first = first.borrow();
        let repairs: Vec<_> = first
            .writes
            .iter()
            .filter(|(key, _, _)| *key == KeyCode::KEY_A)
            .copied()
            .collect();
        assert_eq!(
            repairs,
            [(KeyCode::KEY_A, 1, true), (KeyCode::KEY_A, 0, false)]
        );
        assert!(!first.is_grabbed);
        assert!(!second.borrow().is_grabbed);
    }

    #[test]
    fn failed_repair_release_is_retried_during_cleanup() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            is_repair_release_failed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        assert!(input.enter().is_err());
        drop(input);
        let source = source.borrow();
        let writes: Vec<_> = source
            .writes
            .iter()
            .filter(|(key, _, _)| *key == KeyCode::KEY_A)
            .copied()
            .collect();
        assert_eq!(
            writes,
            [
                (KeyCode::KEY_A, 1, true),
                (KeyCode::KEY_A, 0, false),
                (KeyCode::KEY_A, 0, false)
            ]
        );
        assert!(!source.held.contains(&KeyCode::KEY_A.0));
    }

    #[test]
    fn dropped_events_abort_capture_and_cleanup_releases_device() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            is_grabbed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        input.is_captured = true;
        source.borrow_mut().events.push(InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_DROPPED.0,
            0,
        ));
        assert!(
            input
                .read_captured()
                .unwrap_err()
                .to_string()
                .contains("SYN_DROPPED")
        );
        drop(input);
        assert!(!source.borrow().is_grabbed);
    }

    #[test]
    fn entry_retains_released_keys_and_tracks_each_source_and_repeat() {
        let mut recording = EntryRecording::default();
        recording.record(0, KeyCode::KEY_A.0, 1);
        recording.record(0, KeyCode::KEY_A.0, 0);
        recording.record(1, KeyCode::KEY_A.0, 2);
        recording.record(1, KeyCode::KEY_B.0, 0);
        assert_eq!(
            recording.presses,
            BTreeSet::from([(0, KeyCode::KEY_A.0), (1, KeyCode::KEY_A.0)])
        );
    }

    #[test]
    fn rollback_never_presses_lock_or_unsupported_keys() {
        for key in LOCK_KEYS
            .into_iter()
            .chain([KeyCode::KEY_POWER, KeyCode::KEY_F13])
        {
            assert!(!is_repair_key(key), "{key:?}");
        }
        for key in MODIFIERS.into_iter().chain([
            KeyCode::KEY_A,
            KeyCode::KEY_PAUSE,
            KeyCode::KEY_HOME,
            KeyCode::KEY_F12,
        ]) {
            assert!(is_repair_key(key), "{key:?}");
        }
    }
}
