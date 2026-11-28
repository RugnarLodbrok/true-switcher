use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode, raw_stream::RawDevice};

mod correction;
mod device_watch;
mod devices;
mod interception;
mod keyboard;
mod pending;
mod pointer;
use pending::PendingSwitch;

use correction::CorrectionState;
use interception::InterceptionState;
#[cfg(test)]
use interception::Interceptor;
pub use interception::{CorrectionInterceptor, enter_interceptor};
use keyboard::KeyboardInput;
#[cfg(test)]
use keyboard::Source;

use crate::config::{Configuration, RuntimeSetting, RuntimeSettings};
use crate::history::{History, HistoryEvent};
use crate::input_events::{InputEventMessage, InputEventSender, InputEvents};
use crate::key_codes::is_text_key;
use crate::layout::Layout;
use crate::lifecycle::{Lifecycle, ProcessingError, ProcessingResult, ShutdownReason};
use crate::switch_rules::{AutoSwitch, AutoSwitchDecision, AutoSwitchKind};
use crate::word::ChainMember;

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
const CHAIN_ACROSS_STRONG_PUNCTUATION: bool = true;

trait InputEventExt {
    fn instant(&self) -> Instant;
}

impl InputEventExt for InputEvent {
    fn instant(&self) -> Instant {
        let now = Instant::now();
        let time = self.timestamp();
        let elapsed = SystemTime::now().duration_since(time).ok();
        if time == SystemTime::UNIX_EPOCH {
            now
        } else {
            elapsed.and_then(|age| now.checked_sub(age)).unwrap_or(now)
        }
    }
}

#[derive(Debug)]
pub enum InputRequest {
    Switch,
    Shutdown(ShutdownReason),
}

#[derive(Debug)]
pub struct SwitchRequest {
    pub previous: Option<crate::word::Word>,
    pub words: Vec<crate::word::Word>,
    pub target: Option<Arc<crate::layout::Layout>>,
    pub mode: crate::switcher::SwitchMode,
    pub is_learning: bool,
    pub triggered_at: Option<Instant>,
}

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

pub struct Input {
    keyboard_input: KeyboardInput,
    lifecycle: Lifecycle,
    history: History,
    statistics: Option<crate::statistics::Statistics>,
    auto_switch: AutoSwitch,
    settings: RuntimeSettings,
    layout: Option<Arc<crate::layout::Layout>>,
    observed_layout: Option<Arc<crate::layout::Layout>>,
    events: InputEvents,
    correction: CorrectionState,
    switch_key: KeyCode,
    pending: PendingSwitch,
    is_backing_off: bool,
    mouse_motion_pause: Duration,
    last_mouse_motion: Option<Instant>,
}

impl Input {
    pub fn open(
        events: InputEvents,
        sender: InputEventSender,
        config: &Configuration,
        statistics: crate::statistics::Statistics,
        auto_switch: AutoSwitch,
    ) -> io::Result<Self> {
        let mut input = Self {
            keyboard_input: KeyboardInput::default(),
            lifecycle: Lifecycle::Running,
            history: History::default(),
            statistics: Some(statistics),
            auto_switch,
            settings: config.into(),
            layout: None,
            observed_layout: None,
            events,
            correction: CorrectionState::default(),
            switch_key: config.switch_key,
            pending: PendingSwitch::default(),
            is_backing_off: false,
            mouse_motion_pause: Duration::from_millis(config.mouse_motion_pause_ms),
            last_mouse_motion: None,
        };
        let monitor = devices::Monitor::new()?;
        input.discover_devices(&sender, &monitor)?;
        pointer::start(sender.clone());
        monitor.start(sender)?;
        Ok(input)
    }

    fn discover_devices(
        &mut self,
        sender: &InputEventSender,
        monitor: &devices::Monitor,
    ) -> io::Result<()> {
        for path in monitor.keyboards.keys() {
            if let Err(error) = self.keyboard_input.open(path.clone(), sender) {
                if is_disconnection(&error) {
                    self.devices_changed(Instant::now());
                } else if error.kind() == io::ErrorKind::PermissionDenied {
                    log::warn!("Cannot open keyboard {}: {error}", path.display());
                } else {
                    return Err(error);
                }
            }
        }
        if self.keyboard_input.len() == 0 {
            log::info!("No accessible keyboards; waiting for a keyboard to appear");
        }
        Ok(())
    }

    pub fn wait_for_keyboard_discovery(&mut self, duration: Duration) -> ProcessingResult<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.process_available_events(None)?;
            self.pending.cancel();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            self.wait_for_events(Some(deadline), None)?;
        }
    }

    pub fn next_request(&mut self) -> io::Result<InputRequest> {
        loop {
            match self.wait_for_resume() {
                // pause after devide chaged
                Ok(Some(reason)) => return Ok(InputRequest::Shutdown(reason)),
                Ok(None) => {}
                Err(ProcessingError::Interrupted) => continue,
                Err(ProcessingError::Io(error)) => return Err(error),
            }
            match self.wait_for_switch_request() {
                Ok(true) => return Ok(InputRequest::Switch),
                Ok(false) => {}
                Err(ProcessingError::Interrupted) => continue,
                Err(ProcessingError::Io(error)) => return Err(error),
            }
        }
    }

    fn switch_request(&self) -> Option<SwitchRequest> {
        use crate::switcher::SwitchMode;
        if !self.has_valid_pending_request() {
            return None;
        }
        let (mode, target, is_learning, triggered_at) = match &self.pending {
            PendingSwitch::Auto {
                kind, triggered_at, ..
            } => {
                let mode = match kind {
                    AutoSwitchKind::Regex => SwitchMode::Pattern,
                    AutoSwitchKind::Dictionary => SwitchMode::Auto,
                };
                (
                    mode,
                    Some(self.automatic_target()?),
                    false,
                    Some(*triggered_at),
                )
            }
            PendingSwitch::Manual { is_learning, .. } => {
                (SwitchMode::Manual, None, *is_learning, None)
            }
            PendingSwitch::Idle => return None,
        };
        let word_count = self.correction_word_count();
        Some(SwitchRequest {
            previous: self.history.word_before_latest(word_count).cloned(),
            words: self.history.latest_words(word_count).to_vec(),
            target,
            mode,
            is_learning,
            triggered_at,
        })
    }

    fn correction_word_count(&self) -> usize {
        let PendingSwitch::Auto {
            kind: AutoSwitchKind::Dictionary,
            target,
            ..
        } = &self.pending
        else {
            return 1;
        };
        let Some(source) = self.layout.as_deref() else {
            return 1;
        };
        1 + self.preceding_chain_members(source, target).count()
    }

    fn preceding_chain_members<'a>(
        &'a self,
        source: &'a Layout,
        target: &'a Layout,
    ) -> impl Iterator<Item = ChainMember> + 'a {
        self.history.preceding_words().map_while(move |word| {
            let member = word.chain_member?;
            let has_chain_boundary = word.trailing_spaces > 0
                || (CHAIN_ACROSS_STRONG_PUNCTUATION && word.trailing_punctuation > 0);
            let is_eligible = has_chain_boundary
                && word
                    .entries
                    .iter()
                    .all(|entry| entry.layout.as_deref() == Some(source))
                && !word.has_dead_key
                && !word.has_dead_key_in(source)
                && !word.has_dead_key_in(target);
            is_eligible.then_some(member)
        })
    }

    fn has_valid_pending_request(&self) -> bool {
        self.pending.is_some()
            && !self.latest_word().is_some_and(|word| word.has_dead_key)
            && (!self.pending.is_auto() || self.latest_word().is_some())
    }

    fn wait_for_switch_request(&mut self) -> ProcessingResult<bool> {
        self.lifecycle.check()?;
        if self.pending.is_none() {
            self.process_events_until(|input| input.pending.is_some(), None)?;
        }
        let is_ready = self.has_valid_pending_request();
        if !is_ready {
            self.pending.cancel();
        }
        Ok(is_ready)
    }

    pub fn latest_word(&self) -> Option<&crate::word::Word> {
        self.history.latest_word()
    }

    #[cfg(test)]
    fn is_auto_request(&self) -> bool {
        self.pending.is_auto()
    }

    pub fn automatic_target(&self) -> Option<Arc<crate::layout::Layout>> {
        self.history.latest_word()?;
        self.pending.target()
    }

    pub(crate) fn runtime_settings(&self) -> RuntimeSettings {
        self.settings
    }

    fn set_correction_layout(&mut self, count: usize, layout: Option<Arc<crate::layout::Layout>>) {
        if layout.is_some() {
            self.layout = layout.clone();
            self.observed_layout = layout.clone();
        }
        self.history.set_layout_for_latest_words(count, layout);
    }

    #[cfg(test)]
    pub(crate) fn set_latest_layout(&mut self, layout: Option<Arc<crate::layout::Layout>>) {
        self.set_correction_layout(1, layout);
    }

    pub fn begin_correction(&mut self, count: usize, layout: Option<Arc<crate::layout::Layout>>) {
        self.set_correction_layout(count, layout);
        self.history.block_auto_switch(count);
        self.pending.cancel_auto();
    }

    pub(crate) fn remember_word(&mut self, layout: &str, word: &str) -> io::Result<()> {
        self.auto_switch
            .dictionaries
            .learned
            .increment(layout, word)
    }

    fn is_ordinary_key_held(&self) -> bool {
        let Some(switch_source) = self.pending.source() else {
            return false;
        };
        self.keyboard_input.held_keys().any(|(index, code)| {
            !MODIFIERS.contains(&KeyCode::new(code))
                && !(self.pending.is_original_key_held()
                    && index == switch_source
                    && code == self.switch_key.0)
        })
    }

    fn process_events_until(
        &mut self,
        is_ready: impl Fn(&Self) -> bool,
        mut state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<()> {
        self.process_available_events(state.as_deref_mut())?;
        while !is_ready(self) {
            self.wait_for_events(None, state.as_deref_mut())?;
        }
        Ok(())
    }

    fn process_available_events(
        &mut self,
        mut state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<()> {
        self.drain_events(state.as_deref_mut())?;
        self.synchronize_history(state.as_deref());
        self.update_back_off(Instant::now());
        self.lifecycle.check()
    }

    fn drain_events(&mut self, mut state: Option<&mut InterceptionState>) -> ProcessingResult<()> {
        while let Some(event) = self.events.available()? {
            self.process_message(event, state.as_deref_mut())?;
            self.lifecycle.check()?;
        }
        Ok(())
    }

    fn process_cleanup_events(
        &mut self,
        mut state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<()> {
        let mut error = None;
        while let Some(event) = self.events.available()? {
            if let Err(err) = self.process_message(event, state.as_deref_mut()) {
                log::error!("CLEANUP input event: {err}");
                error.get_or_insert(err);
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn synchronize_history(&mut self, state: Option<&InterceptionState>) {
        if !state.is_some_and(|state| state.is_captured || state.recording.is_some()) {
            self.synchronize_layout();
            if self.history.latest_word().is_none() {
                self.pending.cancel_auto();
            }
        }
    }

    fn wait_for_events(
        &mut self,
        deadline: Option<Instant>,
        mut state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<()> {
        let hold_deadline = if self.is_backing_off {
            None
        } else {
            self.keyboard_input.hold_deadline()
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
    ) -> ProcessingResult<()> {
        let is_captured = state.as_ref().is_some_and(|state| state.is_captured);
        match event {
            InputEventMessage::Keyboard(index, event) => {
                self.process_at(index, event, event.instant(), state)?
            }
            InputEventMessage::DeviceError(error) => return Err(error.into()),
            InputEventMessage::DevicesChanged(at) => self.devices_changed(at),
            InputEventMessage::DeviceDisconnected(index, at) => {
                log::info!(
                    "Keyboard disconnected: {}",
                    self.keyboard_input.path(index).display()
                );
                if self.lifecycle == Lifecycle::Running {
                    self.devices_changed(at);
                }
            }
            InputEventMessage::Shutdown(reason) => {
                self.lifecycle.shutdown(reason);
                self.cancel_processing();
            }
            InputEventMessage::FocusChanged
            | InputEventMessage::PointerPressed
            | InputEventMessage::PointerEventsLost => self.request_history_reset(),
            InputEventMessage::MouseMoved(at) => {
                if !self.mouse_motion_pause.is_zero() {
                    self.last_mouse_motion = self.last_mouse_motion.max(Some(at));
                    self.pending.cancel_auto();
                }
            }
            InputEventMessage::LayoutsChanged { current, active } => {
                self.auto_switch.update_layouts(active);
                self.accept_layout(
                    current,
                    is_captured || state.is_some_and(|state| state.recording.is_some()),
                );
                if self.history.latest_word().is_none() {
                    self.pending.cancel_auto();
                }
            }
            InputEventMessage::ToggleSetting(setting, reply) => {
                self.toggle_setting(setting);
                let _ = reply.send(self.settings);
            }
            InputEventMessage::CorrectionFinished { result, layout } => {
                self.accept_layout(layout.clone(), true);
                if result.is_err() || self.correction.is_cancelled() {
                    self.accept_layout(layout, false);
                    self.reset_history();
                }
                self.correction.store_result(result);
            }
        }
        Ok(())
    }

    fn toggle_setting(&mut self, setting: RuntimeSetting) {
        if self.settings.toggle(setting) {
            return;
        }
        let kind = match setting {
            RuntimeSetting::AutoSwitch => AutoSwitchKind::Dictionary,
            RuntimeSetting::PatternSwitch => AutoSwitchKind::Regex,
            RuntimeSetting::ManualSwitchSound | RuntimeSetting::AutoSwitchSound => return,
        };
        self.pending.cancel_auto_kind(kind);
    }

    pub fn devices_changed(&mut self, at: Instant) {
        if self.lifecycle == Lifecycle::Running {
            log::info!("Keyboard devices changed; cancelling input processing before restart");
        }
        self.lifecycle.devices_changed(at);
        self.cancel_processing();
    }

    fn cancel_processing(&mut self) {
        self.reset_history();
        self.pending.cancel();
        self.correction.request_cancel();
    }

    fn wait_for_resume(&mut self) -> ProcessingResult<Option<ShutdownReason>> {
        if self.lifecycle != Lifecycle::Running
            && let Err(error) = self.process_cleanup_events(None)
        {
            log::error!("Lifecycle event cleanup: {error}");
        }
        if let Lifecycle::PreparingForRestart(deadline) = self.lifecycle {
            log::info!("Waiting for keyboard devices to settle before restart");
            self.lifecycle = Lifecycle::WaitingForRestart(deadline);
        }
        loop {
            let deadline = match self.lifecycle {
                Lifecycle::Running => return Ok(None),
                Lifecycle::Shutdown(_) => {
                    if let Err(error) = self.process_cleanup_events(None) {
                        log::error!("Shutdown event cleanup: {error}");
                    }
                    let Lifecycle::Shutdown(reason) = self.lifecycle else {
                        unreachable!()
                    };
                    return Ok(Some(reason));
                }
                Lifecycle::WaitingForRestart(deadline) => deadline,
                Lifecycle::PreparingForRestart(_) => unreachable!(),
            };
            if Instant::now() >= deadline {
                self.lifecycle.shutdown(ShutdownReason::Restart);
                continue;
            }
            match self.events.wait(Some(deadline))? {
                Some(event) => self.process_message(event, None)?,
                None => self.lifecycle.shutdown(ShutdownReason::Restart),
            }
        }
    }

    fn wait_for_correction(
        &mut self,
        state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<Option<io::Result<()>>> {
        if !self.correction.has_result() {
            self.wait_for_events(None, state)?;
        }
        Ok(self.correction.take_result())
    }

    fn process_at(
        &mut self,
        index: usize,
        event: InputEvent,
        now: Instant,
        state: Option<&mut InterceptionState>,
    ) -> ProcessingResult<()> {
        if self.lifecycle != Lifecycle::Running && state.is_none() {
            return Ok(());
        }
        if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_DROPPED.0
        {
            return Err(io::Error::other(format!(
                "{}: SYN_DROPPED",
                self.keyboard_input.path(index).display()
            ))
            .into());
        }
        if event.event_type() != EventType::KEY {
            return Ok(());
        }
        let key = KeyCode::new(event.code());
        let value = event.value();
        let is_captured = state.as_ref().is_some_and(|state| state.is_captured);
        self.update_back_off(now);
        let is_hold = self.keyboard_input.update(index, event, now);
        if let Some(recording) = state.and_then(|state| state.recording.as_mut()) {
            recording.record(index, key.0, value);
        }
        if self.lifecycle != Lifecycle::Running {
            return Ok(());
        }
        if is_hold {
            self.start_back_off();
        }
        if self.is_backing_off {
            if value == 0 {
                self.request_history_reset();
            }
            self.update_back_off(now);
            return Ok(());
        }
        self.process_history_event(index, key, value, is_captured, now)
    }

    fn process_history_event(
        &mut self,
        index: usize,
        key: KeyCode,
        value: i32,
        is_captured: bool,
        now: Instant,
    ) -> ProcessingResult<()> {
        let is_hard_separator = matches!(
            key,
            KeyCode::KEY_TAB | KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER
        ) && !self.keyboard_input.has_shortcut_modifier();
        if self.should_reset_for_key(key, value)
            && (!is_hard_separator || is_captured || self.correction.is_active())
        {
            self.request_history_reset();
        }
        if is_captured || self.keyboard_input.is_grabbed(index) {
            return Ok(());
        }
        if key == self.switch_key {
            if value == 0 && self.pending.source() == Some(index) {
                self.pending.release_original_key();
            }
            if value == 1 {
                self.request_manual_switch(index);
            }
            return Ok(());
        }
        self.process_text_event(index, key, value, is_hard_separator, now);
        Ok(())
    }

    fn process_text_event(
        &mut self,
        index: usize, // keyboard device index
        key: KeyCode,
        value: i32, // 0 = release, 1 = press, 2 = repeated press while holding
        is_hard_separator: bool,
        now: Instant,
    ) {
        if !matches!(value, 1 | 2) {
            return;
        }

        if key == KeyCode::KEY_BACKSPACE {
            self.pending.cancel_auto();
        }
        if self.keyboard_input.has_shortcut_modifier() {
            return;
        }
        if is_hard_separator {
            self.reset_history();
            return;
        }

        let is_shift = self
            .keyboard_input
            .held_keys()
            .any(|(_, code)| is_shift(KeyCode::new(code)));
        let event = self.history.process(
            key,
            is_shift,
            self.layout.clone(),
            &self.auto_switch.punctuation,
        );
        let Some(word) = self.history.latest_word() else {
            return;
        };
        let is_word_end = matches!(event, HistoryEvent::WordEnd);
        let should_score =
            matches!(event, HistoryEvent::Stroke | HistoryEvent::WordEnd) && word.has_search_text();
        if should_score && self.settings.is_auto_switch {
            self.auto_switch
                .dictionaries
                .log_score(word, self.layout.as_deref(), is_word_end);
        }
        if word.has_dead_key {
            self.pending.cancel_auto();
            if self.correction.is_active() {
                self.correction.request_cancel();
            }
        } else if !matches!(event, HistoryEvent::Other)
            && !self.correction.is_active()
            && (self.pending.is_none() || self.pending.is_auto())
            && !self.history.is_auto_switch_blocked()
        {
            let is_paused = self.is_auto_switch_paused(now);
            if should_score && (!is_paused || (is_word_end && self.settings.is_auto_switch)) {
                self.update_auto_switch(index, is_word_end, is_paused, now);
            } else if !is_paused {
                self.update_pattern_switch(index, event, now);
            }
        }
    }

    fn update_pattern_switch(&mut self, index: usize, event: HistoryEvent, now: Instant) {
        if !self.settings.is_pattern_switch {
            if matches!(event, HistoryEvent::LeadingPunctuation) {
                self.pending.cancel_auto();
            }
            return;
        }
        let Some(layout) = self.layout.as_deref() else {
            self.pending.cancel_auto();
            return;
        };
        let word = self.history.latest_word().unwrap();
        if let Some(target) = self.auto_switch.target(word, layout) {
            self.pending
                .update_auto(index, Some((target, AutoSwitchKind::Regex)), now);
        } else if matches!(event, HistoryEvent::AdditionalSeparator) {
            self.pending.cancel_auto_kind(AutoSwitchKind::Regex);
            if (self.pending.is_auto() || word.chain_member.is_some())
                && self.auto_switch.matches_current(word, layout)
            {
                self.pending.cancel_auto_kind(AutoSwitchKind::Dictionary);
                self.history.latest_word_mut().unwrap().chain_member = None;
            }
        } else {
            self.pending.cancel_auto();
        }
    }

    fn is_auto_switch_paused(&self, now: Instant) -> bool {
        self.last_mouse_motion
            .is_some_and(|at| now.saturating_duration_since(at) < self.mouse_motion_pause)
    }

    fn update_auto_switch(
        &mut self,
        index: usize,
        is_word_end: bool,
        is_paused: bool,
        now: Instant,
    ) {
        let Some(layout) = self.layout.as_deref() else {
            self.pending.cancel_auto();
            return;
        };
        let word = self.history.latest_word().unwrap();
        let decision =
            self.auto_switch
                .decision(word, layout, self.settings, is_word_end, |target| {
                    if is_paused {
                        return 0.0;
                    }
                    self.preceding_chain_members(layout, target)
                        .filter_map(|member| match member {
                            ChainMember::Ambiguous { delta_score } => Some(delta_score),
                            ChainMember::DeferredByPunctuation => None,
                        })
                        .sum()
                });
        let target = match decision {
            AutoSwitchDecision::Keep => None,
            AutoSwitchDecision::Switch(target, kind) => Some((target, kind)),
            AutoSwitchDecision::MarkChainMember(member) => {
                if !is_paused || matches!(member, ChainMember::Ambiguous { .. }) {
                    self.history.latest_word_mut().unwrap().chain_member = Some(member);
                }
                None
            }
        };
        if !is_paused {
            self.pending.update_auto(index, target, now);
        }
    }

    fn should_reset_for_key(&self, key: KeyCode, value: i32) -> bool {
        if !matches!(value, 1 | 2) {
            return false;
        }
        if NAVIGATION_KEYS.contains(&key) || key == KeyCode::KEY_TAB || key == KeyCode::KEY_DELETE {
            return true;
        }
        self.keyboard_input.has_shortcut_modifier() && !MODIFIERS.contains(&key)
    }

    fn request_manual_switch(&mut self, source: usize) {
        if self.correction.is_active() || self.latest_word().is_some_and(|word| word.has_dead_key) {
            return;
        }
        let mut is_learning = false;
        for (index, code) in self.keyboard_input.held_keys() {
            let key = KeyCode::new(code);
            if (index == source && key == self.switch_key) || !MODIFIERS.contains(&key) {
                continue;
            }
            if !is_shift(key) {
                return;
            }
            is_learning = true;
        }
        if self.pending.is_none() || (is_learning && self.pending.is_auto()) {
            self.pending = PendingSwitch::manual(source, is_learning);
        }
    }

    fn request_history_reset(&mut self) {
        self.pending.cancel_auto();
        self.reset_history();
        if self.correction.is_active() {
            self.correction.request_cancel();
        }
    }

    fn start_back_off(&mut self) {
        if self.is_backing_off {
            return;
        }
        self.is_backing_off = true;
        self.request_history_reset();
        self.pending.cancel();
    }

    fn update_back_off(&mut self, now: Instant) {
        if self.keyboard_input.expire_holds(now) {
            self.start_back_off();
        }
        if self.is_backing_off && self.keyboard_input.all_released() {
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
        self.correction.take_cancel_request()
    }

    fn write_source(&mut self, index: usize, key: KeyCode, value: i32) -> ProcessingResult<()> {
        let result = self.keyboard_input.write_source(index, key, value);
        self.device_result(result)
    }

    fn release_modifiers(&mut self) -> ProcessingResult<()> {
        let mut error = None;
        for index in 0..self.keyboard_input.len() {
            for key in MODIFIERS {
                if self.keyboard_input.is_key_held(index, key.0)
                    && let Err(err) = self.write_source(index, key, 0)
                {
                    log::error!("RELEASE modifier keyboard={index} {key:?}: {err}");
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn release_locks(&mut self) -> ProcessingResult<()> {
        let mut error = None;
        for index in 0..self.keyboard_input.len() {
            for key in LOCK_KEYS {
                if let Err(err) = self.write_source(index, key, 0) {
                    log::error!("RELEASE lock keyboard={index} {key:?}: {err}");
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn grab(&mut self) -> ProcessingResult<()> {
        for index in 0..self.keyboard_input.len() {
            let result = self.keyboard_input.grab(index);
            self.device_result(result)?;
        }
        Ok(())
    }

    fn ungrab(&mut self) -> ProcessingResult<()> {
        let result = self.keyboard_input.ungrab();
        self.device_result(result)
    }

    fn snapshot(&mut self, mut state: Option<&mut InterceptionState>) -> ProcessingResult<()> {
        let mut error = None;
        for index in 0..self.keyboard_input.len() {
            let keys = self.keyboard_input.snapshot(index).cloned();
            let keys = match self.device_result(keys) {
                Ok(keys) => keys,
                Err(err) => {
                    log::error!("SNAPSHOT keyboard={index}: {err}");
                    error.get_or_insert(err);
                    continue;
                }
            };
            if let Some(recording) = state
                .as_deref_mut()
                .and_then(|state| state.recording.as_mut())
            {
                recording
                    .presses
                    .extend(keys.iter().map(|&code| (index, code)));
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn device_result<T>(&mut self, result: io::Result<T>) -> ProcessingResult<T> {
        match result {
            Err(error) if is_disconnection(&error) && self.lifecycle == Lifecycle::Running => {
                log::info!("Keyboard disconnected during device operation: {error}");
                self.devices_changed(Instant::now());
                self.lifecycle.check().map(|()| unreachable!())
            }
            result => result.map_err(ProcessingError::Io),
        }
    }
}

fn is_shift(key: KeyCode) -> bool {
    matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT)
}

fn is_disconnection(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(nix::libc::ENODEV | nix::libc::ENOENT)
    )
}

fn start_reader(
    mut device: RawDevice,
    path: PathBuf,
    index: usize,
    sender: InputEventSender,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("input-{}", path.display()))
        .spawn(move || {
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            let message = InputEventMessage::Keyboard(index, event);
                            if !sender.send(message) {
                                return;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if is_disconnection(&error) => {
                        sender.send(InputEventMessage::DeviceDisconnected(index, Instant::now()));
                        return;
                    }
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
        || is_text_key(key)
        || MODIFIERS.contains(&key)
        || [
            KeyCode::KEY_KPENTER,
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
        on_grab: Option<InputEventMessage>,
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
            if let Some(event) = keyboard.on_grab.take() {
                keyboard.events.sender.as_ref().unwrap().send(event);
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
        let keyboards = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                source.borrow_mut().events = SimulatedEvents {
                    sender: Some(sender.clone()),
                    index,
                };
                (
                    PathBuf::from(format!("keyboard-{index}")),
                    Box::new(SimulatedSource(source.clone())) as Box<dyn Source>,
                    source.borrow().held.clone(),
                )
            })
            .collect();
        Input {
            keyboard_input: KeyboardInput::from_sources(keyboards),
            lifecycle: Lifecycle::Running,
            history: History::default(),
            statistics: None,
            auto_switch: Default::default(),
            settings: RuntimeSettings {
                is_auto_switch: true,
                is_pattern_switch: true,
                ..RuntimeSettings::default()
            },
            layout: None,
            observed_layout: None,
            events,
            correction: CorrectionState::default(),
            switch_key: KeyCode::KEY_PAUSE,
            pending: PendingSwitch::manual(0, false),
            is_backing_off: false,
            mouse_motion_pause: Duration::from_millis(crate::config::DEFAULT_MOUSE_MOTION_PAUSE_MS),
            last_mouse_motion: None,
        }
    }

    pub(crate) fn statistics_input(statistics: crate::statistics::Statistics) -> Input {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[source]);
        input.pending.cancel();
        input.statistics = Some(statistics);
        input.accept_layout(Some(layout("us")), false);
        input
    }

    pub(crate) fn statistics_event(input: &mut Input, key: KeyCode, value: i32) {
        input
            .process_at(
                0,
                InputEvent::new(EventType::KEY.0, key.0, value),
                Instant::now(),
                None,
            )
            .unwrap();
    }

    fn auto_input() -> Input {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(&[source]);
        input.pending.cancel();
        let settings = crate::config::from_contents(
            r#"
switch_key = 'KEY_PAUSE'
pattern_switch = true
[[switch_rules]]
target = 'us'
patterns = ['^/[a-z]']
[[switch_rules]]
target = 'ru'
patterns = ['^\.тф']
"#,
        )
        .unwrap();
        let switch = AutoSwitch::compile(&settings, |name| {
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

    fn auto_input_with_patterns(patterns: &str) -> Input {
        let mut input = auto_input();
        let settings = crate::config::from_contents(&format!(
            "switch_key = 'KEY_PAUSE'\npattern_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = [{patterns}]"
        ))
        .unwrap();
        input.auto_switch = AutoSwitch::compile(&settings, |name| Ok(layout(name))).unwrap();
        input
    }

    fn press(input: &mut Input, key: KeyCode) {
        input
            .process_at(
                0,
                InputEvent::new(EventType::KEY.0, key.0, 1),
                Instant::now(),
                None,
            )
            .unwrap();
    }

    fn enable_punctuation_boundaries(input: &mut Input) {
        let config = crate::config::from_contents("switch_key = 'KEY_PAUSE'").unwrap();
        input.auto_switch.punctuation =
            crate::punctuation::Punctuation::new(&config.punctuation).unwrap();
        input
            .auto_switch
            .update_layouts(vec![(*layout("us")).clone(), (*layout("ru")).clone()]);
    }

    #[test]
    fn manual_request_keeps_all_trailing_punctuation() {
        use KeyCode as K;
        let (mut input, _) = channel_input();
        input.pending.cancel();
        input.accept_layout(Some(layout("us")), false);
        enable_punctuation_boundaries(&mut input);
        for key in [K::KEY_H, K::KEY_O, K::KEY_M, K::KEY_E]
            .into_iter()
            .chain(std::iter::repeat_n(K::KEY_SLASH, 7))
        {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
        }
        press(&mut input, K::KEY_PAUSE);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Manual);
        assert_eq!(request.words.len(), 1);
        assert_eq!(request.words[0].text().as_deref(), Ok("home///////"));
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
        input.pending.cancel();
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
        input.pending.cancel();
        press(&mut input, KeyCode::KEY_A);
        assert!(input.latest_word().unwrap().entries[0].stroke.shift);
        let mut state = InterceptionState::default();
        state.is_captured = true;
        input.correction.activate();
        input
            .process_at(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 0),
                Instant::now(),
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

        assert!(matches!(
            input.next_request().unwrap(),
            InputRequest::Switch
        ));
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Manual);
        assert!(request.target.is_none());

        assert_eq!(input.pending.source(), Some(0));
        assert!(matches!(
            input.events.available().unwrap(),
            Some(InputEventMessage::FocusChanged)
        ));
    }

    #[test]
    fn learning_uses_shift_at_switch_press_without_enabling_auto_switch() {
        let (mut input, _) = channel_input();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::learned_rules::FILE_NAME);
        let config = crate::config::from_contents("switch_key = 'KEY_PAUSE'").unwrap();
        input.auto_switch.dictionaries = crate::dictionary::DictionaryAutoSwitch::load(
            &config,
            |_| panic!("No dictionary layout should be resolved"),
            || panic!("No dictionary layout count should be requested"),
        )
        .unwrap();
        input.auto_switch.dictionaries.learned =
            crate::learned_rules::LearnedRules::load(path.clone()).unwrap();
        input.pending.cancel();
        input.settings.is_auto_switch = false;
        let now = Instant::now();
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, now);
        press(&mut input, KeyCode::KEY_RIGHTSHIFT);
        press(&mut input, KeyCode::KEY_PAUSE);
        event_at(&mut input, 0, KeyCode::KEY_RIGHTSHIFT, 0, now);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Manual);
        assert!(request.is_learning);
        assert!(
            !request.words.into_iter().last().unwrap().entries[0]
                .stroke
                .shift
        );
        input.remember_word("ru", "ф").unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let saved = contents.parse::<toml::Table>().unwrap();
        assert_eq!(saved["ru"]["ф"].as_integer(), Some(1));
    }

    #[test]
    fn automatic_request_owns_word_and_target_after_input_changes() {
        let (mut input, _) = channel_input();
        press(&mut input, KeyCode::KEY_A);
        let expected_word = input.latest_word().cloned();
        input.pending.update_auto(
            0,
            Some((layout("ru"), AutoSwitchKind::Regex)),
            Instant::now(),
        );

        assert!(matches!(
            input.next_request().unwrap(),
            InputRequest::Switch
        ));
        let request = input.switch_request().unwrap();
        input.reset_history();
        input.pending.update_auto(
            0,
            Some((layout("us"), AutoSwitchKind::Regex)),
            Instant::now(),
        );

        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.words.last(), expected_word.as_ref());
        assert_eq!(request.target.unwrap().name(), "ru");
    }

    #[test]
    fn automatic_request_replacement_uses_the_new_press_timestamp() {
        use crate::switcher::SwitchMode;

        let mut input = auto_input_with_patterns("'^ca$'");
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("ru", "", "ф 1", 5.0), ("us", "", "c 1", 5.0)]);
        let first_press = Instant::now() - Duration::from_millis(10);
        let second_press = first_press + Duration::from_millis(5);

        event_at(&mut input, 0, KeyCode::KEY_C, 1, first_press);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, SwitchMode::Auto);
        assert_eq!(request.triggered_at, Some(first_press));

        event_at(&mut input, 0, KeyCode::KEY_C, 0, first_press);
        event_at(&mut input, 0, KeyCode::KEY_A, 1, second_press);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, SwitchMode::Pattern);
        assert_eq!(request.triggered_at, Some(second_press));
    }

    #[test]
    fn io_interrupted_is_returned_as_an_error_instead_of_lifecycle_interrupt() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        sender.send(InputEventMessage::DeviceError(io::Error::from(
            io::ErrorKind::Interrupted,
        )));

        assert_eq!(
            input.next_request().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(input.lifecycle, Lifecycle::Running);
    }

    #[test]
    fn pointer_press_and_event_loss_reset_history_without_restarting() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        press(&mut input, KeyCode::KEY_A);
        input.pending.update_auto(
            0,
            Some((layout("us"), AutoSwitchKind::Regex)),
            Instant::now(),
        );
        sender.send(InputEventMessage::PointerPressed);
        input.process_available_events(None).unwrap();
        assert!(input.latest_word().is_none());
        assert!(input.pending.is_none());
        assert!(input.pending.target().is_none());
        assert_eq!(input.lifecycle, Lifecycle::Running);

        press(&mut input, KeyCode::KEY_B);
        sender.send(InputEventMessage::PointerEventsLost);
        input.process_available_events(None).unwrap();
        assert!(input.latest_word().is_none());
        assert_eq!(input.lifecycle, Lifecycle::Running);
    }

    #[test]
    fn mouse_motion_cancels_pending_pattern_and_restarts_pause_from_latest_motion() {
        let mut input = auto_input();
        let now = Instant::now();
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N] {
            event_at(&mut input, 0, key, 1, now);
            event_at(&mut input, 0, key, 0, now);
        }
        assert!(input.pending.is_auto());

        let second_motion = now + input.mouse_motion_pause / 2;
        for at in [now, second_motion] {
            input
                .process_message(InputEventMessage::MouseMoved(at), None)
                .unwrap();
            assert!(input.pending.is_none());
        }

        let at = now + input.mouse_motion_pause;
        event_at(&mut input, 0, KeyCode::KEY_A, 1, at);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, at);
        assert!(input.pending.is_none());
        assert_eq!(input.latest_word().unwrap().entries.len(), 3);

        let end = second_motion + input.mouse_motion_pause;
        event_at(&mut input, 0, KeyCode::KEY_SPACE, 1, end);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.target.unwrap().name(), "us");
    }

    #[test]
    fn mouse_motion_pause_blocks_dictionary_switch_at_first_space() {
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "bkbfoo 1", 5.0),
            ("ru", "", "или 1", 7.0),
        ]);
        let now = Instant::now();
        input
            .process_message(InputEventMessage::MouseMoved(now), None)
            .unwrap();
        for key in [
            KeyCode::KEY_B,
            KeyCode::KEY_K,
            KeyCode::KEY_B,
            KeyCode::KEY_SPACE,
        ] {
            event_at(&mut input, 0, key, 1, now + TEST_STEP);
            event_at(&mut input, 0, key, 0, now + TEST_STEP);
        }
        assert!(input.pending.is_none());
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("bkb"));
        assert_eq!(input.latest_word().unwrap().trailing_spaces, 1);
    }

    #[test]
    fn mouse_motion_pause_preserves_ambiguity_for_next_dictionary_switch() {
        let mut input = ambiguity_input("", "f 1\nbp 1");
        let motion_at = Instant::now();
        input
            .process_message(InputEventMessage::MouseMoved(motion_at), None)
            .unwrap();
        let paused_at = motion_at + TEST_STEP;
        for key in [KeyCode::KEY_F, KeyCode::KEY_SPACE] {
            event_at(&mut input, 0, key, 1, paused_at);
            event_at(&mut input, 0, key, 0, paused_at);
        }
        assert!(input.switch_request().is_none());
        assert!(input.latest_word().unwrap().chain_member.is_some());

        let resumed_at = motion_at + input.mouse_motion_pause;
        event_at(&mut input, 0, KeyCode::KEY_R, 1, resumed_at);
        event_at(&mut input, 0, KeyCode::KEY_R, 0, resumed_at);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        assert_eq!(request.target.unwrap().name(), "ru");
        assert_eq!(request.words.len(), 2);
        assert_eq!(request.words[0].text().unwrap(), "f");
        assert_eq!(request.words[0].trailing_spaces, 1);
        assert_eq!(request.words[1].text().unwrap(), "r");
    }

    #[test]
    fn mouse_motion_keeps_manual_request_and_allows_correction_to_finish() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        input.accept_layout(Some(layout("us")), false);
        let now = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_A, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, now);
        sender.send(InputEventMessage::MouseMoved(now));
        input.process_available_events(None).unwrap();

        event_at(&mut input, 0, KeyCode::KEY_B, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_B, 0, now);
        event_at(&mut input, 0, KeyCode::KEY_PAUSE, 1, now);
        event_at(&mut input, 0, KeyCode::KEY_PAUSE, 0, now);
        sender.send(InputEventMessage::MouseMoved(now));
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        let request = interception.take_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Manual);
        assert_eq!(request.words[0].text().as_deref(), Ok("ab"));

        sender.send(InputEventMessage::MouseMoved(now));
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        assert!(!interception.take_cancel_request());
        interception.finish().unwrap();
    }

    #[test]
    fn mouse_motion_with_zero_pause_preserves_automatic_requests() {
        let mut input = auto_input();
        input.mouse_motion_pause = Duration::ZERO;
        let now = Instant::now();
        input
            .process_message(InputEventMessage::MouseMoved(now), None)
            .unwrap();
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N] {
            event_at(&mut input, 0, key, 1, now);
            event_at(&mut input, 0, key, 0, now);
        }
        assert!(input.pending.is_auto());
        input
            .process_message(InputEventMessage::MouseMoved(now), None)
            .unwrap();
        assert!(input.pending.is_auto());
    }

    #[test]
    fn inactive_history_reset_does_not_cancel_but_lifecycle_changes_do() {
        let (mut input, _) = channel_input();
        input.request_history_reset();
        assert!(!input.take_cancel_request());
        assert!(!input.correction.is_cancelled());

        input.devices_changed(Instant::now());
        assert!(input.take_cancel_request());
        assert!(input.correction.is_cancelled());

        let (mut input, _) = channel_input();
        input
            .process_message(InputEventMessage::Shutdown(ShutdownReason::Stop), None)
            .unwrap();
        assert!(input.take_cancel_request());
        assert!(input.correction.is_cancelled());
    }

    #[test]
    fn pointer_press_during_correction_requests_cancellation() {
        let (mut input, sender) = channel_input();
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        sender.send(InputEventMessage::PointerPressed);
        assert!(interception.wait_for_correction().unwrap().is_none());
        assert!(interception.take_cancel_request());
        assert!(interception.input().latest_word().is_none());
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        interception.finish().unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn switch_request_wait_processes_the_entire_event_batch() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        let worker = thread::spawn(move || {
            for value in [1, 0] {
                sender.send(InputEventMessage::Keyboard(
                    0,
                    InputEvent::new(EventType::KEY.0, KeyCode::KEY_PAUSE.0, value),
                ));
            }
        });
        worker.join().unwrap();

        assert!(matches!(
            input.next_request().unwrap(),
            InputRequest::Switch
        ));

        assert_eq!(input.pending.source(), Some(0));
        assert!(!input.pending.is_original_key_held());
        assert!(!input.keyboard_input.is_key_held(0, KeyCode::KEY_PAUSE.0));
        assert!(input.events.available().unwrap().is_none());
    }

    #[test]
    fn switch_request_wait_propagates_device_errors() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        sender.send(InputEventMessage::DeviceError(io::Error::other(
            "device disconnected",
        )));

        assert_eq!(
            input.next_request().unwrap_err().to_string(),
            "device disconnected"
        );
    }

    #[test]
    fn shutdown_wakes_input_and_discovery_without_any_keyboards() {
        for is_discovering in [false, true] {
            let mut input = simulated_input(&[]);
            input.pending.cancel();
            let (sender, events) = InputEvents::new();
            input.events = events;
            sender.shutdown(ShutdownReason::Stop).unwrap();
            if is_discovering {
                assert!(matches!(
                    input.wait_for_keyboard_discovery(TEST_STEP),
                    Err(ProcessingError::Interrupted)
                ));
            } else {
                assert!(matches!(
                    input.next_request().unwrap(),
                    InputRequest::Shutdown(ShutdownReason::Stop)
                ));
            }
        }
    }

    #[test]
    fn shutdown_during_entry_repairs_hidden_input_before_releasing_capture() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            is_entry_interrupted: true,
            on_grab: Some(InputEventMessage::Shutdown(ShutdownReason::Stop)),
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let Err(error) = Interceptor::enter(&mut input) else {
            panic!("entry must be interrupted");
        };
        assert!(matches!(error, ProcessingError::Interrupted));
        let keyboard = source.borrow();
        assert!(!keyboard.is_grabbed);
        assert!(keyboard.writes.contains(&(KeyCode::KEY_A, 1, true)));
        assert!(keyboard.writes.contains(&(KeyCode::KEY_A, 0, false)));
        drop(keyboard);
        assert_eq!(input.wait_for_resume().unwrap(), Some(ShutdownReason::Stop));
    }

    #[test]
    fn disconnection_cancels_correction_wait_and_cleans_up_with_keys_held() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        let sender = source.borrow().events.sender.as_ref().unwrap().clone();
        source
            .borrow_mut()
            .held
            .extend([KeyCode::KEY_A.0, KeyCode::KEY_LEFTSHIFT.0]);
        source.borrow_mut().events.extend([
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1),
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 1),
        ]);
        sender.send(InputEventMessage::DeviceDisconnected(0, Instant::now()));
        assert!(matches!(
            interception.wait_for_correction().unwrap_err(),
            ProcessingError::Interrupted
        ));
        assert!(interception.take_cancel_request());
        sender.shutdown(ShutdownReason::Stop).unwrap();
        sender.shutdown(ShutdownReason::Restart).unwrap();
        drop(interception);
        assert!(!source.borrow().is_grabbed);
        assert!(!source.borrow().held.contains(&KeyCode::KEY_LEFTSHIFT.0));
        assert!(source.borrow().held.contains(&KeyCode::KEY_A.0));
        assert_eq!(input.wait_for_resume().unwrap(), Some(ShutdownReason::Stop));
    }

    #[test]
    fn shutdown_interrupts_release_wait_without_waiting_for_user() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        let interception = Interceptor::enter(&mut input).unwrap().unwrap();
        source.borrow_mut().held.insert(KeyCode::KEY_A.0);
        source
            .borrow_mut()
            .events
            .push(InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1));
        source
            .borrow()
            .events
            .sender
            .as_ref()
            .unwrap()
            .shutdown(ShutdownReason::Restart)
            .unwrap();
        assert!(matches!(
            interception.finish().unwrap_err(),
            ProcessingError::Interrupted
        ));
        assert!(!source.borrow().is_grabbed);
        assert!(source.borrow().held.contains(&KeyCode::KEY_A.0));
        assert_eq!(
            input.wait_for_resume().unwrap(),
            Some(ShutdownReason::Restart)
        );
    }

    #[test]
    fn restart_wait_ignores_keys_and_release_does_not_resume_processing() {
        let (mut input, sender) = channel_input();
        press(&mut input, KeyCode::KEY_A);
        let now = Instant::now();
        input.devices_changed(now);
        let Lifecycle::PreparingForRestart(deadline) = input.lifecycle else {
            unreachable!()
        };
        input.lifecycle = Lifecycle::WaitingForRestart(deadline);
        let lifecycle = input.lifecycle;
        for key in [KeyCode::KEY_A, KeyCode::KEY_PAUSE] {
            for value in [0, 1, 2, 0] {
                input
                    .process_message(
                        InputEventMessage::Keyboard(
                            0,
                            InputEvent::new(EventType::KEY.0, key.0, value),
                        ),
                        None,
                    )
                    .unwrap();
            }
        }
        assert!(input.latest_word().is_none());
        assert!(input.pending.is_none());
        assert_eq!(input.lifecycle, lifecycle);
        sender.shutdown(ShutdownReason::Restart).unwrap();
        assert_eq!(
            input.wait_for_resume().unwrap(),
            Some(ShutdownReason::Restart)
        );
    }

    #[test]
    fn expired_restart_wait_restarts_once_and_queued_quit_takes_priority() {
        for is_stopping in [false, true] {
            let (mut input, sender) = channel_input();
            input.lifecycle = Lifecycle::PreparingForRestart(Instant::now());
            if is_stopping {
                sender.shutdown(ShutdownReason::Stop).unwrap();
            }
            let reason = if is_stopping {
                ShutdownReason::Stop
            } else {
                ShutdownReason::Restart
            };
            assert!(matches!(
                input.next_request().unwrap(),
                InputRequest::Shutdown(actual) if actual == reason
            ));
        }
    }

    #[test]
    fn queued_layouts_are_applied_in_order_and_duplicate_layouts_keep_history() {
        let (mut input, sender) = channel_input();
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("us")),
            active: Vec::new(),
        });
        sender.send(InputEventMessage::Keyboard(
            0,
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1),
        ));
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("us")),
            active: Vec::new(),
        });
        input.process_available_events(None).unwrap();
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("a"));
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("ru")),
            active: Vec::new(),
        });
        input.process_available_events(None).unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn completion_preserves_early_history_update_and_late_duplicate() {
        let (mut input, sender) = channel_input();
        input.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("ru")),
            active: Vec::new(),
        });
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
        CorrectionInterceptor::input(&mut interception).begin_correction(1, Some(layout("ru")));
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("ru")),
            active: Vec::new(),
        });
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
        assert!(input.history.is_auto_switch_blocked());
    }

    #[test]
    fn layout_change_after_completion_is_reconciled_when_capture_ends() {
        let (mut input, sender) = channel_input();
        input.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
        let mut interception = Interceptor::enter(&mut input).unwrap().unwrap();
        CorrectionInterceptor::input(&mut interception).begin_correction(1, Some(layout("ru")));
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        sender.send(InputEventMessage::LayoutsChanged {
            current: Some(layout("us")),
            active: Vec::new(),
        });
        assert!(interception.wait_for_correction().unwrap().unwrap().is_ok());
        interception.finish().unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn consumed_cancellation_still_prevents_applying_successful_correction() {
        for is_captured in [false, true] {
            let (mut input, sender) = channel_input();
            press(&mut input, KeyCode::KEY_A);
            event_at(&mut input, 0, KeyCode::KEY_A, 0, Instant::now());
            let mut interceptor: Box<dyn CorrectionInterceptor> = if is_captured {
                Box::new(Interceptor::enter(&mut input).unwrap().unwrap())
            } else {
                Box::new(interception::UncapturedInterceptor::enter(&mut input).unwrap())
            };
            sender.send(InputEventMessage::FocusChanged);
            assert!(interceptor.read_available().unwrap());
            assert!(!interceptor.input().take_cancel_request());
            if !is_captured {
                press(interceptor.input(), KeyCode::KEY_C);
                assert!(interceptor.input().latest_word().is_some());
                statistics_event(interceptor.input(), KeyCode::KEY_C, 0);
            }
            sender.send(InputEventMessage::CorrectionFinished {
                result: Ok(()),
                layout: Some(layout("ru")),
            });
            assert!(interceptor.wait_for_correction().unwrap().unwrap().is_ok());
            assert!(interceptor.input().latest_word().is_none());
            assert!(interceptor.input().correction.is_active());
            interceptor.finish().unwrap();
            assert!(!input.correction.is_active());
            assert!(!input.history.is_auto_switch_blocked());
            press(&mut input, KeyCode::KEY_B);
            assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("и"));
        }
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
        input.correction.activate();
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
        assert!(input.correction.is_cancelled());
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
            assert!(input.history.word_before_latest(1).is_some());
            event_at(&mut input, 0, key, 1, now);
            event_at(&mut input, 0, key, 2, now);
            assert!(input.is_backing_off, "{key:?}");
            assert!(input.latest_word().is_none());
            assert!(input.history.word_before_latest(1).is_none());
            assert!(input.pending.is_none());
            assert!(!input.pending.is_original_key_held());
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
        input.update_back_off(now + HOLD_TIMEOUT);
        assert!(!input.is_backing_off);
        input.keyboard_input.update(
            0,
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_B.0, 0),
            now,
        );
        input.keyboard_input.update(
            0,
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_B.0, 1),
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
        input.correction.activate();
        input
            .process_at(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_N.0, 2),
                Instant::now(),
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
            input.update_back_off(now + HOLD_TIMEOUT);
            assert!(!input.is_backing_off);
            input.update_back_off(now + HOLD_TIMEOUT + TEST_STEP);
            assert!(input.is_backing_off, "{key:?}");
            assert!(input.pending.is_none());
        }
    }

    #[test]
    fn manual_correction_blocks_auto_switch_while_editing_the_word() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        complete_manual_correction(&mut input, "us");
        press(&mut input, KeyCode::KEY_N);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_none());
        assert!(input.automatic_target().is_none());
        press(&mut input, KeyCode::KEY_BACKSPACE);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_none());
        assert!(input.history.is_auto_switch_blocked());

        complete_manual_correction(&mut input, "ru");
        assert!(input.history.is_auto_switch_blocked());
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok(".тф"));
    }

    fn complete_manual_correction(input: &mut Input, target: &str) {
        press(input, KeyCode::KEY_PAUSE);
        assert!(input.pending.is_some());
        assert!(!input.is_auto_request());
        input.pending.cancel();
        input.begin_correction(1, Some(layout(target)));
        input
            .process_message(
                InputEventMessage::CorrectionFinished {
                    result: Ok(()),
                    layout: Some(layout(target)),
                },
                None,
            )
            .unwrap();
    }

    #[test]
    fn manual_correction_after_spaces_survives_erasing_spaces() {
        let mut input = auto_input();
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_SPACE, KeyCode::KEY_SPACE] {
            press(&mut input, key);
        }
        complete_manual_correction(&mut input, "us");
        for key in [
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_N,
            KeyCode::KEY_A,
        ] {
            press(&mut input, key);
        }
        assert!(input.pending.is_none());
        assert!(input.history.is_auto_switch_blocked());
    }

    #[test]
    fn erasing_manual_word_allows_auto_switch_when_retyping_it() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        complete_manual_correction(&mut input, "us");
        press(&mut input, KeyCode::KEY_BACKSPACE);
        assert!(!input.history.is_auto_switch_blocked());
        assert!(input.latest_word().is_none());
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N, KeyCode::KEY_A] {
            press(&mut input, key);
        }
        assert_eq!(input.automatic_target().unwrap().name(), "ru");
    }

    #[test]
    fn next_word_forgets_manual_override_even_after_returning_with_backspace() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        complete_manual_correction(&mut input, "us");
        for key in [KeyCode::KEY_SPACE, KeyCode::KEY_B] {
            press(&mut input, key);
        }
        assert!(!input.history.is_auto_switch_blocked());
        for key in [
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_N,
            KeyCode::KEY_A,
        ] {
            press(&mut input, key);
        }
        assert_eq!(input.automatic_target().unwrap().name(), "ru");
    }

    #[test]
    fn context_reset_forgets_manual_override() {
        for key in [KeyCode::KEY_ENTER, KeyCode::KEY_TAB, KeyCode::KEY_LEFT] {
            let mut input = auto_input();
            press(&mut input, KeyCode::KEY_SLASH);
            complete_manual_correction(&mut input, "us");
            press(&mut input, key);
            assert!(!input.history.is_auto_switch_blocked());
            for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N, KeyCode::KEY_A] {
                press(&mut input, key);
            }
            assert_eq!(input.automatic_target().unwrap().name(), "ru");
        }
    }

    #[test]
    fn failed_manual_correction_does_not_block_auto_switch() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        input
            .process_message(
                InputEventMessage::CorrectionFinished {
                    result: Err(io::Error::other("switch failed")),
                    layout: Some(layout("ru")),
                },
                None,
            )
            .unwrap();
        assert!(!input.history.is_auto_switch_blocked());
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N] {
            press(&mut input, key);
        }
        assert_eq!(input.automatic_target().unwrap().name(), "us");
    }

    #[test]
    fn auto_switch_blocks_further_automatic_corrections_of_same_word() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_N);
        assert!(input.is_auto_request());
        let us = input.automatic_target().unwrap();
        assert_eq!(us.name(), "us");
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok(".т"));
        input.begin_correction(1, Some(us.clone()));
        input
            .process_message(
                InputEventMessage::CorrectionFinished {
                    result: Ok(()),
                    layout: Some(us),
                },
                None,
            )
            .unwrap();
        assert!(input.history.is_auto_switch_blocked());
        input.pending.cancel();
        press(&mut input, KeyCode::KEY_A);
        assert!(input.automatic_target().is_none());
        assert!(input.pending.is_none());
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
    fn second_space_can_match_a_pattern_using_both_spaces() {
        let mut input = auto_input_with_patterns("'^foo  $'");
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.automatic_target().is_none());
        for key in [KeyCode::KEY_F, KeyCode::KEY_O, KeyCode::KEY_O] {
            press(&mut input, key);
        }
        assert!(input.automatic_target().is_none());
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.automatic_target().is_none());
        press(&mut input, KeyCode::KEY_SPACE);
        assert_eq!(input.automatic_target().unwrap().name(), "us");
        assert_eq!(input.latest_word().unwrap().trailing_spaces, 2);
    }

    #[test]
    fn regex_matches_the_full_punctuation_series_and_spaces() {
        use KeyCode as K;
        for (pattern, spaces) in [("'^home//$'", 0), ("'^home// $'", 1), ("'^home//  $'", 2)] {
            let mut input = auto_input_with_patterns(pattern);
            enable_punctuation_boundaries(&mut input);
            tap_keys(
                &mut input,
                [K::KEY_H, K::KEY_O, K::KEY_M, K::KEY_E, K::KEY_SLASH],
            );
            assert!(input.switch_request().is_none());
            for key in
                std::iter::once(K::KEY_SLASH).chain(std::iter::repeat_n(K::KEY_SPACE, spaces))
            {
                assert!(input.switch_request().is_none());
                tap_keys(&mut input, [key]);
            }
            let request = input.switch_request().unwrap();
            assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
            let word = &request.words[0];
            assert_eq!(
                request.target.unwrap().to_text(word.strokes()).as_deref(),
                Ok("home//")
            );
            assert_eq!(word.trailing_spaces, spaces);
        }
    }

    #[test]
    fn home_path_pattern_matches_on_the_first_strong_separator() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("'^~/'");
        enable_punctuation_boundaries(&mut input);
        press(&mut input, K::KEY_LEFTSHIFT);
        tap_keys(&mut input, [K::KEY_GRAVE]);
        statistics_event(&mut input, K::KEY_LEFTSHIFT, 0);
        assert!(input.switch_request().is_none());
        tap_keys(&mut input, [K::KEY_SLASH]);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.words[0].text().as_deref(), Ok("Ё."));
    }

    #[test]
    fn punctuation_uses_one_exact_lookup_and_later_regex_can_take_priority() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("'^bkb// $'");
        enable_punctuation_boundaries(&mut input);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "bkb 1", 5.0),
            ("ru", "", "илик 1", 5.0),
        ]);
        tap_keys(&mut input, [K::KEY_B, K::KEY_K, K::KEY_B]);
        assert!(input.switch_request().is_none());
        assert_eq!(
            logged_dictionary_scores(&mut input, K::KEY_SLASH, log::LevelFilter::Debug).len(),
            1
        );
        statistics_event(&mut input, K::KEY_SLASH, 0);
        assert_eq!(
            input.switch_request().unwrap().mode,
            crate::switcher::SwitchMode::Auto
        );
        for key in [K::KEY_SLASH, K::KEY_SPACE] {
            assert!(logged_dictionary_scores(&mut input, key, log::LevelFilter::Debug).is_empty());
            statistics_event(&mut input, key, 0);
        }
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.words[0].text().as_deref(), Ok("или.."));
        assert_eq!(request.words[0].trailing_spaces, 1);
    }

    #[test]
    fn learned_search_uses_the_word_inside_strong_punctuation() {
        use KeyCode as K;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::learned_rules::FILE_NAME);
        std::fs::write(&path, "[us]\n\"/new\" = 3\nnew = 2\nnews = 7\n").unwrap();
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "new 1", 5.0),
            ("ru", "", "туц 1", 5.0),
        ]);
        input.auto_switch.dictionaries.learned =
            crate::learned_rules::LearnedRules::load(path).unwrap();
        let us = layout("us");
        let ru = layout("ru");
        input
            .process_message(
                InputEventMessage::LayoutsChanged {
                    current: Some(ru.clone()),
                    active: vec![(*us).clone(), (*ru).clone()],
                },
                None,
            )
            .unwrap();

        tap_keys(&mut input, [K::KEY_SLASH, K::KEY_N, K::KEY_E, K::KEY_W]);
        let word = input.latest_word().unwrap();
        assert_eq!(
            input
                .auto_switch
                .dictionaries
                .comparison(word, &us, false)
                .unwrap()
                .learned_difference(),
            (std::cmp::Ordering::Less, 12)
        );
        tap_keys(&mut input, [K::KEY_SLASH, K::KEY_SLASH]);
        let word = input.latest_word().unwrap();
        assert_eq!(
            input
                .auto_switch
                .dictionaries
                .comparison(word, &us, true)
                .unwrap()
                .learned_difference(),
            (std::cmp::Ordering::Less, 5)
        );
        assert_eq!(us.to_text(word.strokes()).as_deref(), Ok("/new//"));
    }

    #[test]
    fn first_space_clears_unmatched_target_and_respects_manual_block() {
        let mut input = auto_input_with_patterns("'^foo$'");
        for key in [KeyCode::KEY_F, KeyCode::KEY_O, KeyCode::KEY_O] {
            press(&mut input, key);
        }
        assert_eq!(input.automatic_target().unwrap().name(), "us");
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.automatic_target().is_none());

        let mut input = auto_input_with_patterns("'^foo $'");
        press(&mut input, KeyCode::KEY_F);
        complete_manual_correction(&mut input, "us");
        for key in [KeyCode::KEY_O, KeyCode::KEY_O, KeyCode::KEY_SPACE] {
            press(&mut input, key);
        }
        assert!(input.automatic_target().is_none());
    }

    #[test]
    fn ambiguity_is_reassessed_only_on_the_first_space_and_cleared_by_context_reset() {
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "a 1\nz 3", 5.0),
            ("ru", "", "ф 2\nя 2", 5.0),
        ]);
        for (key, expected) in [
            (KeyCode::KEY_A, None),
            (
                KeyCode::KEY_SPACE,
                Some(ChainMember::Ambiguous { delta_score: 1.0 }),
            ),
            (
                KeyCode::KEY_SPACE,
                Some(ChainMember::Ambiguous { delta_score: 1.0 }),
            ),
            (
                KeyCode::KEY_BACKSPACE,
                Some(ChainMember::Ambiguous { delta_score: 1.0 }),
            ),
            (KeyCode::KEY_BACKSPACE, None),
            (
                KeyCode::KEY_SPACE,
                Some(ChainMember::Ambiguous { delta_score: 1.0 }),
            ),
        ] {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
            assert_eq!(
                input.latest_word().unwrap().chain_member,
                expected,
                "{key:?}"
            );
            assert!(input.switch_request().is_none());
        }
        input
            .process_message(InputEventMessage::FocusChanged, None)
            .unwrap();
        assert!(input.latest_word().is_none());
    }

    #[test]
    fn ambiguity_respects_pattern_priority_and_dictionary_guards() {
        use crate::switcher::SwitchMode;
        for (source, patterns, is_auto, key, expected_mode) in [
            (
                "ru",
                "'^a $'",
                true,
                KeyCode::KEY_A,
                Some(SwitchMode::Pattern),
            ),
            ("us", "'^a $'", true, KeyCode::KEY_A, None),
            ("us", "", false, KeyCode::KEY_A, None),
            ("us", "", true, KeyCode::KEY_GRAVE, None),
        ] {
            let mut input = auto_input_with_patterns(patterns);
            input.accept_layout(Some(layout(source)), false);
            input.settings.is_auto_switch = is_auto;
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 1\n` 1", 5.0),
                ("ru", "", "ф 1\nё 1", 5.0),
            ]);
            press(&mut input, key);
            press(&mut input, KeyCode::KEY_SPACE);
            assert_eq!(
                input.switch_request().map(|request| request.mode),
                expected_mode
            );
            assert!(input.latest_word().unwrap().chain_member.is_none());
        }

        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "a 1", 5.0), ("ru", "", "ф 1", 5.0)]);
        press(&mut input, KeyCode::KEY_A);
        complete_manual_correction(&mut input, "us");
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.latest_word().unwrap().chain_member.is_none());
    }

    const BOX_KEYS: [KeyCode; 7] = [
        KeyCode::KEY_R,
        KeyCode::KEY_J,
        KeyCode::KEY_H,
        KeyCode::KEY_J,
        KeyCode::KEY_COMMA,
        KeyCode::KEY_R,
        KeyCode::KEY_B,
    ];

    fn ambiguity_input(patterns: &str, us_records: &str) -> Input {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        ambiguity_input_with_source(&source, patterns, us_records)
    }

    fn ambiguity_input_with_source(
        source: &Rc<RefCell<SimulatedKeyboard>>,
        patterns: &str,
        us_records: &str,
    ) -> Input {
        let mut input = simulated_input(std::slice::from_ref(source));
        input.pending.cancel();
        input.accept_layout(Some(layout("us")), false);
        let config = crate::config::from_contents(&format!(
            "switch_key = 'KEY_PAUSE'\npattern_switch = true\n[[switch_rules]]\ntarget = 'gb'\npatterns = [{patterns}]"
        )).unwrap();
        input.auto_switch = AutoSwitch::compile(&config, |name| Ok(layout(name))).unwrap();
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", us_records, 5.0),
            ("ru", "", "а 1\nиз 1\nкоробки 16", 5.0),
        ]);
        input
    }

    fn tap_keys(input: &mut Input, keys: impl IntoIterator<Item = KeyCode>) {
        for key in keys {
            press(input, key);
            statistics_event(input, key, 0);
        }
    }

    #[test]
    fn keypad_navigation_keys_stay_out_of_history_across_num_lock_toggle() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("");
        let keypad_keys = [
            K::KEY_KP0,
            K::KEY_KP1,
            K::KEY_KP2,
            K::KEY_KP3,
            K::KEY_KP4,
            K::KEY_KP5,
            K::KEY_KP6,
            K::KEY_KP7,
            K::KEY_KP8,
            K::KEY_KP9,
            K::KEY_KPDOT,
        ];

        tap_keys(&mut input, [K::KEY_F]);
        tap_keys(&mut input, keypad_keys);
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("а"));

        tap_keys(&mut input, [K::KEY_NUMLOCK]);
        tap_keys(&mut input, keypad_keys);
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("а"));
    }

    #[test]
    fn dictionary_switch_includes_keypad_plus_in_the_word() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("");
        enable_punctuation_boundaries(&mut input);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "us+ru 32", 5.0),
            ("ru", "", "гы+кг 1\nгы+кы 64", 5.0),
        ]);

        tap_keys(&mut input, [K::KEY_U, K::KEY_S, K::KEY_KPPLUS, K::KEY_R]);
        assert!(input.switch_request().is_none());

        tap_keys(&mut input, [K::KEY_U]);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        let [word] = request.words.as_slice() else {
            panic!("expected one word");
        };
        assert_eq!(word.text().as_deref(), Ok("гы+кг"));
        let target = request.target.unwrap();
        assert_eq!(target.name(), "us");
        assert_eq!(target.to_text(word.strokes()).as_deref(), Ok("us+ru"));
    }

    #[test]
    fn punctuation_defers_a_dictionary_switch_on_the_first_space() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        enable_punctuation_boundaries(&mut input);
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "z 1", 5.0), ("ru", "", "ж 1", 5.0)]);

        for (key, expected) in [
            (K::KEY_SEMICOLON, None),
            (K::KEY_SPACE, Some(ChainMember::DeferredByPunctuation)),
            (K::KEY_SPACE, Some(ChainMember::DeferredByPunctuation)),
            (K::KEY_BACKSPACE, Some(ChainMember::DeferredByPunctuation)),
            (K::KEY_BACKSPACE, None),
            (K::KEY_SPACE, Some(ChainMember::DeferredByPunctuation)),
        ] {
            tap_keys(&mut input, [key]);
            let word = input.latest_word().unwrap();
            assert_eq!(word.chain_member, expected, "{key:?}");
            assert_eq!(word.text().as_deref(), Ok(";"));
            assert!(input.switch_request().is_none());
        }
    }

    #[test]
    fn dictionary_correction_includes_a_deferred_token_with_multiple_signs() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        enable_punctuation_boundaries(&mut input);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "c 16\ncg 8\ncgb 4\nz 36", 5.0),
            ("ru", "", "ёж 1\nспит 16\nя 47", 5.0),
        ]);

        tap_keys(&mut input, [K::KEY_GRAVE, K::KEY_SEMICOLON, K::KEY_SPACE]);
        let word = input.latest_word().unwrap();
        assert_eq!(word.text().as_deref(), Ok("`;"));
        assert_eq!(word.chain_member, Some(ChainMember::DeferredByPunctuation));
        assert!(input.switch_request().is_none());

        tap_keys(&mut input, [K::KEY_C, K::KEY_G, K::KEY_B, K::KEY_N]);
        let request = input.switch_request().unwrap();
        input.begin_correction(request.words.len(), request.target);
        assert_eq!(
            input
                .history
                .latest_words(2)
                .iter()
                .map(|word| (word.text().unwrap(), word.trailing_spaces))
                .collect::<Vec<_>>(),
            [("ёж".into(), 1), ("спит".into(), 0)]
        );
    }

    #[test]
    fn current_layout_regex_and_mouse_pause_prevent_deferring_punctuation() {
        use KeyCode as K;
        for (patterns, is_paused) in [("'^; $'", false), ("", true)] {
            let mut input = auto_input_with_patterns(patterns);
            input.accept_layout(Some(layout("us")), false);
            enable_punctuation_boundaries(&mut input);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "z 1", 5.0),
                ("ru", "", "ж 1", 5.0),
            ]);
            let now = Instant::now();
            if is_paused {
                input
                    .process_message(InputEventMessage::MouseMoved(now), None)
                    .unwrap();
            }
            for key in [K::KEY_SEMICOLON, K::KEY_SPACE] {
                event_at(&mut input, 0, key, 1, now + TEST_STEP);
                event_at(&mut input, 0, key, 0, now + TEST_STEP);
            }
            assert!(input.switch_request().is_none());
            assert_eq!(input.latest_word().unwrap().chain_member, None);
        }
    }

    #[test]
    fn current_layout_regex_on_the_second_space_breaks_the_chain() {
        use KeyCode as K;
        for (key, expected_member, expected_text) in [
            (K::KEY_SEMICOLON, ChainMember::DeferredByPunctuation, ";"),
            (K::KEY_F, ChainMember::Ambiguous { delta_score: 0.0 }, "f"),
        ] {
            let mut input = auto_input_with_patterns("'^; {2}$', '^f {2}$'");
            input.accept_layout(Some(layout("us")), false);
            enable_punctuation_boundaries(&mut input);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "z 1\nf 1\ng 32\ngh 16\nx 14", 5.0),
                ("ru", "", "я 4\nа 1\nж 1\nпра 32\nч 26", 5.0),
            ]);

            tap_keys(&mut input, [K::KEY_Z, K::KEY_SPACE]);
            assert_eq!(
                input.latest_word().unwrap().chain_member,
                Some(ChainMember::Ambiguous { delta_score: 2.0 })
            );
            tap_keys(&mut input, [key, K::KEY_SPACE]);
            assert_eq!(
                input.latest_word().unwrap().chain_member,
                Some(expected_member)
            );
            assert!(input.switch_request().is_none());

            tap_keys(&mut input, [K::KEY_SPACE]);
            assert_eq!(input.latest_word().unwrap().chain_member, None);
            tap_keys(&mut input, [K::KEY_G, K::KEY_H, K::KEY_F]);

            let request = input.switch_request().unwrap();
            assert_eq!(request.words.len(), 1);
            input.begin_correction(request.words.len(), request.target);
            assert_eq!(
                input
                    .history
                    .latest_words(3)
                    .iter()
                    .map(|word| (word.text().unwrap(), word.trailing_spaces))
                    .collect::<Vec<_>>(),
                [
                    ("z".into(), 1),
                    (expected_text.into(), 2),
                    ("пра".into(), 0)
                ]
            );
        }
    }

    #[test]
    fn dictionary_chain_crosses_strong_punctuation_until_manual_correction() {
        use KeyCode as K;
        for is_manually_corrected in [false, true] {
            let mut input = auto_input_with_patterns("");
            enable_punctuation_boundaries(&mut input);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "home 4\nconfig 16\nz 12", 5.0),
                ("ru", "", "рщьу 1\nсщташп 1\nсщташг 4\nя 26", 5.0),
            ]);
            tap_keys(
                &mut input,
                [K::KEY_H, K::KEY_O, K::KEY_M, K::KEY_E, K::KEY_SLASH],
            );
            assert_eq!(
                input.latest_word().unwrap().chain_member,
                Some(ChainMember::Ambiguous { delta_score: 2.0 })
            );
            assert!(input.switch_request().is_none());
            if is_manually_corrected {
                complete_manual_correction(&mut input, "ru");
                assert!(input.latest_word().unwrap().chain_member.is_none());
            }
            tap_keys(
                &mut input,
                [K::KEY_C, K::KEY_O, K::KEY_N, K::KEY_F, K::KEY_I],
            );
            assert!(input.switch_request().is_none());
            tap_keys(&mut input, [K::KEY_G]);
            if is_manually_corrected {
                assert!(input.switch_request().is_none());
            } else {
                let request = input.switch_request().unwrap();
                assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
                let target = request.target.unwrap();
                assert_eq!(
                    request
                        .words
                        .iter()
                        .map(|word| (
                            target.to_text(word.strokes()).unwrap(),
                            word.trailing_spaces
                        ))
                        .collect::<Vec<_>>(),
                    [("home/".into(), 0), ("config".into(), 0)]
                );
            }
        }
    }

    #[test]
    fn ambiguous_chain_scores_trigger_a_prefix_switch_only_for_contiguous_words() {
        for is_interrupted in [false, true] {
            let mut input = auto_input_with_patterns("");
            input.accept_layout(Some(layout("us")), false);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 1\nb 2\nc 1\nz 28", 5.0),
                ("ru", "", "ф 4\nи 4\nс 8\nя 16", 5.0),
            ]);
            for (key, delta_score) in [(KeyCode::KEY_A, 2.0), (KeyCode::KEY_B, 1.0)] {
                tap_keys(&mut input, [key, KeyCode::KEY_SPACE]);
                assert_eq!(
                    input.latest_word().unwrap().chain_member,
                    Some(ChainMember::Ambiguous { delta_score })
                );
                assert!(input.switch_request().is_none());
            }

            if is_interrupted {
                tap_keys(&mut input, [KeyCode::KEY_Q, KeyCode::KEY_SPACE]);
                assert!(input.switch_request().is_none());
                assert_eq!(input.latest_word().unwrap().chain_member, None);
                tap_keys(&mut input, [KeyCode::KEY_C, KeyCode::KEY_SPACE]);
                assert!(input.switch_request().is_none());
                assert_eq!(
                    input.latest_word().unwrap().chain_member,
                    Some(ChainMember::Ambiguous { delta_score: 3.0 })
                );
            } else {
                tap_keys(&mut input, [KeyCode::KEY_C]);
                let request = input.switch_request().unwrap();
                assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
                assert_eq!(request.target.unwrap().name(), "ru");
                assert_eq!(request.words.len(), 3);
                assert!(request.previous.is_none());
            }
        }
    }

    #[test]
    fn deferred_punctuation_keeps_earlier_scores_without_contributing_its_own() {
        use KeyCode as K;
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        enable_punctuation_boundaries(&mut input);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "z 1\nghf 1\ngh 24\ng 48\nq 1\nx 53", 5.0),
            ("ru", "", "я 4\nж 1\nпра 16\nй 8\nч 99", 5.0),
        ]);

        tap_keys(&mut input, [K::KEY_Z, K::KEY_SPACE]);
        assert_eq!(
            input.latest_word().unwrap().chain_member,
            Some(ChainMember::Ambiguous { delta_score: 2.0 })
        );
        tap_keys(&mut input, [K::KEY_SEMICOLON, K::KEY_SPACE]);
        assert_eq!(
            input.latest_word().unwrap().chain_member,
            Some(ChainMember::DeferredByPunctuation)
        );
        assert!(input.switch_request().is_none());

        tap_keys(&mut input, [K::KEY_Q]);
        assert!(input.switch_request().is_none()); // +2 + 3 equals the threshold.
        tap_keys(&mut input, [K::KEY_BACKSPACE, K::KEY_G, K::KEY_H, K::KEY_F]);

        let request = input.switch_request().unwrap(); // +2 + 4 exceeds the threshold.
        assert!(request.previous.is_none());
        input.begin_correction(request.words.len(), request.target);
        assert_eq!(
            input
                .history
                .latest_words(3)
                .iter()
                .map(|word| (word.text().unwrap(), word.trailing_spaces))
                .collect::<Vec<_>>(),
            [("я".into(), 1), ("ж".into(), 1), ("пра".into(), 0)]
        );
    }

    #[test]
    fn negative_ambiguity_score_can_make_a_first_space_switch_equal_the_threshold() {
        for has_ambiguous_word in [false, true] {
            let mut input = auto_input_with_patterns("");
            input.accept_layout(Some(layout("us")), false);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 2\nb 1\nbx 3\nz 122", 5.0),
                ("ru", "", "ф 1\nи 64\nя 63", 5.0),
            ]);
            if has_ambiguous_word {
                tap_keys(&mut input, [KeyCode::KEY_A, KeyCode::KEY_SPACE]);
                assert_eq!(
                    input.latest_word().unwrap().chain_member,
                    Some(ChainMember::Ambiguous { delta_score: -1.0 })
                );
            }

            tap_keys(&mut input, [KeyCode::KEY_B]);
            assert!(input.switch_request().is_none());
            tap_keys(&mut input, [KeyCode::KEY_SPACE]);
            assert_eq!(input.switch_request().is_some(), !has_ambiguous_word);
            assert_eq!(input.latest_word().unwrap().chain_member, None);
        }
    }

    #[test]
    fn captured_prefix_switch_includes_previous_ambiguous_word() {
        let mut input = ambiguity_input("", "f 1\nbp 1");
        tap_keys(&mut input, [KeyCode::KEY_F, KeyCode::KEY_SPACE]);
        assert!(input.switch_request().is_none());

        tap_keys(&mut input, [KeyCode::KEY_R]);
        let mut interceptor = Interceptor::enter(&mut input).unwrap().unwrap();
        assert!(interceptor.input().keyboard_input.is_any_grabbed());
        let request = interceptor.take_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        assert_eq!(request.words.len(), 2);
        assert!(request.previous.is_none());
        assert_eq!(request.words[0].text().unwrap(), "f");
        assert_eq!(request.words[0].trailing_spaces, 1);
        assert_eq!(request.words[1].text().unwrap(), "r");
        assert_eq!(request.words[1].trailing_spaces, 0);
        interceptor.finish().unwrap();
        assert!(!input.keyboard_input.is_any_grabbed());
    }

    #[test]
    fn first_space_switch_includes_entire_ambiguous_chain_without_grab() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = ambiguity_input_with_source(&source, "", "f 1\nbp 1\nrjhj,rbx 16");
        tap_keys(&mut input, [KeyCode::KEY_Q, KeyCode::KEY_SPACE]);
        tap_keys(
            &mut input,
            [
                KeyCode::KEY_F,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_B,
                KeyCode::KEY_P,
                KeyCode::KEY_SPACE,
            ],
        );
        for key in BOX_KEYS {
            tap_keys(&mut input, [key]);
            assert!(input.switch_request().is_none());
        }
        source.borrow_mut().held.insert(KeyCode::KEY_SPACE.0);
        press(&mut input, KeyCode::KEY_SPACE);
        let mut interceptor = enter_interceptor(&mut input).unwrap().unwrap();
        assert_eq!(source.borrow().writes, [(KeyCode::KEY_SPACE, 0, false)]);
        assert_eq!(source.borrow().grab_count, 0);
        let request = interceptor.take_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        assert_eq!(request.previous.unwrap().text().unwrap(), "q");
        let target = request.target.unwrap();
        let words = request
            .words
            .iter()
            .map(|word| {
                (
                    target.to_text(word.strokes()).unwrap(),
                    word.trailing_spaces,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            words,
            [("а".into(), 2), ("из".into(), 1), ("коробки".into(), 1)]
        );
        interceptor.finish().unwrap();
    }

    #[test]
    fn range_correction_updates_history_and_blocks_auto_switch_after_backspace() {
        let mut input = ambiguity_input("'q$'", "f 1\nbp 1");
        tap_keys(
            &mut input,
            [
                KeyCode::KEY_F,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_B,
                KeyCode::KEY_P,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_R,
            ],
        );
        let mut interceptor = enter_interceptor(&mut input).unwrap().unwrap();
        let request = interceptor.take_request().unwrap();
        assert_eq!(request.words.len(), 3);
        interceptor
            .input()
            .begin_correction(request.words.len(), request.target);
        interceptor.finish().unwrap();

        for (expected, spaces) in [("к", 0), ("из", 1), ("а", 2)] {
            let word = input.latest_word().unwrap();
            assert_eq!(word.text().unwrap(), expected);
            assert_eq!(word.trailing_spaces, spaces);
            assert!(word.chain_member.is_none());
            let length = word.entries.len();
            tap_keys(
                &mut input,
                std::iter::repeat_n(KeyCode::KEY_BACKSPACE, spaces),
            );
            assert!(input.history.is_auto_switch_blocked());
            tap_keys(&mut input, [KeyCode::KEY_Q]);
            assert!(input.switch_request().is_none());
            tap_keys(
                &mut input,
                std::iter::repeat_n(KeyCode::KEY_BACKSPACE, length + 1),
            );
        }
        assert!(input.latest_word().is_none());
        assert!(!input.history.is_auto_switch_blocked());
    }

    #[test]
    fn pattern_and_manual_requests_leave_ambiguous_context_outside_the_range() {
        use crate::switcher::SwitchMode;

        for (patterns, trigger, mode) in [
            ("'^r$'", KeyCode::KEY_R, SwitchMode::Pattern),
            ("", KeyCode::KEY_PAUSE, SwitchMode::Manual),
        ] {
            let mut input = ambiguity_input(patterns, "f 1\nbp 1");
            tap_keys(&mut input, [KeyCode::KEY_F, KeyCode::KEY_SPACE]);
            assert!(input.latest_word().unwrap().chain_member.is_some());
            if mode == SwitchMode::Manual {
                tap_keys(&mut input, [KeyCode::KEY_Q]);
            }
            tap_keys(&mut input, [trigger]);
            let request = input.switch_request().unwrap();
            assert_eq!(request.mode, mode);
            assert_eq!(request.words.len(), 1);
            assert_eq!(request.previous.unwrap().text().unwrap(), "f");
        }
    }

    #[test]
    fn word_missing_in_both_layouts_breaks_the_ambiguous_chain() {
        let mut input = ambiguity_input("", "f 1\nbp 1");
        tap_keys(&mut input, [KeyCode::KEY_F, KeyCode::KEY_SPACE]);
        assert!(input.latest_word().unwrap().chain_member.is_some());
        tap_keys(&mut input, [KeyCode::KEY_Q, KeyCode::KEY_SPACE]);
        assert!(input.latest_word().unwrap().chain_member.is_none());
        tap_keys(
            &mut input,
            [
                KeyCode::KEY_B,
                KeyCode::KEY_P,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_R,
            ],
        );
        let request = input.switch_request().unwrap();
        assert_eq!(
            request
                .words
                .iter()
                .map(|word| word.text().unwrap())
                .collect::<Vec<_>>(),
            ["bp", "r"]
        );
    }

    #[test]
    fn dead_key_in_either_layout_breaks_the_ambiguous_chain() {
        for (source_variant, target_variant) in [("intl", ""), ("", "intl")] {
            let mut input = auto_input_with_patterns("");
            let source = Arc::new(crate::layout::Layout::new("us", source_variant, "", ""));
            let target = Arc::new(crate::layout::Layout::new("us", target_variant, "", ""));
            input.accept_layout(Some(source), false);
            tap_keys(&mut input, [KeyCode::KEY_F, KeyCode::KEY_SPACE]);
            input.history.latest_word_mut().unwrap().chain_member =
                Some(ChainMember::Ambiguous { delta_score: 0.0 });
            tap_keys(&mut input, [KeyCode::KEY_APOSTROPHE, KeyCode::KEY_SPACE]);
            // Check that range selection revalidates a previously marked word.
            input.history.latest_word_mut().unwrap().chain_member =
                Some(ChainMember::Ambiguous { delta_score: 0.0 });
            tap_keys(&mut input, [KeyCode::KEY_B, KeyCode::KEY_SPACE]);
            input.history.latest_word_mut().unwrap().chain_member =
                Some(ChainMember::Ambiguous { delta_score: 0.0 });
            tap_keys(&mut input, [KeyCode::KEY_R]);
            input.pending.update_auto(
                0,
                Some((target, AutoSwitchKind::Dictionary)),
                Instant::now(),
            );

            let request = input.switch_request().unwrap();
            assert_eq!(
                request
                    .words
                    .iter()
                    .map(|word| word.text().unwrap())
                    .collect::<Vec<_>>(),
                ["b", "r"]
            );
        }
    }

    #[test]
    fn dictionary_switch_keeps_leading_punctuation_in_the_replayed_word() {
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("ru")), false);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "git 1", 5.0),
            ("ru", "", "юпше 1\nя 255", 5.0),
        ]);
        for key in [
            KeyCode::KEY_DOT,
            KeyCode::KEY_G,
            KeyCode::KEY_I,
            KeyCode::KEY_T,
        ] {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
        }
        press(&mut input, KeyCode::KEY_SPACE);
        statistics_event(&mut input, KeyCode::KEY_SPACE, 0);

        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        let target = request.target.unwrap();
        assert_eq!(target.name(), "us");
        let word = request.words.into_iter().last().unwrap();
        assert_eq!(word.text().as_deref(), Ok("юпше"));
        assert_eq!(target.to_text(word.strokes()).as_deref(), Ok(".git"));
        assert_eq!(word.trailing_spaces, 1);
    }

    #[test]
    fn dictionary_first_space_switches_a_complete_word_and_keeps_the_space() {
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "bkbfoo 1", 5.0),
            ("ru", "", "или 1", 7.0),
        ]);
        for key in [KeyCode::KEY_B, KeyCode::KEY_K, KeyCode::KEY_B] {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
            assert!(input.switch_request().is_none());
        }

        let space_pressed_at = Instant::now();
        event_at(&mut input, 0, KeyCode::KEY_SPACE, 1, space_pressed_at);
        event_at(
            &mut input,
            0,
            KeyCode::KEY_SPACE,
            0,
            space_pressed_at + Duration::from_millis(5),
        );
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Auto);
        assert_eq!(request.triggered_at, Some(space_pressed_at));
        assert_eq!(request.target.unwrap().name(), "ru");
        let word = request.words.into_iter().last().unwrap();
        assert_eq!(word.text().as_deref(), Ok("bkb"));
        assert_eq!(word.trailing_spaces, 1);

        input.pending.cancel_auto();
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.switch_request().is_none());
        assert_eq!(input.latest_word().unwrap().trailing_spaces, 2);
    }

    #[test]
    fn regex_still_wins_when_the_learned_counter_favors_the_current_layout() {
        let mut input = auto_input_with_patterns("'^a$'");
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "a 1", 0.5), ("ru", "", "ф 1", 0.5)]);
        input.remember_word("ru", "ф").unwrap();
        press(&mut input, KeyCode::KEY_A);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.target.unwrap().name(), "us");
    }

    #[test]
    fn a_learned_word_does_not_override_repeated_punctuation_protection() {
        let mut input = auto_input_with_patterns("");
        input.accept_layout(Some(layout("us")), false);
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "` 1", 0.5), ("ru", "", "ё 1", 0.5)]);
        input.remember_word("ru", "ё").unwrap();
        press(&mut input, KeyCode::KEY_GRAVE);
        assert!(input.automatic_target().is_none());
        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.automatic_target().is_none());
    }

    #[test]
    fn dictionary_preserves_repeated_punctuation_while_typing_and_at_first_space() {
        for (key, is_shift, count) in [
            (KeyCode::KEY_GRAVE, false, 1),
            (KeyCode::KEY_GRAVE, false, 3),
            (KeyCode::KEY_APOSTROPHE, false, 2),
            (KeyCode::KEY_APOSTROPHE, true, 2),
        ] {
            let mut input = auto_input_with_patterns("");
            input.accept_layout(Some(layout("us")), false);
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "apple 1", 5.0),
                ("ru", "", "ё 1\nёё 1\nёёё 1\nэ 1\nээ 1", 5.0),
            ]);
            if is_shift {
                press(&mut input, KeyCode::KEY_LEFTSHIFT);
            }
            for _ in 0..count {
                press(&mut input, key);
                statistics_event(&mut input, key, 0);
                assert!(
                    input.automatic_target().is_none(),
                    "{key:?}, shift={is_shift}"
                );
            }
            if is_shift {
                statistics_event(&mut input, KeyCode::KEY_LEFTSHIFT, 0);
            }
            press(&mut input, KeyCode::KEY_SPACE);
            assert!(
                input.automatic_target().is_none(),
                "{key:?}, shift={is_shift}"
            );
        }
    }

    #[test]
    fn dictionary_blocks_mixed_punctuation_even_with_pattern_switch_off() {
        for (first_key, second_key, is_shift) in [
            (KeyCode::KEY_GRAVE, KeyCode::KEY_SEMICOLON, false),
            (KeyCode::KEY_APOSTROPHE, KeyCode::KEY_APOSTROPHE, true),
        ] {
            let mut input = auto_input_with_patterns("");
            input.accept_layout(Some(layout("us")), false);
            input.settings.is_pattern_switch = false;
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "apple 1", 5.0),
                ("ru", "", "ёж 1\nээ 1", 5.0),
            ]);
            press(&mut input, first_key);
            statistics_event(&mut input, first_key, 0);
            assert!(input.automatic_target().is_none());
            if is_shift {
                press(&mut input, KeyCode::KEY_LEFTSHIFT);
            }
            press(&mut input, second_key);
            assert!(input.automatic_target().is_none());
            statistics_event(&mut input, second_key, 0);
            if is_shift {
                statistics_event(&mut input, KeyCode::KEY_LEFTSHIFT, 0);
            }
            press(&mut input, KeyCode::KEY_SPACE);
            assert!(input.automatic_target().is_none());
        }
    }

    #[test]
    fn punctuation_block_leaves_forward_rules_and_tokens_with_letters() {
        use crate::switcher::SwitchMode;

        for (first_key, second_key, expected_mode) in [
            (
                KeyCode::KEY_GRAVE,
                KeyCode::KEY_SEMICOLON,
                SwitchMode::Pattern,
            ),
            (KeyCode::KEY_APOSTROPHE, KeyCode::KEY_A, SwitchMode::Auto),
        ] {
            let mut input = auto_input();
            input.accept_layout(Some(layout("us")), false);
            let settings = crate::config::from_contents(
                "switch_key = 'KEY_PAUSE'\npattern_switch = true\n\
                 [[switch_rules]]\ntarget = 'ru'\npatterns = ['^ёж ?$']",
            )
            .unwrap();
            input.auto_switch = AutoSwitch::compile(&settings, |name| Ok(layout(name))).unwrap();
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "banana 1", 5.0),
                ("ru", "", "ёж 1\nэф 1", 5.0),
            ]);
            press(&mut input, first_key);
            statistics_event(&mut input, first_key, 0);
            assert!(input.automatic_target().is_none());

            press(&mut input, second_key);
            assert_eq!(input.switch_request().unwrap().mode, expected_mode);
            statistics_event(&mut input, second_key, 0);
            press(&mut input, KeyCode::KEY_SPACE);
            assert_eq!(input.switch_request().unwrap().mode, expected_mode);
        }
    }

    #[test]
    fn dictionary_does_not_boost_repeated_punctuation_in_the_alternative_layout() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "apple 1", 1.0),
            ("ru", "", "жжение 1\nмир 9", 1.0),
        ]);
        for key in [
            KeyCode::KEY_SEMICOLON,
            KeyCode::KEY_SEMICOLON,
            KeyCode::KEY_T,
            KeyCode::KEY_Y,
            KeyCode::KEY_B,
            KeyCode::KEY_T,
            KeyCode::KEY_SPACE,
        ] {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
            assert!(input.automatic_target().is_none());
        }
        assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("жжение"));
    }

    #[test]
    fn dictionary_first_space_cancels_a_prefix_only_request() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "apple 1", 5.0),
            ("ru", "", "я 1", 5.0),
        ]);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_dictionary());

        press(&mut input, KeyCode::KEY_SPACE);
        assert!(input.switch_request().is_none());
    }

    #[test]
    fn dictionary_backspace_after_first_space_restores_prefix_search() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "apple 1", 5.0),
            ("ru", "", "я 1", 5.0),
        ]);
        for key in [KeyCode::KEY_A, KeyCode::KEY_SPACE, KeyCode::KEY_BACKSPACE] {
            press(&mut input, key);
            statistics_event(&mut input, key, 0);
        }
        assert!(input.switch_request().is_none());

        press(&mut input, KeyCode::KEY_P);
        assert_eq!(input.automatic_target().unwrap().name(), "us");
        let word = input.latest_word().unwrap();
        assert_eq!(word.trailing_spaces, 0);
        assert_eq!(layout("us").to_text(word.strokes()).as_deref(), Ok("ap"));
    }

    #[test]
    fn first_space_refreshes_word_and_target_before_capture() {
        for (is_captured, pattern, expected) in [
            (true, "^A $", "gb"),
            (true, "^b $", "us"),
            (false, "^A $", "gb"),
            (false, "^b $", "us"),
        ] {
            let (mut input, sender) = channel_input();
            input.pending.cancel();
            input.accept_layout(Some(layout("ru")), false);
            let settings = crate::config::from_contents(&format!(
                "switch_key = 'KEY_PAUSE'\npattern_switch = true\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['{pattern}']",
            )).unwrap();
            input.auto_switch = AutoSwitch::compile(&settings, |name| Ok(layout(name))).unwrap();
            // A distinct synthetic regex target makes a replaced request observable.
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 1\napple 1", 5.0),
                ("ru", "", "я 1", 5.0),
            ]);
            press(&mut input, KeyCode::KEY_LEFTSHIFT);
            press(&mut input, KeyCode::KEY_A);
            assert!(matches!(
                input.next_request().unwrap(),
                InputRequest::Switch
            ));
            assert!(input.pending.is_dictionary());
            for (key, value) in [
                (KeyCode::KEY_LEFTSHIFT, 0),
                (KeyCode::KEY_A, 0),
                (KeyCode::KEY_SPACE, 1),
                (KeyCode::KEY_SPACE, 0),
            ] {
                sender.send(InputEventMessage::Keyboard(
                    0,
                    InputEvent::new(EventType::KEY.0, key.0, value),
                ));
            }
            let mut interception: Box<dyn CorrectionInterceptor> = if is_captured {
                Box::new(Interceptor::enter(&mut input).unwrap().unwrap())
            } else {
                enter_interceptor(&mut input).unwrap().unwrap()
            };
            let request = interception.take_request().unwrap();
            assert_eq!(request.target.unwrap().name(), expected);
            let expected_mode = if expected == "gb" {
                crate::switcher::SwitchMode::Pattern
            } else {
                crate::switcher::SwitchMode::Auto
            };
            assert_eq!(request.mode, expected_mode);
            let word = request.words.into_iter().last().unwrap();
            assert_eq!(word.trailing_spaces, 1);
            assert!(word.entries[0].stroke.shift);
            interception.finish().unwrap();
        }
    }

    #[test]
    fn current_layout_patterns_block_only_dictionary_requests() {
        use crate::switcher::SwitchMode;

        for (pattern_switch, current_pattern, forward_pattern, before_space, after_space) in [
            (true, "^ф$", "^z$", None, Some(SwitchMode::Auto)),
            (true, "^ф $", "^z$", Some(SwitchMode::Auto), None),
            (
                true,
                "^ф ?$",
                "^a ?$",
                Some(SwitchMode::Pattern),
                Some(SwitchMode::Pattern),
            ),
            (
                false,
                "^ф ?$",
                "^z$",
                Some(SwitchMode::Auto),
                Some(SwitchMode::Auto),
            ),
        ] {
            let mut input = auto_input();
            let settings = crate::config::from_contents(&format!(
                "switch_key = 'KEY_PAUSE'\npattern_switch = {pattern_switch}\n\
                 [[switch_rules]]\ntarget = 'ru'\npatterns = ['{current_pattern}']\n\
                 [[switch_rules]]\ntarget = 'us'\npatterns = ['{forward_pattern}']",
            ))
            .unwrap();
            input.settings.is_pattern_switch = settings.pattern_switch;
            input.auto_switch = AutoSwitch::compile(&settings, |name| Ok(layout(name))).unwrap();
            input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 1", 5.0),
                ("ru", "", "я 1", 5.0),
            ]);
            press(&mut input, KeyCode::KEY_A);
            assert_eq!(
                input.switch_request().map(|request| request.mode),
                before_space,
                "{current_pattern}, {forward_pattern}, pattern_switch={pattern_switch}",
            );
            press(&mut input, KeyCode::KEY_SPACE);
            assert_eq!(
                input.switch_request().map(|request| request.mode),
                after_space,
                "{current_pattern}, {forward_pattern}, pattern_switch={pattern_switch}",
            );
        }
    }

    #[test]
    fn pattern_and_dictionary_switch_flags_are_independent() {
        let path = std::env::temp_dir().join(format!(
            "true-switcher-switch-flags-{}.txt",
            std::process::id(),
        ));
        std::fs::write(&path, "a 1\nb 1\nя 1\n").unwrap();
        for (is_pattern_switch, is_auto_switch) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let settings = crate::config::from_contents(&format!(
                r#"
switch_key = "KEY_PAUSE"
pattern_switch = {is_pattern_switch}
auto_switch = {is_auto_switch}

[[switch_rules]]
target = "us"
patterns = ['^[ac]$']

[[dict]]
target = "us"
file = {path:?}
threshold = 5.0

[[dict]]
target = "ru"
file = {path:?}
threshold = 7.0
"#,
            ))
            .unwrap();
            let mut input = auto_input();
            let resolve = |name: &str| Ok(layout(name));
            input.settings = (&settings).into();
            input.auto_switch = AutoSwitch::compile(&settings, resolve).unwrap();
            input.auto_switch.dictionaries =
                crate::dictionary::DictionaryAutoSwitch::load(&settings, resolve, || Ok(2))
                    .unwrap();

            for (key, has_pattern_match, has_dictionary_match) in [
                (KeyCode::KEY_A, true, true),
                (KeyCode::KEY_B, false, true),
                (KeyCode::KEY_C, true, false),
            ] {
                let expects_pattern = is_pattern_switch && has_pattern_match;
                let expects_dictionary = is_auto_switch && has_dictionary_match && !expects_pattern;
                let (_, messages) = crate::test_logging::capture(|| press(&mut input, key));
                assert_eq!(
                    input
                        .automatic_target()
                        .as_deref()
                        .map(crate::layout::Layout::name),
                    (expects_pattern || expects_dictionary).then_some("us"),
                    "pattern_switch={is_pattern_switch}, auto_switch={is_auto_switch}, key={key:?}",
                );
                assert_eq!(input.pending.is_dictionary(), expects_dictionary);
                let expected_mode = if expects_pattern {
                    Some(crate::switcher::SwitchMode::Pattern)
                } else if expects_dictionary {
                    Some(crate::switcher::SwitchMode::Auto)
                } else {
                    None
                };
                assert_eq!(
                    input.switch_request().map(|request| request.mode),
                    expected_mode
                );
                assert_eq!(
                    messages
                        .iter()
                        .filter(|message| message.starts_with("dict switch score: "))
                        .count(),
                    usize::from(is_auto_switch),
                );
                press(&mut input, KeyCode::KEY_BACKSPACE);
                statistics_event(&mut input, KeyCode::KEY_BACKSPACE, 0);
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn toggles_cancel_only_the_pending_switch_of_the_disabled_kind() {
        let mut input = auto_input_with_patterns("'^c$'");
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "b 1", 1.0), ("ru", "", "я 1", 1.0)]);
        let toggle = |input: &mut Input, setting| {
            let (reply, result) = std::sync::mpsc::channel();
            input
                .process_message(InputEventMessage::ToggleSetting(setting, reply), None)
                .unwrap();
            result.try_recv().unwrap()
        };

        press(&mut input, KeyCode::KEY_B);
        assert!(input.pending.is_dictionary());
        assert!(!toggle(&mut input, RuntimeSetting::PatternSwitch).is_pattern_switch);
        assert!(input.pending.is_dictionary());
        assert!(!toggle(&mut input, RuntimeSetting::AutoSwitch).is_auto_switch);
        assert!(input.pending.is_none());

        assert!(toggle(&mut input, RuntimeSetting::PatternSwitch).is_pattern_switch);
        press(&mut input, KeyCode::KEY_BACKSPACE);
        statistics_event(&mut input, KeyCode::KEY_BACKSPACE, 0);
        press(&mut input, KeyCode::KEY_C);
        assert!(input.pending.is_auto());
        assert!(!input.pending.is_dictionary());
        assert!(toggle(&mut input, RuntimeSetting::AutoSwitch).is_auto_switch);
        assert!(input.pending.is_auto());
        assert!(!toggle(&mut input, RuntimeSetting::PatternSwitch).is_pattern_switch);
        assert!(input.pending.is_none());
    }

    fn logged_dictionary_scores(
        input: &mut Input,
        key: KeyCode,
        level: log::LevelFilter,
    ) -> Vec<String> {
        let (_, messages) = crate::test_logging::capture_at(level, || press(input, key));
        messages
            .into_iter()
            .filter_map(|message| {
                message
                    .strip_prefix("dict switch score: ")
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn logs_dictionary_score_for_text_and_first_space() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "a 53\nap 1\nc 1\nd 45", 5.0),
            ("ru", "", "ф 4\nфз 1\nи 1\nя 94", 7.0),
        ]);

        for (key, expected) in [
            (KeyCode::KEY_A, Some(" +3.43 =  4.32 -  0.89")),
            (KeyCode::KEY_SPACE, Some(" +3.73 =  4.64 -  0.92")),
            (KeyCode::KEY_SPACE, None),
            (KeyCode::KEY_BACKSPACE, None),
            (KeyCode::KEY_BACKSPACE, None),
            (KeyCode::KEY_P, Some(" +0.00 =  6.64 -  6.64")),
            (KeyCode::KEY_SPACE, Some(" +0.00 =  6.64 -  6.64")),
            (KeyCode::KEY_BACKSPACE, None),
            (KeyCode::KEY_SPACE, Some(" +0.00 =  6.64 -  6.64")),
            (KeyCode::KEY_B, Some("  -inf =  6.64 -   inf")),
            (KeyCode::KEY_SPACE, Some("  -inf =  6.64 -   inf")),
            (KeyCode::KEY_C, Some("  +inf =   inf -  6.64")),
            (KeyCode::KEY_SPACE, Some("  +inf =   inf -  6.64")),
            (KeyCode::KEY_X, Some(" +0.00 (both layout scores missing)")),
        ] {
            let scores = logged_dictionary_scores(&mut input, key, log::LevelFilter::Debug);
            assert_eq!(scores, expected.as_slice(), "{key:?}");
        }
    }

    #[test]
    fn logs_learned_dictionary_correction_as_an_integer() {
        for (learned_current, learned_alternative, expected) in [
            (1, 1, " +0.00 =  8.00 -  8.00"),
            (0, 1, " +1.00 =  8.00 -  8.00 +  1"),
            (1, 0, " -1.00 =  8.00 -  8.00 -  1"),
            (0, 100, "+100.00 =  8.00 -  8.00 + 100"),
        ] {
            let mut input = auto_input_with_patterns("");
            let mut dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", "a 1\nb 255", 200.0),
                ("ru", "", "ф 1\nи 255", 200.0),
            ]);
            for _ in 0..learned_current {
                dictionaries.learned.increment("ru", "ф").unwrap();
            }
            for _ in 0..learned_alternative {
                dictionaries.learned.increment("us", "a").unwrap();
            }
            input.auto_switch.dictionaries = dictionaries;

            let scores =
                logged_dictionary_scores(&mut input, KeyCode::KEY_A, log::LevelFilter::Debug);
            assert_eq!(scores, [expected]);
        }
    }

    #[test]
    fn logs_the_same_winning_key_used_for_the_dictionary_switch() {
        use KeyCode as K;

        for (count, expected, is_switch) in [
            (6, " +5.00 = 12.00 -  7.00", true),
            (8, " +3.00 = 18.00 -  7.00 -  8", false),
        ] {
            let mut input = auto_input_with_patterns("");
            input.accept_layout(Some(layout("us")), false);
            let mut dictionaries = crate::dictionary::test_dictionaries([
                ("us", "", ";git 1\nz 4095", 4.0),
                ("ru", "", "жпше 1\nя 127", 4.0),
            ]);
            for _ in 0..count {
                dictionaries.learned.increment("us", "git").unwrap();
            }
            input.auto_switch.dictionaries = dictionaries;
            for key in [K::KEY_SEMICOLON, K::KEY_G, K::KEY_I, K::KEY_T] {
                press(&mut input, key);
            }
            let scores =
                logged_dictionary_scores(&mut input, K::KEY_SPACE, log::LevelFilter::Debug);
            assert_eq!(scores, [expected]);
            assert_eq!(input.pending.is_dictionary(), is_switch);
        }
    }

    #[test]
    fn filters_dictionary_score_logs_and_reports_unavailable_comparisons() {
        use log::LevelFilter;

        let mut input = auto_input_with_patterns("");
        assert!(
            logged_dictionary_scores(&mut input, KeyCode::KEY_A, LevelFilter::Debug).is_empty()
        );

        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "a 1", 5.0), ("ru", "", "ф 1", 7.0)]);
        input.accept_layout(Some(layout("de")), false);
        for (level, expected) in [
            (LevelFilter::Off, None),
            (LevelFilter::Error, None),
            (LevelFilter::Warn, None),
            (LevelFilter::Info, None),
            (LevelFilter::Debug, Some("unavailable")),
            (LevelFilter::Trace, Some("unavailable")),
        ] {
            let scores = logged_dictionary_scores(&mut input, KeyCode::KEY_A, level);
            assert_eq!(scores, expected.as_slice(), "{level:?}");
        }
    }

    #[test]
    fn dictionary_backspace_restores_missing_prefix_and_previous_word() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "", "cats 7", 5.0),
            ("ru", "", "я 1", 5.0),
        ]);
        let keypress = |input: &mut Input, key| {
            press(input, key);
            statistics_event(input, key, 0);
        };
        for key in [KeyCode::KEY_C, KeyCode::KEY_A, KeyCode::KEY_T] {
            keypress(&mut input, key);
            assert!(input.pending.is_dictionary());
        }
        for key in [
            KeyCode::KEY_DOT,
            KeyCode::KEY_1,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_Z,
        ] {
            keypress(&mut input, key);
        }
        let us = layout("us");
        let prefix_score = |input: &Input| {
            input
                .auto_switch
                .dictionaries
                .comparison(input.latest_word().unwrap(), &us, false)
                .unwrap()
                .current_base_score
        };
        keypress(&mut input, KeyCode::KEY_BACKSPACE);
        let word = input.latest_word().unwrap();
        assert_eq!(us.to_text(word.strokes()).as_deref(), Ok("cat.1"));
        assert_eq!(word.trailing_spaces, 1);
        keypress(&mut input, KeyCode::KEY_BACKSPACE); // Space
        assert_eq!(prefix_score(&input), f64::NEG_INFINITY);
        keypress(&mut input, KeyCode::KEY_BACKSPACE); // Digit
        assert_eq!(prefix_score(&input), f64::NEG_INFINITY);
        keypress(&mut input, KeyCode::KEY_BACKSPACE); // Punctuation
        assert_eq!(prefix_score(&input), 0.0);
        assert!(input.pending.is_none());
        keypress(&mut input, KeyCode::KEY_S);
        assert_eq!(input.automatic_target().unwrap().name(), "us");
    }

    #[test]
    fn dictionary_and_regex_share_priority_and_correction_block() {
        let mut input = auto_input_with_patterns("'^a$'");
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("us", "", "ab 2", 5.0), ("ru", "", "я 2", 5.0)]);
        press(&mut input, KeyCode::KEY_A);
        assert_eq!(input.automatic_target().unwrap().name(), "us");
        assert!(!input.pending.is_dictionary());
        input.begin_correction(1, Some(layout("us")));
        press(&mut input, KeyCode::KEY_B);
        assert!(input.pending.is_none());

        input.reset_history();
        input.auto_switch.dictionaries =
            crate::dictionary::test_dictionaries([("ru", "", "с 1", 5.0), ("us", "", "a 1", 5.0)]);
        press(&mut input, KeyCode::KEY_C);
        assert!(input.pending.is_dictionary());
        input.begin_correction(1, Some(layout("ru")));
        input.auto_switch = auto_input_with_patterns("'^ca$'").auto_switch;
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_none());
    }

    #[test]
    fn source_dead_key_cancels_auto_and_prevents_manual_even_after_backspace() {
        let (mut input, sender) = channel_input();
        input.pending.cancel();
        input.accept_layout(
            Some(Arc::new(crate::layout::Layout::new("us", "intl", "", ""))),
            false,
        );
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("ru", "", "фа 1\nфэ 1", 5.0),
            ("us", "intl", "b 1", 5.0),
        ]);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_dictionary());
        sender.send(InputEventMessage::Keyboard(
            0,
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_APOSTROPHE.0, 1),
        ));
        assert!(Interceptor::enter(&mut input).unwrap().is_none());
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_BACKSPACE);
        press(&mut input, KeyCode::KEY_PAUSE);
        assert!(input.switch_request().is_none());
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_SPACE);
        press(&mut input, KeyCode::KEY_B);
        assert!(!input.latest_word().unwrap().has_dead_key);
    }

    #[test]
    fn dictionary_conversion_failure_survives_until_the_stroke_is_removed() {
        let mut input = auto_input_with_patterns("");
        input.auto_switch.dictionaries = crate::dictionary::test_dictionaries([
            ("us", "intl", "apple 1", 5.0),
            ("ru", "", "я 1", 5.0),
        ]);
        press(&mut input, KeyCode::KEY_A);
        assert!(input.pending.is_dictionary());
        press(&mut input, KeyCode::KEY_APOSTROPHE);
        press(&mut input, KeyCode::KEY_P);
        let word = input.latest_word().unwrap();
        assert!(!word.has_dead_key);
        assert!(
            input
                .auto_switch
                .dictionaries
                .comparison(word, &layout("ru"), false)
                .is_none()
        );
        assert!(input.pending.is_none());
        for _ in 0..2 {
            press(&mut input, KeyCode::KEY_BACKSPACE);
            statistics_event(&mut input, KeyCode::KEY_BACKSPACE, 0);
        }
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_P);
        assert!(input.pending.is_dictionary());
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
            input.process_available_events(None).unwrap();
            assert!(input.automatic_target().is_none());
            assert!(input.pending.is_none());
            assert!(!input.is_auto_request());
            assert!(input.switch_request().is_none());
        }
        let mut input = auto_input();
        for key in [KeyCode::KEY_SLASH, KeyCode::KEY_N, KeyCode::KEY_A] {
            press(&mut input, key);
        }
        input.pending.cancel();
        press(&mut input, KeyCode::KEY_BACKSPACE);
        assert!(input.pending.is_none());
        assert!(input.automatic_target().is_none());
    }

    #[test]
    fn typing_after_backspace_creates_a_new_automatic_request() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_SLASH);
        press(&mut input, KeyCode::KEY_N);
        assert!(input.pending.is_auto());
        press(&mut input, KeyCode::KEY_BACKSPACE);
        assert!(input.switch_request().is_none());
        press(&mut input, KeyCode::KEY_N);
        let request = input.switch_request().unwrap();
        assert_eq!(request.mode, crate::switcher::SwitchMode::Pattern);
        assert_eq!(request.target.unwrap().name(), "us");
    }

    #[test]
    fn discovery_drains_channel_and_ignores_switch_requests() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        input.pending.cancel();
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
            .send(InputEventMessage::Keyboard(
                0,
                InputEvent::new(EventType::RELATIVE.0, 0, 1),
            ));
        input.wait_for_keyboard_discovery(TEST_STEP * 3).unwrap();
        assert!(input.events.available().unwrap().is_none());
        assert_eq!(input.pending.source(), None);
        assert!(!input.pending.is_original_key_held());
        assert!(
            input
                .keyboard_input
                .is_key_held(0, KeyCode::KEY_LEFTSHIFT.0)
        );
        assert!(!input.keyboard_input.is_key_held(0, KeyCode::KEY_PAUSE.0));
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
        assert_eq!(input.pending.source(), Some(0));
    }

    #[test]
    fn discovery_propagates_keyboard_event_loss() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard::default()));
        let mut input = simulated_input(std::slice::from_ref(&source));
        source
            .borrow()
            .events
            .sender
            .as_ref()
            .unwrap()
            .send(InputEventMessage::Keyboard(
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
    fn default_interceptor_releases_all_sources_without_grabbing_or_waiting() {
        let keys = [KeyCode::KEY_A, KeyCode::KEY_LEFTCTRL, KeyCode::KEY_PAUSE]
            .into_iter()
            .chain(LOCK_KEYS)
            .map(|key| key.0)
            .collect();
        let first = Rc::new(RefCell::new(SimulatedKeyboard {
            held: keys,
            ..Default::default()
        }));
        let second = Rc::new(RefCell::new(SimulatedKeyboard {
            held: BTreeSet::from([KeyCode::KEY_B.0, KeyCode::KEY_RIGHTSHIFT.0]),
            ..Default::default()
        }));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        let mut interceptor = enter_interceptor(&mut input).unwrap().unwrap();
        assert!(!interceptor.read_available().unwrap());
        assert!(interceptor.input().keyboard_input.hold_deadline().is_none());
        for source in [&first, &second] {
            let keyboard = source.borrow();
            assert!(keyboard.held.is_empty());
            assert_eq!(keyboard.grab_count, 0);
            assert!(
                keyboard
                    .writes
                    .iter()
                    .all(|(_, value, captured)| *value == 0 && !captured)
            );
        }
        assert_eq!(first.borrow().writes.len(), 6);
        assert_eq!(second.borrow().writes.len(), 2);
        press(interceptor.input(), KeyCode::KEY_C);
        interceptor.finish().unwrap();
        assert!(input.keyboard_input.is_key_held(0, KeyCode::KEY_C.0));
        assert!(!input.correction.is_active());
        assert_eq!(first.borrow().ungrab_count, 0);
    }

    #[test]
    fn both_interceptors_preserve_early_history_update_through_common_interface() {
        for is_captured in [false, true] {
            let (mut input, sender) = channel_input();
            input.accept_layout(Some(layout("us")), false);
            press(&mut input, KeyCode::KEY_A);
            statistics_event(&mut input, KeyCode::KEY_A, 0);
            let mut interceptor: Box<dyn CorrectionInterceptor> = if is_captured {
                Box::new(Interceptor::enter(&mut input).unwrap().unwrap())
            } else {
                Box::new(interception::UncapturedInterceptor::enter(&mut input).unwrap())
            };
            interceptor.input().begin_correction(1, Some(layout("ru")));
            assert_eq!(
                interceptor.input().latest_word().unwrap().text().as_deref(),
                Ok("ф")
            );
            assert!(interceptor.input().history.is_auto_switch_blocked());
            sender.send(InputEventMessage::CorrectionFinished {
                result: Ok(()),
                layout: Some(layout("ru")),
            });
            assert!(interceptor.wait_for_correction().unwrap().unwrap().is_ok());
            assert!(interceptor.input().correction.is_active());
            interceptor.finish().unwrap();
            assert!(!input.correction.is_active());
            assert_eq!(input.latest_word().unwrap().text().as_deref(), Ok("ф"));
            assert!(input.history.is_auto_switch_blocked());
        }
    }

    #[test]
    fn uncaptured_text_uses_new_layout_and_completion_does_not_rewrite_new_word() {
        let (mut input, sender) = channel_input();
        input.accept_layout(Some(layout("us")), false);
        press(&mut input, KeyCode::KEY_A);
        let mut interceptor = interception::UncapturedInterceptor::enter(&mut input).unwrap();
        interceptor.input().begin_correction(1, Some(layout("ru")));
        press(interceptor.input(), KeyCode::KEY_B);
        assert_eq!(
            interceptor.input().latest_word().unwrap().text().as_deref(),
            Ok("фи")
        );
        assert!(interceptor.input().history.is_auto_switch_blocked());
        press(interceptor.input(), KeyCode::KEY_SPACE);
        press(interceptor.input(), KeyCode::KEY_C);
        assert!(!interceptor.input().history.is_auto_switch_blocked());
        sender.send(InputEventMessage::CorrectionFinished {
            result: Ok(()),
            layout: Some(layout("ru")),
        });
        assert!(!interceptor.read_available().unwrap());
        assert!(interceptor.wait_for_correction().unwrap().unwrap().is_ok());
        assert_eq!(interceptor.input().latest_word().unwrap().entries.len(), 1);
        assert_eq!(
            interceptor.input().latest_word().unwrap().text().as_deref(),
            Ok("с")
        );
        assert!(!interceptor.input().history.is_auto_switch_blocked());
    }

    #[test]
    fn uncaptured_correction_ignores_switch_requests_but_records_text() {
        let mut input = auto_input();
        press(&mut input, KeyCode::KEY_A);
        let mut interceptor = interception::UncapturedInterceptor::enter(&mut input).unwrap();
        interceptor.input().begin_correction(1, Some(layout("us")));
        for key in [
            KeyCode::KEY_SPACE,
            KeyCode::KEY_SLASH,
            KeyCode::KEY_N,
            KeyCode::KEY_A,
            KeyCode::KEY_PAUSE,
        ] {
            press(interceptor.input(), key);
            statistics_event(interceptor.input(), key, 0);
        }
        assert!(!interceptor.read_available().unwrap());
        assert!(interceptor.input().pending.is_none());
        assert!(interceptor.input().automatic_target().is_none());
        assert_eq!(
            interceptor.input().latest_word().unwrap().text().as_deref(),
            Ok("/na")
        );
        Box::new(interceptor).finish().unwrap();
        assert!(input.pending.is_none());
        press(&mut input, KeyCode::KEY_PAUSE);
        assert!(input.pending.is_some());
    }

    #[test]
    fn uncaptured_correction_cancels_on_focus_navigation_and_long_hold() {
        for event in [
            InputEventMessage::FocusChanged,
            InputEventMessage::PointerPressed,
        ] {
            let (mut input, sender) = channel_input();
            let mut interceptor = interception::UncapturedInterceptor::enter(&mut input).unwrap();
            sender.send(event);
            assert!(interceptor.read_available().unwrap());
            assert!(interceptor.input().correction.is_cancelled());
        }
        for key in [KeyCode::KEY_LEFT, KeyCode::KEY_TAB, KeyCode::KEY_A] {
            let (mut input, _) = channel_input();
            let mut interceptor = interception::UncapturedInterceptor::enter(&mut input).unwrap();
            let now = Instant::now();
            event_at(interceptor.input(), 0, key, 1, now);
            if key == KeyCode::KEY_A {
                assert!(!interceptor.input().take_cancel_request());
                interceptor
                    .input()
                    .update_back_off(now + HOLD_TIMEOUT + TEST_STEP);
            }
            assert!(interceptor.input().take_cancel_request(), "{key:?}");
        }
    }

    #[test]
    fn uncaptured_release_failure_attempts_other_keys_and_clears_operation_state() {
        let source = Rc::new(RefCell::new(SimulatedKeyboard {
            held: BTreeSet::from([KeyCode::KEY_A.0, KeyCode::KEY_B.0]),
            write_failures: BTreeMap::from([((KeyCode::KEY_A.0, 0), 1)]),
            ..Default::default()
        }));
        let mut input = simulated_input(std::slice::from_ref(&source));
        assert!(interception::UncapturedInterceptor::enter(&mut input).is_err());
        assert!(!input.correction.is_active());
        assert_eq!(source.borrow().writes.len(), 2);
        assert_eq!(source.borrow().grab_count, 0);
        assert!(!source.borrow().held.contains(&KeyCode::KEY_B.0));
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
            on_grab: Some(InputEventMessage::Keyboard(
                0,
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_KPEQUAL.0, 1),
            )),
            ..Default::default()
        }));
        let second = Rc::new(RefCell::new(SimulatedKeyboard {
            is_grab_failed: true,
            ..Default::default()
        }));
        let mut input = simulated_input(&[first.clone(), second.clone()]);
        assert!(Interceptor::enter(&mut input).is_err());
        assert!(!input.correction.is_active());
        let first = first.borrow();
        let repairs: Vec<_> = first
            .writes
            .iter()
            .filter(|(key, _, _)| matches!(*key, KeyCode::KEY_A | KeyCode::KEY_KPEQUAL))
            .copied()
            .collect();
        assert_eq!(
            repairs,
            [
                (KeyCode::KEY_A, 1, true),
                (KeyCode::KEY_KPEQUAL, 1, true),
                (KeyCode::KEY_A, 0, false),
                (KeyCode::KEY_KPEQUAL, 0, false),
            ]
        );
        assert!(!first.is_grabbed);
        let second = second.borrow();
        assert!(!second.is_grabbed);
        assert!(
            second
                .writes
                .iter()
                .all(|(key, _, _)| *key != KeyCode::KEY_KPEQUAL)
        );
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
        assert!(!input.keyboard_input.is_any_grabbed());
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
        assert!(!input.keyboard_input.is_any_grabbed());
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
