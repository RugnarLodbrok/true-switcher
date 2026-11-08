use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode, raw_stream::RawDevice};

mod interception;

use interception::InterceptionState;
pub use interception::Interceptor;

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
    statistics: Option<crate::statistics::Statistics>,
    auto_switch: crate::switch_rules::AutoSwitchRules,
    layout: Option<Arc<crate::layout::Layout>>,
    observed_layout: Option<Arc<crate::layout::Layout>>,
    events: InputEvents,
    holds: HoldDetector,
    correction_result: Option<io::Result<()>>,
    is_correction_cancelled: bool,
    switch_key: KeyCode,
    is_cancel_requested: bool,
    pending: Option<usize>,
    is_original_switch_key_held: bool,
    is_backing_off: bool,
    is_auto_request: bool,
    automatic_target: Option<Arc<crate::layout::Layout>>,
}

impl Input {
    pub fn open(
        events: InputEvents,
        sender: InputEventSender,
        switch_key: KeyCode,
        statistics: crate::statistics::Statistics,
        auto_switch: crate::switch_rules::AutoSwitchRules,
    ) -> io::Result<Self> {
        let mut input = Self {
            keyboards: Vec::new(),
            pointers: Vec::new(),
            history: History::default(),
            statistics: Some(statistics),
            auto_switch,
            layout: None,
            observed_layout: None,
            events,
            holds: HoldDetector::default(),
            correction_result: None,
            is_correction_cancelled: false,
            switch_key,
            is_cancel_requested: false,
            pending: None,
            is_original_switch_key_held: false,
            is_backing_off: false,
            is_auto_request: false,
            automatic_target: None,
        };
        input.discover_devices(&sender)?;
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
            self.process_available_events(None)?;
            self.pending = None;
            self.is_auto_request = false;
            self.automatic_target = None;
            self.is_original_switch_key_held = false;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            self.wait_for_events(Some(deadline), None)?;
        }
    }

    pub fn wait_for_switch_request(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        self.process_events_until(|input| input.pending.is_some(), None)
    }

    pub fn latest_word(&self) -> Option<&crate::word::Word> {
        self.history.latest_word()
    }

    pub fn is_auto_request(&self) -> bool {
        self.is_auto_request
    }

    pub fn automatic_target(&self) -> Option<Arc<crate::layout::Layout>> {
        self.history.latest_word()?;
        self.automatic_target.clone()
    }

    pub fn previous_word(&self) -> Option<&crate::word::Word> {
        self.history.previous_word()
    }

    pub fn set_latest_layout(&mut self, layout: Option<Arc<crate::layout::Layout>>) {
        if layout.is_some() {
            self.layout = layout.clone();
            self.observed_layout = layout.clone();
        }
        self.history.set_latest_layout(layout);
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

    fn process_events_until(
        &mut self,
        is_ready: impl Fn(&Self) -> bool,
        mut state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
        self.process_available_events(state.as_deref_mut())?;
        while !is_ready(self) {
            self.wait_for_events(None, state.as_deref_mut())?;
        }
        Ok(())
    }

    fn process_available_events(
        &mut self,
        mut state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
        self.drain_events(state.as_deref_mut())?;
        self.synchronize_history(state.as_deref());
        self.update_back_off(Instant::now(), state.is_some_and(|state| state.is_captured));
        Ok(())
    }

    fn drain_events(&mut self, mut state: Option<&mut InterceptionState>) -> io::Result<()> {
        while let Some(event) = self.events.available()? {
            self.process_message(event, state.as_deref_mut())?;
        }
        Ok(())
    }

    fn synchronize_history(&mut self, state: Option<&InterceptionState>) {
        if !state.is_some_and(|state| state.is_captured || state.recording.is_some()) {
            self.synchronize_layout();
            if self.history.latest_word().is_none() {
                self.automatic_target = None;
            }
        }
    }

    fn wait_for_events(
        &mut self,
        deadline: Option<Instant>,
        mut state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
        let hold_deadline = if self.is_backing_off {
            None
        } else {
            self.holds.deadline()
        };
        let deadline = deadline.into_iter().chain(hold_deadline).min();
        if let Some(event) = self.events.wait(deadline)? {
            self.process_message(event, state.as_deref_mut())?;
        }
        self.process_available_events(state)
    }

    fn process_message(
        &mut self,
        event: InputEventMessage,
        state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
        let is_captured = state.as_ref().is_some_and(|state| state.is_captured);
        match event {
            InputEventMessage::Keyboard(index, event) => self.process(index, event, state)?,
            InputEventMessage::Pointer(index, event) => {
                self.process_pointer(index, event, is_captured)?
            }
            InputEventMessage::DeviceError(error) => return Err(error),
            InputEventMessage::FocusChanged => self.request_history_reset(is_captured),
            InputEventMessage::LayoutChanged(layout) => {
                self.accept_layout(
                    layout,
                    is_captured || state.is_some_and(|state| state.recording.is_some()),
                );
                if self.history.latest_word().is_none() {
                    self.automatic_target = None;
                }
            }
            InputEventMessage::CorrectionFinished { result, layout } => {
                self.accept_layout(layout.clone(), true);
                if result.is_ok() && !self.is_correction_cancelled {
                    self.set_latest_layout(layout);
                } else {
                    self.accept_layout(layout, false);
                    self.reset_history();
                }
                self.correction_result = Some(result);
            }
        }
        Ok(())
    }

    fn wait_for_correction(
        &mut self,
        state: Option<&mut InterceptionState>,
    ) -> io::Result<Option<io::Result<()>>> {
        if self.correction_result.is_none() {
            self.wait_for_events(None, state)?;
        }
        Ok(self.correction_result.take())
    }

    fn process(
        &mut self,
        index: usize,
        event: InputEvent,
        state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
        let now = Instant::now();
        let time = event.timestamp();
        let elapsed = SystemTime::now().duration_since(time).ok();
        let now = if time == SystemTime::UNIX_EPOCH {
            now
        } else {
            elapsed.and_then(|age| now.checked_sub(age)).unwrap_or(now)
        };
        self.process_at(index, event, now, state)
    }

    fn process_at(
        &mut self,
        index: usize,
        event: InputEvent,
        now: Instant,
        state: Option<&mut InterceptionState>,
    ) -> io::Result<()> {
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
        let is_captured = state.as_ref().is_some_and(|state| state.is_captured);
        self.update_back_off(now, is_captured);
        self.holds.update(index, key.0, value, now);
        self.track_key(index, event);
        if let Some(recording) = state.and_then(|state| state.recording.as_mut()) {
            recording.record(index, key.0, value);
        }
        if value == 2 && !is_shift(key) {
            self.start_back_off(is_captured);
        }
        if self.is_backing_off {
            if value == 0 {
                self.request_history_reset(is_captured);
            }
            self.update_back_off(now, is_captured);
            return Ok(());
        }
        self.process_history_event(index, key, value, is_captured)
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
        key: KeyCode,
        value: i32,
        is_captured: bool,
    ) -> io::Result<()> {
        let is_hard_separator = matches!(
            key,
            KeyCode::KEY_TAB | KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER
        ) && !self.has_shortcut_modifier();
        let is_pointer_click = value == 1 && is_pointer_press(key.0);
        if (self.should_reset_for_key(key, value) || is_pointer_click)
            && (!is_hard_separator || is_captured)
        {
            self.request_history_reset(is_captured);
        }
        if is_captured || self.keyboards[index].device.is_grabbed() {
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
        self.process_text_event(index, key, value, is_hard_separator);
        Ok(())
    }

    fn process_text_event(
        &mut self,
        index: usize,
        key: KeyCode,
        value: i32,
        is_hard_separator: bool,
    ) {
        if matches!(value, 1 | 2) && !self.has_shortcut_modifier() {
            if is_hard_separator {
                self.reset_history();
            } else {
                let is_shift = self.keyboards.iter().any(|keyboard| {
                    keyboard
                        .held
                        .iter()
                        .any(|&code| is_shift(KeyCode::new(code)))
                });
                self.history.process(key, is_shift, self.layout.clone());
            }
        }
        if matches!(value, 1 | 2) && key == KeyCode::KEY_BACKSPACE {
            self.automatic_target = None;
        }
        if matches!(value, 1 | 2)
            && crate::history::is_text_key(key)
            && !self.has_shortcut_modifier()
            && (self.pending.is_none() || self.is_auto_request)
        {
            self.automatic_target = self
                .history
                .latest_word()
                .and_then(|word| {
                    self.layout
                        .as_ref()
                        .and_then(|layout| self.auto_switch.target(word, layout))
                });
            if self.automatic_target.is_some() {
                self.pending = Some(index);
                self.is_auto_request = true;
                self.is_original_switch_key_held = false;
            }
        }
    }

    fn process_pointer(
        &mut self,
        index: usize,
        event: InputEvent,
        is_captured: bool,
    ) -> io::Result<()> {
        if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_DROPPED.0
        {
            self.request_history_reset(is_captured);
            return Err(io::Error::other(format!(
                "{}: SYN_DROPPED",
                self.pointers[index].path.display()
            )));
        }
        if event.event_type() == EventType::KEY
            && event.value() == 1
            && is_pointer_press(event.code())
        {
            self.request_history_reset(is_captured);
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

    fn request_history_reset(&mut self, is_captured: bool) {
        self.automatic_target = None;
        self.reset_history();
        if is_captured {
            self.is_cancel_requested = true;
            self.is_correction_cancelled = true;
        }
    }

    fn start_back_off(&mut self, is_captured: bool) {
        if self.is_backing_off {
            return;
        }
        self.is_backing_off = true;
        self.request_history_reset(is_captured);
        self.pending = None;
        self.is_auto_request = false;
        self.is_original_switch_key_held = false;
    }

    fn update_back_off(&mut self, now: Instant, is_captured: bool) {
        if self.holds.expire(now) {
            self.start_back_off(is_captured);
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
        for word in self.history.reset() {
            if let Some(statistics) = &self.statistics {
                statistics.record_word(word);
            }
        }
    }

    fn accept_layout(&mut self, layout: Option<Arc<crate::layout::Layout>>, is_deferred: bool) {
        self.layout = layout;
        if !is_deferred {
            self.synchronize_layout();
        }
    }

    fn synchronize_layout(&mut self) {
        let Some(current) = self.layout.clone() else {
            return;
        };
        if self
            .observed_layout
            .as_ref()
            .is_some_and(|old| old != &current)
        {
            self.reset_history();
        }
        self.observed_layout = Some(current);
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
        } else {
            keyboard.held.insert(key.0);
        }
        Ok(())
    }

    fn release_modifiers(&mut self) -> io::Result<()> {
        let mut error = None;
        for index in 0..self.keyboards.len() {
            for key in MODIFIERS {
                if self.keyboards[index].held.contains(&key.0)
                    && let Err(err) = self.write_source(index, key, 0)
                {
                    log::error!("RELEASE modifier keyboard={index} {key:?}: {err}");
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn release_locks(&mut self) -> io::Result<()> {
        let mut error = None;
        for index in 0..self.keyboards.len() {
            for key in LOCK_KEYS {
                if let Err(err) = self.write_source(index, key, 0) {
                    log::error!("RELEASE lock keyboard={index} {key:?}: {err}");
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
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
                    log::error!("UNGRAB {}: {err}", keyboard.path.display());
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn snapshot(&mut self, mut state: Option<&mut InterceptionState>) -> io::Result<()> {
        for (index, keyboard) in self.keyboards.iter_mut().enumerate() {
            keyboard.held = keyboard.device.keys()?;
            self.holds.snapshot(index, &keyboard.held, Instant::now());
            if let Some(recording) = state
                .as_deref_mut()
                .and_then(|state| state.recording.as_mut())
            {
                recording
                    .presses
                    .extend(keyboard.held.iter().map(|&code| (index, code)));
            }
        }
        Ok(())
    }
}

fn is_shift(key: KeyCode) -> bool {
    matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT)
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
pub(crate) mod tests {
    use super::*;
    use crate::hold::HOLD_TIMEOUT;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
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
        ungrab_count: usize,
        ungrab_failures: usize,
        write_failures: BTreeMap<(u16, i32), usize>,
        is_read_error_after_ungrab: bool,
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
            if let Some(failures) = keyboard.write_failures.get_mut(&(code, value))
                && *failures > 0
            {
                *failures -= 1;
                return Err(io::Error::other("write failed"));
            }
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
            let mut keyboard = self.0.borrow_mut();
            keyboard.ungrab_count += 1;
            if keyboard.ungrab_failures > 0 {
                keyboard.ungrab_failures -= 1;
                return Err(io::Error::other("ungrab failed"));
            }
            keyboard.is_grabbed = false;
            if keyboard.is_read_error_after_ungrab {
                keyboard.is_read_error_after_ungrab = false;
                keyboard
                    .events
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(InputEventMessage::DeviceError(io::Error::other(
                        "read after ungrab failed",
                    )));
            }
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
            statistics: None,
            auto_switch: Default::default(),
            layout: None,
            observed_layout: None,
            events,
            holds,
            correction_result: None,
            is_correction_cancelled: false,
            switch_key: KeyCode::KEY_PAUSE,
            is_cancel_requested: false,
            pending: Some(0),
            is_original_switch_key_held: true,
            is_backing_off: false,
            is_auto_request: false,
            automatic_target: None,
        }
    }

    pub(crate) fn statistics_input(statistics: crate::statistics::Statistics) -> Input {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[source]);
        input.pending = None;
        input.is_original_switch_key_held = false;
        input.statistics = Some(statistics);
        input.accept_layout(Some(layout("us")), false);
        input
    }

    pub(crate) fn statistics_event(input: &mut Input, key: KeyCode, value: i32) {
        input
            .process(0, InputEvent::new(EventType::KEY.0, key.0, value), None)
            .unwrap();
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
            Ok(Arc::new(crate::layout::Layout::new(name, "", "", "")))
        })
        .unwrap();
        input.auto_switch = switch;
        input.accept_layout(
            Some(Arc::new(crate::layout::Layout::new("ru", "", "", ""))),
            false,
        );
        input
    }

    fn press(input: &mut Input, key: KeyCode) {
        input
            .process(0, InputEvent::new(EventType::KEY.0, key.0, 1), None)
            .unwrap();
    }

    fn event_at(input: &mut Input, source: usize, key: KeyCode, value: i32, now: Instant) {
        input
            .process_at(
                source,
                InputEvent::new(EventType::KEY.0, key.0, value),
                now,
                None,
            )
            .unwrap();
    }

    fn layout(name: &str) -> Arc<crate::layout::Layout> {
        Arc::new(crate::layout::Layout::new(name, "", "", ""))
    }

    fn channel_input() -> (Input, InputEventSender) {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let input = simulated_input(std::slice::from_ref(&source));
        let sender = source.borrow().events.sender.as_ref().unwrap().clone();
        (input, sender)
    }

    #[test]
    fn shift_on_other_keyboard_survives_release_and_history_reset() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first, second]);
        input.pending = None;
        let now = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_LEFTSHIFT, 1, now);
        event_at(&mut input, 1, KeyCode::KEY_RIGHTSHIFT, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_LEFTSHIFT, 0, now);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.latest_word().unwrap().entries[0].stroke.shift);
        input.reset_history();
        press(&mut input, KeyCode::KEY_B);
        event_at(&mut input, 1, KeyCode::KEY_RIGHTSHIFT, 0, now);
        press(&mut input, KeyCode::KEY_C);
        let entries = &input.latest_word().unwrap().entries;
        assert!(entries[0].stroke.shift);
        assert!(!entries[1].stroke.shift);
    }

    #[test]
    fn initial_held_shift_is_used_and_capture_tracks_release() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        source.borrow_mut().held.insert(KeyCode::KEY_LEFTSHIFT.0);
        let mut input = simulated_input(&[source]);
        input.pending = None;
        press(&mut input, KeyCode::KEY_A);
        assert!(input.latest_word().unwrap().entries[0].stroke.shift);
        let mut state = InterceptionState::default();
        state.is_captured = true;
        input
            .process(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 0),
                Some(&mut state),
            )
            .unwrap();
        press(&mut input, KeyCode::KEY_B);
        assert!(!input.latest_word().unwrap().entries[1].stroke.shift);
    }

    #[test]
    fn unknown_layout_keeps_history_and_last_known_layout_for_comparison() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_A);
        input.accept_layout(None, false);
        press(&mut input, KeyCode::KEY_B);
        assert_eq!(input.latest_word().unwrap().entries.len(), 2);
        assert!(input.latest_word().unwrap().entries[1].layout.is_none());
        input.accept_layout(Some(layout("us")), false);
        assert!(input.latest_word().is_none());
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
        input.process_available_events(None).unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("a"));
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        input.process_available_events(None).unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn completion_replaces_deferred_layout_and_late_duplicate_preserves_corrected_word() {
        let (mut input, sender) = channel_input();
        input.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        interception.read_captured().unwrap();
        assert_eq!(
            interception
                .input()
                .latest_word()
                .unwrap()
                .text()
                .as_deref(),
            Ok("a")
        );
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutChanged(Some(layout("ru"))));
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        assert_eq!(
            interception
                .input()
                .latest_word()
                .unwrap()
                .text()
                .as_deref(),
            Ok("ф")
        );
        interception.finish().unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("ф"));
    }

    #[test]
    fn layout_change_after_completion_is_reconciled_when_capture_ends() {
        let (mut input, sender) = channel_input();
        input.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutChanged(Some(layout("us"))));
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        interception.finish().unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn consumed_cancellation_still_prevents_applying_successful_correction() {
        let (mut input, sender) = channel_input();
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        sender.send(InputEventMessage::FocusChanged);
        assert!(interception.read_captured().unwrap());
        assert!(!interception.take_cancel_request());
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        assert!(interception.input().latest_word().is_none());
        interception.finish().unwrap();
        press(&mut input, KeyCode::KEY_B);
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("и"));
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
                .wait_for_correction(None)
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
            input.wait_for_events(None, None).unwrap_err().to_string(),
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
        input.wait_for_events(None, None).unwrap();
        assert!(input.is_backing_off);
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn correction_wait_preserves_capture_context_when_hold_timer_expires() {
        let (mut input, _sender) = channel_input();
        let mut state = InterceptionState::default();
        state.is_captured = true;
        input
            .process_at(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1),
                Instant::now() - HOLD_TIMEOUT - TEST_STEP,
                Some(&mut state),
            )
            .unwrap();
        assert!(!input.is_backing_off);
        assert!(
            input
                .wait_for_correction(Some(&mut state))
                .unwrap()
                .is_none()
        );
        assert!(input.is_backing_off);
        assert!(input.take_cancel_request());
        assert!(input.is_correction_cancelled);
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
            assert_eq!(input.latest_word().unwrap().entries.len(), 1);
        }
    }

    #[test]
    fn hold_timeout_runs_without_events_and_is_individual_for_each_key() {
        let mut input = auto_input();
        let now = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_A, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_B, 1, now + HOLD_TIMEOUT / 2);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, now + HOLD_TIMEOUT / 2);
        input.update_back_off(now + HOLD_TIMEOUT, false);
        assert!(!input.is_backing_off);
        input.holds.update(0, KeyCode::KEY_B.0, 0, now);
        input.holds.update(
            0,
            KeyCode::KEY_B.0,
            1,
            Instant::now() - HOLD_TIMEOUT - TEST_STEP,
        );
        input.process_available_events(None).unwrap();
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
        input.update_back_off(later, false);
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
        assert!(!input.latest_word().unwrap().entries[0].stroke.shift);
    }

    #[test]
    fn back_off_cancels_automatic_and_captured_corrections() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        press(&mut input, KeyCode::KEY_N);
        assert!(input.automatic_target().is_some());
        let mut state = InterceptionState::default();
        state.is_captured = true;
        input
            .process(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_N.0, 2),
                Some(&mut state),
            )
            .unwrap();
        assert!(input.pending.is_none());
        assert!(input.automatic_target().is_none());
        assert!(!input.is_auto_request());
        assert!(input.take_cancel_request());
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
        assert!(Interceptor::enter(&mut input).unwrap().is_none());
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
        assert!(Interceptor::enter(&mut input).unwrap().is_none());
        assert!(input.pending.is_none());
        assert!(!source.borrow().is_grabbed);
        assert_eq!(source.borrow().ungrab_count, 1);
        assert_eq!(
            source
                .borrow()
                .writes
                .iter()
                .filter(|(key, _, _)| LOCK_KEYS.contains(key))
                .count(),
            LOCK_KEYS.len() * 2
        );
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
            input.update_back_off(now + HOLD_TIMEOUT, false);
            assert!(!input.is_backing_off);
            input.update_back_off(now + HOLD_TIMEOUT + TEST_STEP, false);
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
        assert_eq!(us.name(), "us");
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok(".т"));
        input.set_latest_layout(Some(us));
        input.pending = None;
        press(&mut input, KeyCode::KEY_A);
        assert_eq!(input.automatic_target().unwrap().name(), "ru");
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("/na"));
    }

    #[test]
    fn spaces_preserve_auto_switch_while_waiting_for_key_release() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        statistics_event(&mut input, KeyCode::KEY_SLASH, 0);
        statistics_event(&mut input, KeyCode::KEY_N, 1);
        for spaces in 1..=3 {
            press(&mut input, KeyCode::KEY_SPACE);
            statistics_event(&mut input, KeyCode::KEY_SPACE, 0);
            assert!(input.is_ordinary_key_held());
            assert_eq!(input.automatic_target().unwrap().name(), "us");
            assert_eq!(input.latest_word().unwrap().trailing_spaces, spaces);
        }
        statistics_event(&mut input, KeyCode::KEY_N, 0);
        assert!(!input.is_ordinary_key_held());
        assert!(input.is_auto_request());
        assert!(input.pending.is_some());
        assert_eq!(input.automatic_target().unwrap().name(), "us");
        press(&mut input, KeyCode::KEY_A);
        assert!(input.automatic_target().is_none());
    }

    #[test]
    fn backspace_does_not_check_patterns_and_separators_cancel_pending_auto_switch() {
        for separator in [
            KeyCode::KEY_BACKSPACE,
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
        input.process_available_events(None).unwrap();
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
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
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
        drop(keyboard);
        interception.finish().unwrap();
    }

    #[test]
    fn capture_exposes_history_updated_during_entry() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        source
            .borrow_mut()
            .events
            .extend([1, 0].map(|value| InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, value)));
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        assert_eq!(interception.input().latest_word().unwrap().entries.len(), 1);
        interception.finish().unwrap();
    }

    #[test]
    fn exit_resets_reasserted_modifiers_on_their_source() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        second.borrow_mut().held.insert(KeyCode::KEY_RIGHTCTRL.0);
        second.borrow_mut().events.push(InputEvent::new(
            EventType::KEY.0,
            KeyCode::KEY_RIGHTCTRL.0,
            2,
        ));
        interception.finish().unwrap();
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
        assert!(Interceptor::enter(&mut input).is_err());
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
        assert!(Interceptor::enter(&mut input).is_err());
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
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        source.borrow_mut().events.push(InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_DROPPED.0,
            0,
        ));
        assert!(
            interception
                .read_captured()
                .unwrap_err()
                .to_string()
                .contains("SYN_DROPPED")
        );
        drop(interception);
        assert!(!source.borrow().is_grabbed);
        assert!(
            input
                .keyboards
                .iter()
                .all(|keyboard| !keyboard.device.is_grabbed())
        );
    }

    #[test]
    fn dropping_capture_releases_modifiers_without_waiting_for_ordinary_keys() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        {
            let mut keyboard = source.borrow_mut();
            for key in [KeyCode::KEY_A, KeyCode::KEY_LEFTSHIFT] {
                keyboard.held.insert(key.0);
                keyboard
                    .events
                    .push(InputEvent::new(EventType::KEY.0, key.0, 1));
            }
        }
        interception.read_captured().unwrap();
        assert!(interception.input().latest_word().is_none());
        drop(interception);
        assert!(!source.borrow().is_grabbed);
        assert!(source.borrow().held.contains(&KeyCode::KEY_A.0));
        assert!(
            source
                .borrow()
                .writes
                .contains(&(KeyCode::KEY_LEFTSHIFT, 0, true))
        );
        press(&mut input, KeyCode::KEY_B);
        assert_eq!(input.latest_word().unwrap().entries.len(), 1);
    }

    #[test]
    fn successful_finish_and_input_drop_do_not_repeat_cleanup() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        source.borrow_mut().writes.clear();
        interception.finish().unwrap();
        assert_eq!(source.borrow().ungrab_count, 1);
        assert_eq!(source.borrow().writes.len(), LOCK_KEYS.len());
        let writes = source.borrow().writes.clone();
        drop(input);
        assert_eq!(source.borrow().writes, writes);
        assert_eq!(source.borrow().ungrab_count, 1);
    }

    #[test]
    fn failed_finish_returns_original_error_and_continues_cleanup_after_write_errors() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        for source in [&first, &second] {
            let mut keyboard = source.borrow_mut();
            keyboard.held.insert(KeyCode::KEY_LEFTSHIFT.0);
            keyboard.events.push(InputEvent::new(
                EventType::KEY.0,
                KeyCode::KEY_LEFTSHIFT.0,
                1,
            ));
        }
        interception.read_captured().unwrap();
        first
            .borrow_mut()
            .write_failures
            .insert((KeyCode::KEY_LEFTSHIFT.0, 0), 1);
        first
            .borrow_mut()
            .write_failures
            .insert((KeyCode::KEY_CAPSLOCK.0, 0), 1);
        let sender = first.borrow().events.sender.as_ref().unwrap().clone();
        sender.send(InputEventMessage::DeviceError(io::Error::other(
            "finish read failed",
        )));
        assert_eq!(
            interception.finish().unwrap_err().to_string(),
            "finish read failed"
        );
        assert!(!first.borrow().is_grabbed);
        assert!(!second.borrow().is_grabbed);
        assert!(!second.borrow().held.contains(&KeyCode::KEY_LEFTSHIFT.0));
        assert!(
            first
                .borrow()
                .writes
                .contains(&(KeyCode::KEY_SCROLLLOCK, 0, false))
        );
        assert!(
            second
                .borrow()
                .writes
                .contains(&(KeyCode::KEY_CAPSLOCK, 0, false))
        );
        assert!(
            input
                .keyboards
                .iter()
                .all(|keyboard| !keyboard.device.is_grabbed())
        );
    }

    #[test]
    fn failed_finish_ungrabs_only_remaining_devices() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let second = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        second.borrow_mut().ungrab_failures = 1;
        assert_eq!(
            interception.finish().unwrap_err().to_string(),
            "ungrab failed"
        );
        assert_eq!(first.borrow().ungrab_count, 1);
        assert_eq!(second.borrow().ungrab_count, 2);
        assert!(!first.borrow().is_grabbed);
        assert!(!second.borrow().is_grabbed);
    }

    #[test]
    fn failed_finish_retries_incomplete_lock_release() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        source.borrow_mut().writes.clear();
        source
            .borrow_mut()
            .write_failures
            .insert((KeyCode::KEY_CAPSLOCK.0, 0), 1);
        assert_eq!(
            interception.finish().unwrap_err().to_string(),
            "write failed"
        );
        assert_eq!(source.borrow().ungrab_count, 1);
        assert_eq!(source.borrow().writes.len(), LOCK_KEYS.len() * 2);
        assert!(
            source
                .borrow()
                .writes
                .iter()
                .all(|(_, value, is_grabbed)| *value == 0 && !is_grabbed)
        );
    }

    #[test]
    fn failed_finish_read_does_not_repeat_completed_cleanup() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        source.borrow_mut().writes.clear();
        source.borrow_mut().is_read_error_after_ungrab = true;
        assert_eq!(
            interception.finish().unwrap_err().to_string(),
            "read after ungrab failed"
        );
        assert_eq!(source.borrow().ungrab_count, 1);
        assert_eq!(source.borrow().writes.len(), LOCK_KEYS.len());
    }

    #[test]
    fn failed_rollback_resumes_without_repeating_hidden_presses() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            ungrab_failures: 1,
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        assert!(Interceptor::enter(&mut input).is_err());
        let source = source.borrow();
        let repairs: Vec<_> = source
            .writes
            .iter()
            .filter(|(key, _, _)| *key == KeyCode::KEY_A)
            .copied()
            .collect();
        assert_eq!(
            repairs,
            [(KeyCode::KEY_A, 1, true), (KeyCode::KEY_A, 0, false)]
        );
        assert!(!source.is_grabbed);
    }

    #[test]
    fn cleanup_releases_repaired_keys_after_retrying_a_partial_grab_ungrab() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            ungrab_failures: 1,
            ..Default::default()
        }));
        let second = Rc::new(RefCell::new(SimulatedKeyboard {
            is_grab_failed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(&[first.clone(), second]);
        assert!(Interceptor::enter(&mut input).is_err());
        let source = first.borrow();
        let repairs: Vec<_> = source
            .writes
            .iter()
            .filter(|(key, _, _)| *key == KeyCode::KEY_A)
            .copied()
            .collect();
        assert_eq!(
            repairs,
            [(KeyCode::KEY_A, 1, true), (KeyCode::KEY_A, 0, false)]
        );
        assert!(!source.is_grabbed);
        assert!(!source.held.contains(&KeyCode::KEY_A.0));
    }

    #[test]
    fn failed_rollback_does_not_repeat_successful_repairs_on_other_devices() {
        let first = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            ..Default::default()
        }));
        let second = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            is_repair_release_failed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        assert!(Interceptor::enter(&mut input).is_err());
        assert_eq!(
            first
                .borrow()
                .writes
                .iter()
                .filter(|(key, _, _)| *key == KeyCode::KEY_A)
                .count(),
            2
        );
        assert_eq!(
            second
                .borrow()
                .writes
                .iter()
                .filter(|(key, _, _)| *key == KeyCode::KEY_A)
                .count(),
            3
        );
        assert!(!first.borrow().is_grabbed);
        assert!(!second.borrow().is_grabbed);
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
