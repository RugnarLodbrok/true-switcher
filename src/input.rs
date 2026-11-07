use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode, raw_stream::RawDevice};

use crate::history::History;
use crate::hold::HoldDetector;
use crate::input_events::{InputEventMessage, InputEventSender, InputEvents};
use crate::output::is_own_keyboard;

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
const NAVIGATION_KEYS: [KeyCode; 9] = [
    KeyCode::KEY_UP,
    KeyCode::KEY_DOWN,
    KeyCode::KEY_LEFT,
    KeyCode::KEY_RIGHT,
    KeyCode::KEY_PAGEUP,
    KeyCode::KEY_PAGEDOWN,
    KeyCode::KEY_HOME,
    KeyCode::KEY_END,
    KeyCode::KEY_ESC,
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

struct Pointer {
    path: PathBuf,
}

trait Source {
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

pub struct Input {
    keyboards: Vec<Keyboard>,
    pointers: Vec<Pointer>,
    history: History,
    events: InputEvents,
    holds: HoldDetector,
    correction_result: Option<io::Result<()>>,
    is_correction_cancelled: bool,
    switch_key: KeyCode,
    is_cancel_requested: bool,
    pending: Option<usize>,
    is_original_switch_key_held: bool,
    recording: Option<EntryRecording>,
    repair: BTreeSet<(usize, u16)>,
    repair_prepared: BTreeSet<(usize, u16)>,
    is_captured: bool,
    is_backing_off: bool,
    is_auto_request: bool,
    automatic_target: Option<Arc<crate::layout::Layout>>,
}

impl Input {
    pub fn open(
        events: InputEvents,
        sender: InputEventSender,
        switch_key: KeyCode,
        history: History,
    ) -> io::Result<Self> {
        let mut input = Self {
            keyboards: Vec::new(),
            pointers: Vec::new(),
            history,
            events,
            holds: HoldDetector::default(),
            correction_result: None,
            is_correction_cancelled: false,
            switch_key,
            is_cancel_requested: false,
            pending: None,
            is_original_switch_key_held: false,
            recording: None,
            repair: BTreeSet::new(),
            repair_prepared: BTreeSet::new(),
            is_captured: false,
            is_backing_off: false,
            is_auto_request: false,
            automatic_target: None,
        };
        input.discover_devices(&sender)?;
        for keyboard in &input.keyboards {
            for &code in &keyboard.held {
                if MODIFIERS.contains(&KeyCode::new(code)) {
                    input.history.process(&keyboard.path, KeyCode::new(code), 1);
                }
            }
        }
        Ok(input)
    }

    fn discover_devices(&mut self, sender: &InputEventSender) -> io::Result<()> {
        for entry in fs::read_dir("/dev/input")? {
            let entry = entry?;
            if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
                continue;
            }
            let path = entry.path();
            let probe = match RawDevice::open(&path) {
                Ok(device) => device,
                Err(error) => {
                    log::debug!("Cannot open {}: {error}", path.display());
                    continue;
                }
            };
            if is_own_keyboard(probe.physical_path(), probe.name()) {
                continue;
            }
            if is_pointer(&probe) {
                self.open_pointer(&path, sender)?;
            }
            if is_keyboard(&probe) {
                self.open_keyboard(path, sender)?;
            }
        }
        if self.keyboards.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no accessible keyboards",
            ));
        }
        if self.pointers.is_empty() {
            log::warn!(
                "No accessible pointer devices; mouse clicks and touchpad taps cannot reset history"
            );
        }
        Ok(())
    }

    fn open_pointer(&mut self, path: &Path, sender: &InputEventSender) -> io::Result<()> {
        match open_read_device(path) {
            Ok(device) => {
                log::debug!("OPEN POINTER {} {:?}", path.display(), device.name());
                start_reader(
                    device,
                    path.to_owned(),
                    self.pointers.len(),
                    false,
                    sender.clone(),
                )?;
                self.pointers.push(Pointer {
                    path: path.to_owned(),
                });
            }
            Err(error) => log::warn!("Cannot monitor pointer {}: {error}", path.display()),
        }
        Ok(())
    }

    fn open_keyboard(&mut self, path: PathBuf, sender: &InputEventSender) -> io::Result<()> {
        let device = open_device(&path)?;
        let held: BTreeSet<_> = device.get_key_state()?.iter().map(|key| key.0).collect();
        self.holds
            .snapshot(self.keyboards.len(), &held, Instant::now());
        log::debug!("OPEN {} {:?}", path.display(), device.name());
        log::trace!("OPEN {} held={held:?}", path.display());
        let reader = RawDevice::from_fd(device.as_fd().try_clone_to_owned()?)?;
        start_reader(
            reader,
            path.clone(),
            self.keyboards.len(),
            true,
            sender.clone(),
        )?;
        self.keyboards.push(Keyboard {
            path,
            device: Box::new(device),
            held,
        });
        Ok(())
    }

    pub fn wait_for_keyboard_discovery(&mut self, duration: Duration) -> io::Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.process_available_events()?;
            self.pending = None;
            self.is_auto_request = false;
            self.automatic_target = None;
            self.is_original_switch_key_held = false;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            self.wait_for_events(Some(deadline))?;
        }
    }

    pub fn wait_for_switch_request(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        self.process_events_until(|input| input.pending.is_some())
    }

    pub fn enter(&mut self) -> io::Result<bool> {
        let mut is_switch_key_reset = false;
        let mut attempt = 0_u64;
        loop {
            if !self.wait_ordinary()? {
                return Ok(false);
            }
            attempt += 1;
            log::debug!("ENTRY attempt={attempt} begin");
            self.recording = Some(EntryRecording::default());
            self.release_locks()?;
            self.release_modifiers()?;
            if !is_switch_key_reset {
                let source = self.pending.unwrap();
                if self.keyboards[source].held.contains(&self.switch_key.0) {
                    self.write_source(source, self.switch_key, 0)?;
                }
                is_switch_key_reset = true;
                self.is_original_switch_key_held = false;
            }
            self.process_available_events()?;
            self.grab()?;
            self.process_available_events()?;
            self.snapshot()?;
            self.process_available_events()?;
            let presses = self.recording.take().unwrap().presses;
            log::trace!("ENTRY attempt={attempt} recorded {presses:?}");
            if self.pending.is_none() {
                self.repair = presses;
                self.rollback()?;
                return Ok(false);
            }
            if presses.is_empty() {
                self.is_captured = true;
                self.is_correction_cancelled = false;
                self.pending = None;
                log::debug!("ENTRY accepted");
                return Ok(true);
            }
            self.repair = presses;
            self.rollback()?;
        }
    }

    pub fn latest_word(&self) -> Option<&crate::history::Word> {
        self.history.latest_word()
    }

    pub fn is_auto_request(&self) -> bool {
        self.is_auto_request
    }

    pub fn automatic_target(&self) -> Option<Arc<crate::layout::Layout>> {
        self.history.latest_word()?;
        self.automatic_target.clone()
    }

    pub fn previous_word(&self) -> Option<&crate::history::Word> {
        self.history.previous_word()
    }

    pub fn set_latest_layout(&mut self, layout: Option<Arc<crate::layout::Layout>>) {
        self.history.set_latest_layout(layout);
    }

    pub fn read_captured(&mut self) -> io::Result<bool> {
        self.process_available_events()?;
        Ok(self.take_cancel_request())
    }

    pub fn leave(&mut self) -> io::Result<()> {
        log::trace!("WAIT_RELEASES begin");
        loop {
            self.process_available_events()?;
            self.release_modifiers()?;
            self.process_available_events()?;
            if self
                .keyboards
                .iter()
                .all(|keyboard| keyboard.held.is_empty())
            {
                break;
            }
            self.wait_for_events(None)?;
        }
        self.ungrab()?;
        self.is_captured = false;
        self.release_locks()?;
        self.process_available_events()?;
        log::trace!("WAIT_RELEASES complete");
        Ok(())
    }

    fn wait_ordinary(&mut self) -> io::Result<bool> {
        log::trace!("WAIT_ORDINARY begin");
        self.process_events_until(|input| !input.is_ordinary_key_held())?;
        Ok(self.pending.is_some())
    }

    fn is_ordinary_key_held(&self) -> bool {
        let Some(switch_source) = self.pending else {
            return false;
        };
        self.keyboards.iter().enumerate().any(|(index, keyboard)| {
            keyboard.held.iter().any(|&code| {
                !MODIFIERS.contains(&KeyCode::new(code))
                    && !(self.is_original_switch_key_held
                        && index == switch_source
                        && code == self.switch_key.0)
            })
        })
    }

    fn process_events_until(&mut self, is_ready: impl Fn(&Self) -> bool) -> io::Result<()> {
        self.process_available_events()?;
        while !is_ready(self) {
            self.wait_for_events(None)?;
        }
        Ok(())
    }

    fn process_available_events(&mut self) -> io::Result<()> {
        self.drain_events()?;
        self.synchronize_history();
        self.update_back_off(Instant::now());
        Ok(())
    }

    fn drain_events(&mut self) -> io::Result<()> {
        while let Some(event) = self.events.available()? {
            self.process_message(event)?;
        }
        Ok(())
    }

    fn synchronize_history(&mut self) {
        if !self.is_captured && self.recording.is_none() {
            self.history.synchronize_layout();
            if self.history.latest_word().is_none() {
                self.automatic_target = None;
            }
        }
    }

    fn wait_for_events(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        let hold_deadline = if self.is_backing_off {
            None
        } else {
            self.holds.deadline()
        };
        let deadline = deadline.into_iter().chain(hold_deadline).min();
        if let Some(event) = self.events.wait(deadline)? {
            self.process_message(event)?;
        }
        self.process_available_events()
    }

    fn process_message(&mut self, event: InputEventMessage) -> io::Result<()> {
        match event {
            InputEventMessage::Keyboard(index, event) => self.process(index, event)?,
            InputEventMessage::Pointer(index, event) => self.process_pointer(index, event)?,
            InputEventMessage::DeviceError(error) => return Err(error),
            InputEventMessage::FocusChanged => self.request_history_reset(),
            InputEventMessage::LayoutChanged(layout) => {
                self.history
                    .accept_layout(layout, self.is_captured || self.recording.is_some());
                if self.history.latest_word().is_none() {
                    self.automatic_target = None;
                }
            }
            InputEventMessage::CorrectionFinished { result, layout } => {
                self.history.accept_layout(layout.clone(), true);
                if result.is_ok() && !self.is_correction_cancelled {
                    self.set_latest_layout(layout);
                } else {
                    self.history.accept_layout(layout, false);
                    self.history.reset();
                }
                self.correction_result = Some(result);
            }
        }
        Ok(())
    }

    pub fn wait_for_correction(&mut self) -> io::Result<Option<io::Result<()>>> {
        if self.correction_result.is_none() {
            self.wait_for_events(None)?;
        }
        Ok(self.correction_result.take())
    }

    fn process(&mut self, index: usize, event: InputEvent) -> io::Result<()> {
        let now = Instant::now();
        let time = event.timestamp();
        let elapsed = SystemTime::now().duration_since(time).ok();
        let now = if time == SystemTime::UNIX_EPOCH {
            now
        } else {
            elapsed.and_then(|age| now.checked_sub(age)).unwrap_or(now)
        };
        self.process_at(index, event, now)
    }

    fn process_at(&mut self, index: usize, event: InputEvent, now: Instant) -> io::Result<()> {
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
        self.update_back_off(now);
        self.holds.update(index, key.0, value, now);
        self.track_key(index, event);
        if let Some(recording) = &mut self.recording {
            recording.record(index, key.0, value);
        }
        if value == 2 && !is_shift(key) {
            self.start_back_off();
        }
        let path = self.keyboards[index].path.clone();
        if self.is_backing_off {
            if MODIFIERS.contains(&key) {
                self.history.process(&path, key, value);
            }
            if value == 0 {
                self.request_history_reset();
            }
            self.update_back_off(now);
            return Ok(());
        }
        self.process_history_event(index, &path, key, value)
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

    fn process_history_event(
        &mut self,
        index: usize,
        path: &Path,
        key: KeyCode,
        value: i32,
    ) -> io::Result<()> {
        let is_tab_separator = key == KeyCode::KEY_TAB && !self.has_shortcut_modifier();
        let is_pointer_click = value == 1 && is_pointer_press(key.0);
        if (self.should_reset_for_key(key, value) || is_pointer_click)
            && (!is_tab_separator || self.is_captured)
        {
            self.request_history_reset();
        }
        if self.is_captured || self.keyboards[index].device.is_grabbed() {
            return Ok(());
        }
        if key == self.switch_key {
            if value == 0 && self.pending == Some(index) {
                self.is_original_switch_key_held = false;
            }
            if value == 1 && self.pending.is_none() && !self.has_other_switch_modifiers(index, key)
            {
                self.pending = Some(index);
                self.is_auto_request = false;
                self.is_original_switch_key_held = true;
            }
            return Ok(());
        }
        let lines = self.history.process(path, key, value);
        if matches!(value, 1 | 2) && (key == KeyCode::KEY_BACKSPACE || key == KeyCode::KEY_SPACE) {
            self.automatic_target = None;
        }
        if matches!(value, 1 | 2)
            && crate::history::is_text_key(key)
            && !self.has_shortcut_modifier()
            && (self.pending.is_none() || self.is_auto_request)
        {
            self.automatic_target = self.history.automatic_target();
            if self.automatic_target.is_some() {
                self.pending = Some(index);
                self.is_auto_request = true;
                self.is_original_switch_key_held = false;
            }
        }
        for line in lines {
            log::trace!("{line}");
        }
        Ok(())
    }

    fn process_pointer(&mut self, index: usize, event: InputEvent) -> io::Result<()> {
        if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_DROPPED.0
        {
            self.request_history_reset();
            return Err(io::Error::other(format!(
                "{}: SYN_DROPPED",
                self.pointers[index].path.display()
            )));
        }
        if event.event_type() == EventType::KEY
            && event.value() == 1
            && is_pointer_press(event.code())
        {
            self.request_history_reset();
        }
        Ok(())
    }

    fn should_reset_for_key(&self, key: KeyCode, value: i32) -> bool {
        if !matches!(value, 1 | 2) {
            return false;
        }
        if NAVIGATION_KEYS.contains(&key) || key == KeyCode::KEY_TAB || key == KeyCode::KEY_DELETE {
            return true;
        }
        self.has_shortcut_modifier() && !MODIFIERS.contains(&key)
    }

    fn has_shortcut_modifier(&self) -> bool {
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

    fn has_other_switch_modifiers(&self, source: usize, switch_key: KeyCode) -> bool {
        self.keyboards.iter().enumerate().any(|(index, keyboard)| {
            keyboard.held.iter().any(|&code| {
                MODIFIERS.contains(&KeyCode::new(code))
                    && !(index == source && code == switch_key.0)
            })
        })
    }

    fn request_history_reset(&mut self) {
        self.automatic_target = None;
        self.history.reset();
        if self.is_captured {
            self.is_cancel_requested = true;
            self.is_correction_cancelled = true;
        }
    }

    fn start_back_off(&mut self) {
        if self.is_backing_off {
            return;
        }
        self.is_backing_off = true;
        self.request_history_reset();
        self.pending = None;
        self.is_auto_request = false;
        self.is_original_switch_key_held = false;
    }

    fn update_back_off(&mut self, now: Instant) {
        if self.holds.expire(now) {
            self.start_back_off();
        }
        if self.is_backing_off
            && self
                .keyboards
                .iter()
                .all(|keyboard| keyboard.held.is_empty())
        {
            self.is_backing_off = false;
        }
    }

    pub fn reset_history(&mut self) {
        self.history.reset();
    }

    pub fn take_cancel_request(&mut self) -> bool {
        std::mem::take(&mut self.is_cancel_requested)
    }

    fn write_source(&mut self, index: usize, key: KeyCode, value: i32) -> io::Result<()> {
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
            if result.is_ok() {
                log::debug!("GRAB {}", keyboard.path.display());
            }
            result?;
        }
        Ok(())
    }

    fn ungrab(&mut self) -> io::Result<()> {
        let mut error = None;
        for keyboard in &mut self.keyboards {
            if keyboard.device.is_grabbed() {
                let result = keyboard.device.ungrab();
                if result.is_ok() {
                    log::debug!("UNGRAB {}", keyboard.path.display());
                }
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
            self.holds.snapshot(index, &keyboard.held, Instant::now());
            if let Some(recording) = &mut self.recording {
                recording
                    .presses
                    .extend(keyboard.held.iter().map(|&code| (index, code)));
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> io::Result<()> {
        log::debug!("ROLLBACK begin");
        log::trace!("ROLLBACK keys {:?}", self.repair);
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
        if let Err(err) = self.process_available_events() {
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

fn is_shift(key: KeyCode) -> bool {
    matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT)
}

impl Drop for Input {
    fn drop(&mut self) {
        log::debug!("CLEANUP begin");
        if self.recording.is_some() {
            let _ = self.process_available_events();
            let _ = self.snapshot();
            self.repair
                .extend(self.recording.take().unwrap_or_default().presses);
        }
        if !self.repair.is_empty()
            && let Err(error) = self.rollback()
        {
            log::error!("CLEANUP rollback: {error}");
        }
        if self.is_captured
            && let Err(error) = self.release_modifiers()
        {
            log::error!("CLEANUP modifiers: {error}");
        }
        if let Err(error) = self.ungrab() {
            log::error!("CLEANUP ungrab: {error}");
        }
        if let Err(error) = self.release_locks() {
            log::error!("CLEANUP locks: {error}");
        }
        log::debug!("CLEANUP end");
    }
}

fn is_keyboard(device: &RawDevice) -> bool {
    device.supported_keys().is_some_and(|keys| {
        [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
            .iter()
            .all(|key| keys.contains(*key))
    })
}

fn is_pointer(device: &RawDevice) -> bool {
    device.supported_keys().is_some_and(|keys| {
        (KeyCode::BTN_LEFT.0..=KeyCode::BTN_TASK.0).any(|code| keys.contains(KeyCode::new(code)))
            || keys.contains(KeyCode::BTN_TOUCH)
    })
}

fn is_pointer_press(code: u16) -> bool {
    (KeyCode::BTN_LEFT.0..=KeyCode::BTN_TASK.0).contains(&code) || code == KeyCode::BTN_TOUCH.0
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
    RawDevice::try_from(file)
}

fn open_read_device(path: &Path) -> io::Result<RawDevice> {
    let file = OpenOptions::new().read(true).open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{} requires read access: {error}", path.display()),
        )
    })?;
    RawDevice::try_from(file)
}

fn start_reader(
    mut device: RawDevice,
    path: PathBuf,
    index: usize,
    is_keyboard: bool,
    sender: InputEventSender,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("input-{}", path.display()))
        .spawn(move || {
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            let message = if is_keyboard {
                                InputEventMessage::Keyboard(index, event)
                            } else {
                                InputEventMessage::Pointer(index, event)
                            };
                            if !sender.send(message) {
                                return;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        sender.send(InputEventMessage::DeviceError(io::Error::new(
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
    use crate::hold::HOLD_TIMEOUT;
    use std::cell::RefCell;
    use std::rc::Rc;

    const TEST_STEP: Duration = Duration::from_millis(5);

    #[derive(Default)]
    struct SimulatedEvents {
        sender: Option<InputEventSender>,
        index: usize,
    }

    impl SimulatedEvents {
        fn push(&mut self, event: InputEvent) {
            assert!(
                self.sender
                    .as_ref()
                    .unwrap()
                    .send(InputEventMessage::Keyboard(self.index, event))
            );
        }

        fn extend(&mut self, events: impl IntoIterator<Item = InputEvent>) {
            for event in events {
                self.push(event);
            }
        }
    }

    #[derive(Default)]
    struct SimulatedKeyboard {
        events: SimulatedEvents,
        held: BTreeSet<u16>,
        writes: Vec<(KeyCode, i32, bool)>,
        is_grabbed: bool,
        grab_count: usize,
        is_entry_interrupted: bool,
        is_entry_repeated: bool,
        is_grab_failed: bool,
        is_repair_release_failed: bool,
    }

    struct SimulatedSource(Rc<RefCell<SimulatedKeyboard>>);

    impl Source for SimulatedSource {
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
            if keyboard.is_entry_repeated {
                keyboard.is_entry_repeated = false;
                for value in [1, 2, 0] {
                    keyboard.events.push(InputEvent::new(
                        EventType::KEY.0,
                        KeyCode::KEY_A.0,
                        value,
                    ));
                }
            }
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
        let (sender, events) = InputEvents::new();
        let mut holds = HoldDetector::default();
        let keyboards = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                source.borrow_mut().events = SimulatedEvents {
                    sender: Some(sender.clone()),
                    index,
                };
                holds.snapshot(index, &source.borrow().held, Instant::now());
                Keyboard {
                    path: PathBuf::from(format!("keyboard-{index}")),
                    device: Box::new(SimulatedSource(source.clone())),
                    held: source.borrow().held.clone(),
                }
            })
            .collect();
        Input {
            keyboards,
            pointers: Vec::new(),
            history: History::default(),
            events,
            holds,
            correction_result: None,
            is_correction_cancelled: false,
            switch_key: KeyCode::KEY_PAUSE,
            is_cancel_requested: false,
            pending: Some(0),
            is_original_switch_key_held: true,
            recording: None,
            repair: BTreeSet::new(),
            repair_prepared: BTreeSet::new(),
            is_captured: false,
            is_backing_off: false,
            is_auto_request: false,
            automatic_target: None,
        }
    }

    fn auto_input() -> Input {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[source]);
        input.pending = None;
        input.is_original_switch_key_held = false;
        let settings = crate::config::from_contents(
            r#"
switch_key = 'KEY_PAUSE'
auto_switch = true
[[switch_rules]]
target = 'us'
patterns = ['^/[a-z]']
[[switch_rules]]
target = 'ru'
patterns = ['^\.тф']
"#,
        )
        .unwrap();
        let switch = crate::switch_rules::AutoSwitchRules::compile(&settings, |name| {
            Ok(Arc::new(crate::layout::Layout {
                name: name.into(),
                ..Default::default()
            }))
        })
        .unwrap();
        input.history = History::default().with_auto_switch(switch);
        input.history.accept_layout(
            Some(Arc::new(crate::layout::Layout {
                name: "ru".into(),
                ..Default::default()
            })),
            false,
        );
        input
    }

    fn press(input: &mut Input, key: KeyCode) {
        input
            .process(0, InputEvent::new(EventType::KEY.0, key.0, 1))
            .unwrap();
    }

    fn event_at(input: &mut Input, source: usize, key: KeyCode, value: i32, now: Instant) {
        input
            .process_at(source, InputEvent::new(EventType::KEY.0, key.0, value), now)
            .unwrap();
    }

    fn layout(name: &str) -> Arc<crate::layout::Layout> {
        Arc::new(crate::layout::Layout {
            name: name.into(),
            ..Default::default()
        })
    }

    fn channel_input() -> (Input, InputEventSender) {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let input = simulated_input(std::slice::from_ref(&source));
        let sender = source.borrow().events.sender.as_ref().unwrap().clone();
        (input, sender)
    }

    #[test]
    fn existing_switch_request_returns_without_consuming_events() {
        let (mut input, sender) = channel_input();
        sender.send(InputEventMessage::FocusChanged);

        input.wait_for_switch_request().unwrap();

        assert_eq!(input.pending, Some(0));
        assert!(matches!(
            input.events.available().unwrap(),
            Some(InputEventMessage::FocusChanged)
        ));
    }

    #[test]
    fn switch_request_wait_processes_the_entire_event_batch() {
        let (mut input, sender) = channel_input();
        input.pending = None;
        input.is_original_switch_key_held = false;
        let worker = thread::spawn(move || {
            for value in [1, 0] {
                sender.send(InputEventMessage::Keyboard(
                    0,
                    InputEvent::new(EventType::KEY.0, KeyCode::KEY_PAUSE.0, value),
                ));
            }
        });
        worker.join().unwrap();

        input.wait_for_switch_request().unwrap();

        assert_eq!(input.pending, Some(0));
        assert!(!input.is_original_switch_key_held);
        assert!(!input.keyboards[0].held.contains(&KeyCode::KEY_PAUSE.0));
        assert!(input.events.available().unwrap().is_none());
    }

    #[test]
    fn switch_request_wait_propagates_device_errors() {
        let (mut input, sender) = channel_input();
        input.pending = None;
        sender.send(InputEventMessage::DeviceError(io::Error::other(
            "device disconnected",
        )));

        assert_eq!(
            input.wait_for_switch_request().unwrap_err().to_string(),
            "device disconnected"
        );
    }

    #[test]
    fn queued_layouts_are_applied_in_order_and_duplicate_layouts_keep_history() {
        let (mut input, sender) = channel_input();
        sender.send(InputEventMessage::LayoutChanged(Some(layout("us"))));
        sender.send(InputEventMessage::Keyboard(
            0,
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1),
        ));
        sender.send(InputEventMessage::LayoutChanged(Some(layout("us"))));
        input.process_available_events().unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("a"));
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        input.process_available_events().unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn completion_replaces_deferred_layout_and_late_duplicate_preserves_corrected_word() {
        let (mut input, sender) = channel_input();
        input.history.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        input.is_captured = true;
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        input.process_available_events().unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("a"));
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        assert!(input.wait_for_correction().unwrap().unwrap().is_ok());
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("ф"));
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        input.leave().unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("ф"));
    }

    #[test]
    fn layout_change_after_completion_is_reconciled_when_capture_ends() {
        let (mut input, sender) = channel_input();
        input.history.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        input.is_captured = true;
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutChanged(Some(layout("us"))));
        assert!(input.wait_for_correction().unwrap().unwrap().is_ok());
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        input.leave().unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn consumed_cancellation_still_prevents_applying_successful_correction() {
        let (mut input, sender) = channel_input();
        press(&mut input, KeyCode::KEY_A);
        input.is_captured = true;
        sender.send(InputEventMessage::FocusChanged);
        assert!(input.read_captured().unwrap());
        assert!(!input.take_cancel_request());
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        assert!(input.wait_for_correction().unwrap().unwrap().is_ok());
        assert!(input.latest_word().is_none());
        input.is_captured = false;
        press(&mut input, KeyCode::KEY_B);
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("и"));
    }

    #[test]
    fn completion_and_device_errors_wake_input_without_keyboard_events() {
        let (mut input, sender) = channel_input();
        let worker = thread::spawn(move || {
            sender.send(InputEventMessage::CorrectionFinished {
                result: Err(io::Error::other("correction failed")),
                layout: None,
            });
        });
        assert_eq!(
            input
                .wait_for_correction()
                .unwrap()
                .unwrap()
                .unwrap_err()
                .to_string(),
            "correction failed"
        );
        worker.join().unwrap();
        let (mut input, sender) = channel_input();
        sender.send(InputEventMessage::DeviceError(io::Error::other(
            "device disconnected",
        )));
        assert_eq!(
            input.wait_for_events(None).unwrap_err().to_string(),
            "device disconnected"
        );
    }

    #[test]
    fn waiting_expires_hold_timer_without_new_messages() {
        let (mut input, _sender) = channel_input();
        event_at(
            &mut input,
            0,
            KeyCode::KEY_A,
            1,
            Instant::now() - HOLD_TIMEOUT - TEST_STEP,
        );
        assert!(!input.is_backing_off);
        input.wait_for_events(None).unwrap();
        assert!(input.is_backing_off);
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn repeats_reset_both_words_cancel_manual_requests_and_do_not_write_to_devices() {
        for key in [
            KeyCode::KEY_A,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_KP1,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_PAUSE,
        ] {
            let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
            let mut input = simulated_input(std::slice::from_ref(&source));
            let now = Instant::now();
            for key in [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_B] {
                event_at(&mut input, 0, key, 1, now);
                event_at(&mut input, 0, key, 0, now);
            }
            assert!(input.previous_word().is_some());
            event_at(&mut input, 0, key, 1, now);
            event_at(&mut input, 0, key, 2, now);
            assert!(input.is_backing_off, "{key:?}");
            assert!(input.latest_word().is_none());
            assert!(input.previous_word().is_none());
            assert!(input.pending.is_none());
            assert!(!input.is_original_switch_key_held);
            assert!(source.borrow().writes.is_empty());
            event_at(&mut input, 0, key, 0, now);
            assert!(!input.is_backing_off);
            event_at(&mut input, 0, KeyCode::KEY_C, 1, now);
            assert_eq!(input.latest_word().unwrap().strokes.len(), 1);
        }
    }

    #[test]
    fn hold_timeout_runs_without_events_and_is_individual_for_each_key() {
        let mut input = auto_input();
        let now = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_A, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_B, 1, now + HOLD_TIMEOUT / 2);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, now + HOLD_TIMEOUT / 2);
        input.update_back_off(now + HOLD_TIMEOUT);
        assert!(!input.is_backing_off);
        input.holds.update(0, KeyCode::KEY_B.0, 0, now);
        input.holds.update(
            0,
            KeyCode::KEY_B.0,
            1,
            Instant::now() - HOLD_TIMEOUT - TEST_STEP,
        );
        input.process_available_events().unwrap();
        assert!(input.is_backing_off);
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn long_release_resets_history_even_without_an_intermediate_timer_check() {
        let mut input = auto_input();
        let now = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_A, 1, now);
        event_at(
            &mut input,
            0,
            KeyCode::KEY_A,
            0,
            now + HOLD_TIMEOUT + TEST_STEP,
        );
        assert!(input.latest_word().is_none());
        assert!(!input.is_backing_off);
    }

    #[test]
    fn shift_alone_never_triggers_back_off_but_all_keyboards_must_be_released() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first, second]);
        let now = Instant::now();
        for key in [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT] {
            event_at(&mut input, 1, key, 1, now);
            event_at(&mut input, 1, key, 2, now);
        }
        let later = now + HOLD_TIMEOUT + TEST_STEP;
        input.update_back_off(later);
        assert!(!input.is_backing_off);
        event_at(&mut input, 0, KeyCode::KEY_A, 1, later);
        event_at(&mut input, 0, KeyCode::KEY_A, 2, later);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, later);
        event_at(&mut input, 0, KeyCode::KEY_B, 1, later);
        event_at(&mut input, 0, KeyCode::KEY_B, 0, later);
        event_at(&mut input, 0, KeyCode::KEY_PAUSE, 1, later);
        event_at(&mut input, 0, KeyCode::KEY_PAUSE, 0, later);
        assert!(input.is_backing_off);
        assert!(input.pending.is_none());
        assert!(input.latest_word().is_none());
        event_at(&mut input, 1, KeyCode::KEY_LEFTSHIFT, 0, later);
        assert!(input.is_backing_off);
        event_at(&mut input, 1, KeyCode::KEY_RIGHTSHIFT, 0, later);
        assert!(!input.is_backing_off);
        event_at(&mut input, 0, KeyCode::KEY_C, 1, later);
        assert!(!input.latest_word().unwrap().strokes[0].shift);
    }

    #[test]
    fn back_off_cancels_automatic_and_captured_corrections() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        press(&mut input, KeyCode::KEY_N);
        assert!(input.automatic_target().is_some());
        input.is_captured = true;
        input
            .process(0, InputEvent::new(EventType::KEY.0, KeyCode::KEY_N.0, 2))
            .unwrap();
        assert!(input.pending.is_none());
        assert!(input.automatic_target().is_none());
        assert!(!input.is_auto_request());
        assert!(input.read_captured().unwrap());
    }

    #[test]
    fn back_off_cancels_entry_waiting_for_an_ordinary_key_without_grabbing() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        press(&mut input, KeyCode::KEY_A);
        source
            .borrow_mut()
            .events
            .push(InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 2));
        assert!(!input.enter().unwrap());
        assert_eq!(source.borrow().grab_count, 0);
        assert!(source.borrow().writes.is_empty());
    }

    #[test]
    fn back_off_during_entry_repairs_input_and_releases_capture() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_repeated: true,
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        assert!(!input.enter().unwrap());
        assert!(!input.is_captured);
        assert!(input.pending.is_none());
        assert!(!source.borrow().is_grabbed);
        assert!(source.borrow().writes.contains(&(KeyCode::KEY_A, 1, true)));
        assert!(source.borrow().writes.contains(&(KeyCode::KEY_A, 0, false)));
    }

    #[test]
    fn timeout_applies_to_space_numpad_modifiers_and_the_switch_key() {
        for key in [
            KeyCode::KEY_SPACE,
            KeyCode::KEY_KP1,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTMETA,
            KeyCode::KEY_PAUSE,
        ] {
            let mut input = auto_input();
            let now = Instant::now();
            event_at(&mut input, 0, key, 1, now);
            input.update_back_off(now + HOLD_TIMEOUT);
            assert!(!input.is_backing_off);
            input.update_back_off(now + HOLD_TIMEOUT + TEST_STEP);
            assert!(input.is_backing_off, "{key:?}");
            assert!(input.pending.is_none());
        }
    }

    #[test]
    fn auto_switches_before_separator_and_can_switch_same_word_again() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_N);
        assert!(input.is_auto_request());
        let us = input.automatic_target().unwrap();
        assert_eq!(us.name, "us");
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some(".т"));
        input.set_latest_layout(Some(us));
        input.pending = None;
        press(&mut input, KeyCode::KEY_A);
        assert_eq!(input.automatic_target().unwrap().name, "ru");
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Some("/na"));
    }

    #[test]
    fn backspace_does_not_check_patterns_and_separators_cancel_pending_auto_switch() {
        for separator in [
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_TAB,
            KeyCode::KEY_ENTER,
            KeyCode::KEY_LEFT,
        ] {
            let mut input = auto_input();
            press(&mut input, KeyCode::KEY_SLASH);
            press(&mut input, KeyCode::KEY_N);
            press(&mut input, separator);
            assert!(input.automatic_target().is_none());
        }
        let mut input = auto_input();
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N, KeyCode::KEY_A] {
            press(&mut input, key);
        }
        input.pending = None;
        input.automatic_target = None;
        press(&mut input, KeyCode::KEY_BACKSPACE);
        assert!(input.pending.is_none());
        assert!(input.automatic_target().is_none());
    }

    #[test]
    fn discovery_drains_channel_and_ignores_switch_requests() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        input.pending = None;
        input.pointers.push(Pointer {
            path: PathBuf::from("pointer"),
        });
        source.borrow_mut().events.extend([
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 1),
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_PAUSE.0, 1),
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_PAUSE.0, 0),
        ]);
        source
            .borrow()
            .events
            .sender
            .as_ref()
            .unwrap()
            .send(InputEventMessage::Pointer(
                0,
                InputEvent::new(EventType::RELATIVE.0, 0, 1),
            ));
        input.wait_for_keyboard_discovery(TEST_STEP * 3).unwrap();
        assert!(input.events.available().unwrap().is_none());
        assert_eq!(input.pending, None);
        assert!(!input.is_original_switch_key_held);
        assert!(input.keyboards[0].held.contains(&KeyCode::KEY_LEFTSHIFT.0));
        assert!(!input.keyboards[0].held.contains(&KeyCode::KEY_PAUSE.0));
        source.borrow_mut().events.push(InputEvent::new(
            EventType::KEY.0,
            KeyCode::KEY_LEFTSHIFT.0,
            0,
        ));
        source
            .borrow_mut()
            .events
            .push(InputEvent::new(EventType::KEY.0, KeyCode::KEY_PAUSE.0, 1));
        input.process_available_events().unwrap();
        assert_eq!(input.pending, Some(0));
    }

    #[test]
    fn discovery_propagates_pointer_event_loss() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        input.pointers.push(Pointer {
            path: PathBuf::from("pointer"),
        });
        source
            .borrow()
            .events
            .sender
            .as_ref()
            .unwrap()
            .send(InputEventMessage::Pointer(
                0,
                InputEvent::new(
                    EventType::SYNCHRONIZATION.0,
                    SynchronizationCode::SYN_DROPPED.0,
                    0,
                ),
            ));
        assert!(
            input
                .wait_for_keyboard_discovery(TEST_STEP)
                .unwrap_err()
                .to_string()
                .contains("SYN_DROPPED")
        );
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
