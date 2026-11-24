use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{
    AbsoluteAxisCode, EventType, InputEvent, KeyCode, PropType, RelativeAxisCode,
    SynchronizationCode, raw_stream::RawDevice,
};
use nix::sys::inotify::AddWatchFlags;

use super::InputEventExt;
use super::device_watch::{self, DirectoryWatcher, INPUT_DIRECTORY, Identity};

use crate::input_events::{InputEventMessage, InputEventSender};

const TAP_DURATION: Duration = Duration::from_millis(250);
const TAP_TRAVEL_MM: i32 = 3;
const FALLBACK_TRAVEL_DIVISOR: i32 = 70;

pub(super) fn start(sender: InputEventSender) {
    let observer_sender = sender.clone();
    if let Err(error) = thread::Builder::new()
        .name("input-pointers".into())
        .spawn(move || {
            if let Err(error) = watch(&observer_sender) {
                log::warn!("Pointer observer stopped: {error}");
                observer_sender.send(InputEventMessage::PointerEventsLost);
            }
        })
    {
        log::warn!("Cannot start pointer observer: {error}");
        sender.send(InputEventMessage::PointerEventsLost);
    }
}

fn watch(sender: &InputEventSender) -> io::Result<()> {
    let watcher = DirectoryWatcher::new(Path::new(INPUT_DIRECTORY))?;
    let mut devices = BTreeMap::new();
    refresh(&mut devices, sender)?;
    loop {
        let events = watcher.read_events()?;
        let mut is_relevant = false;
        for event in events {
            if device_watch::is_watch_lost(event.mask) {
                return Err(io::Error::other("pointer directory watch lost"));
            }
            if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                sender.send(InputEventMessage::PointerEventsLost);
                is_relevant = true;
            }
            is_relevant |= event
                .name
                .is_some_and(|name| device_watch::is_device_name(&name));
        }
        if is_relevant {
            refresh(&mut devices, sender)?;
        }
    }
}

fn refresh(devices: &mut BTreeMap<PathBuf, Identity>, sender: &InputEventSender) -> io::Result<()> {
    let mut found = BTreeMap::new();
    for path in device_watch::device_paths(Path::new(INPUT_DIRECTORY))? {
        let path = path?;
        match open_pointer(&path) {
            Ok(Some((identity, device))) => {
                if devices.get(&path) != Some(&identity) {
                    if devices.contains_key(&path) {
                        sender.send(InputEventMessage::PointerEventsLost);
                    }
                    start_reader(path.clone(), device, sender.clone());
                }
                found.insert(path, identity);
            }
            Ok(None) => {}
            Err(error) => log::debug!("Cannot observe {}: {error}", path.display()),
        }
    }
    if devices.keys().any(|path| !found.contains_key(path)) {
        sender.send(InputEventMessage::PointerEventsLost);
    }
    *devices = found;
    Ok(())
}

fn open_pointer(path: &Path) -> io::Result<Option<(Identity, RawDevice)>> {
    let (identity, device) = device_watch::open_device(path)?;
    let Some(keys) = device.supported_keys() else {
        return Ok(None);
    };
    let is_touchpad = keys.contains(KeyCode::BTN_TOUCH)
        && device.properties().contains(PropType::POINTER)
        && !device.properties().contains(PropType::DIRECT);
    let is_mouse =
        (KeyCode::BTN_LEFT.0..KeyCode::BTN_TRIGGER.0).any(|code| keys.contains(KeyCode::new(code)));
    if is_mouse || is_touchpad {
        Ok(Some((identity, device)))
    } else {
        Ok(None)
    }
}

fn start_reader(path: PathBuf, mut device: RawDevice, sender: InputEventSender) {
    let is_touchpad = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::BTN_TOUCH))
        && device.properties().contains(PropType::POINTER);
    let tap = TapDetector::new(&device);
    let reader_path = path.clone();
    let reader_sender = sender.clone();
    if let Err(error) = thread::Builder::new()
        .name(format!("pointer-{}", path.display()))
        .spawn(move || {
            let mut tap = tap;
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            if is_syn_dropped(event) {
                                reader_sender.send(InputEventMessage::PointerEventsLost);
                                tap.cancel();
                            } else if is_button_press(event) {
                                tap.cancel();
                                if !reader_sender.send(InputEventMessage::PointerPressed) {
                                    return;
                                }
                            } else if is_touchpad
                                && tap.process(event)
                                && !reader_sender.send(InputEventMessage::PointerPressed)
                            {
                                return;
                            } else if is_mouse_motion(event, is_touchpad)
                                && !reader_sender
                                    .send(InputEventMessage::MouseMoved(event.instant()))
                            {
                                return;
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        log::warn!("Pointer read failed {}: {error}", reader_path.display());
                        reader_sender.send(InputEventMessage::PointerEventsLost);
                        return;
                    }
                }
            }
        })
    {
        log::warn!("Cannot start reader {}: {error}", path.display());
        sender.send(InputEventMessage::PointerEventsLost);
    }
}

fn is_mouse_motion(event: InputEvent, is_touchpad: bool) -> bool {
    !is_touchpad
        && event.event_type() == EventType::RELATIVE
        && matches!(
            RelativeAxisCode(event.code()),
            RelativeAxisCode::REL_X | RelativeAxisCode::REL_Y
        )
        && event.value() != 0
}

fn is_syn_dropped(event: InputEvent) -> bool {
    event.event_type() == EventType::SYNCHRONIZATION
        && event.code() == SynchronizationCode::SYN_DROPPED.0
}

fn is_button_press(event: InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && (KeyCode::BTN_LEFT.0..KeyCode::BTN_TRIGGER.0).contains(&event.code())
        && event.value() == 1
}

#[derive(Default)]
struct Contact {
    x: Option<i32>,
    y: Option<i32>,
    start_x: Option<i32>,
    start_y: Option<i32>,
}

struct TapDetector {
    started: Option<Instant>,
    is_valid: bool,
    contacts: BTreeMap<i32, Contact>,
    slot: i32,
    has_mt: bool,
    x_limit: i32,
    y_limit: i32,
}

impl TapDetector {
    fn new(device: &RawDevice) -> Self {
        let mut x_limit = 0;
        let mut y_limit = 0;
        if let Ok(axes) = device.get_absinfo() {
            for (axis, info) in axes {
                let limit = if info.resolution() > 0 {
                    info.resolution() * TAP_TRAVEL_MM
                } else {
                    (info.maximum() - info.minimum()) / FALLBACK_TRAVEL_DIVISOR
                }
                .max(1);
                match axis {
                    AbsoluteAxisCode::ABS_MT_POSITION_X | AbsoluteAxisCode::ABS_X => {
                        x_limit = limit
                    }
                    AbsoluteAxisCode::ABS_MT_POSITION_Y | AbsoluteAxisCode::ABS_Y => {
                        y_limit = limit
                    }
                    _ => {}
                }
            }
        }
        Self {
            started: None,
            is_valid: false,
            contacts: BTreeMap::new(),
            slot: 0,
            has_mt: device
                .supported_absolute_axes()
                .is_some_and(|axes| axes.contains(AbsoluteAxisCode::ABS_MT_SLOT)),
            x_limit,
            y_limit,
        }
    }

    fn cancel(&mut self) {
        self.is_valid = false;
        self.started = None;
        self.contacts.clear();
    }

    fn process(&mut self, event: InputEvent) -> bool {
        if event.event_type() == EventType::KEY {
            if event.code() == KeyCode::BTN_TOUCH.0 {
                if event.value() == 1 {
                    self.started = Some(Instant::now());
                    self.is_valid = self.x_limit > 0 && self.y_limit > 0;
                    for contact in self.contacts.values_mut() {
                        contact.start_x = contact.x;
                        contact.start_y = contact.y;
                    }
                } else if event.value() == 0 {
                    let is_tap = self.is_valid
                        && self
                            .started
                            .is_some_and(|start| start.elapsed() <= TAP_DURATION)
                        && self
                            .contacts
                            .values()
                            .any(|contact| contact.start_x.is_some() && contact.start_y.is_some());
                    self.started = None;
                    self.contacts.clear();
                    return is_tap;
                }
            }
        } else if event.event_type() == EventType::ABSOLUTE {
            self.position(event);
        } else if event.event_type() == EventType::RELATIVE
            && matches!(
                RelativeAxisCode(event.code()),
                RelativeAxisCode::REL_WHEEL
                    | RelativeAxisCode::REL_HWHEEL
                    | RelativeAxisCode::REL_WHEEL_HI_RES
                    | RelativeAxisCode::REL_HWHEEL_HI_RES
            )
        {
            self.cancel();
        }
        false
    }

    fn position(&mut self, event: InputEvent) {
        let axis = AbsoluteAxisCode(event.code());
        if axis == AbsoluteAxisCode::ABS_MT_SLOT {
            self.slot = event.value();
            return;
        }
        if axis == AbsoluteAxisCode::ABS_MT_TRACKING_ID {
            if event.value() >= 0 {
                self.contacts.insert(self.slot, Contact::default());
            } else if self.started.is_none() {
                self.contacts.remove(&self.slot);
            }
            return;
        }
        let slot = if self.has_mt { self.slot } else { 0 };
        let contact = self.contacts.entry(slot).or_default();
        let is_x = matches!(
            axis,
            AbsoluteAxisCode::ABS_MT_POSITION_X | AbsoluteAxisCode::ABS_X
        );
        let is_y = matches!(
            axis,
            AbsoluteAxisCode::ABS_MT_POSITION_Y | AbsoluteAxisCode::ABS_Y
        );
        if is_x {
            if self.started.is_some() {
                let start = contact.start_x.get_or_insert(event.value());
                self.is_valid &= (event.value() - *start).abs() <= self.x_limit;
            }
            contact.x = Some(event.value());
        } else if is_y {
            if self.started.is_some() {
                let start = contact.start_y.get_or_insert(event.value());
                self.is_valid &= (event.value() - *start).abs() <= self.y_limit;
            }
            contact.y = Some(event.value());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> TapDetector {
        TapDetector {
            started: None,
            is_valid: false,
            contacts: BTreeMap::new(),
            slot: 0,
            has_mt: true,
            x_limit: 30,
            y_limit: 30,
        }
    }

    fn key(code: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.0, value)
    }

    fn abs(code: AbsoluteAxisCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, code.0, value)
    }

    #[test]
    fn mouse_motion_requires_nonzero_relative_xy_and_excludes_touchpads() {
        for axis in [RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y] {
            for delta in [-1, 1] {
                let event = InputEvent::new(EventType::RELATIVE.0, axis.0, delta);
                assert!(is_mouse_motion(event, false));
                assert!(!is_mouse_motion(event, true));
            }
            assert!(!is_mouse_motion(
                InputEvent::new(EventType::RELATIVE.0, axis.0, 0),
                false
            ));
        }
        for axis in [RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL] {
            assert!(!is_mouse_motion(
                InputEvent::new(EventType::RELATIVE.0, axis.0, 1),
                false
            ));
        }
        assert!(!is_mouse_motion(abs(AbsoluteAxisCode::ABS_X, 1), false));
    }

    #[test]
    fn buttons_only_report_press() {
        for code in [
            KeyCode::BTN_LEFT,
            KeyCode::BTN_RIGHT,
            KeyCode::BTN_MIDDLE,
            KeyCode::BTN_SIDE,
        ] {
            assert!(is_button_press(key(code, 1)));
            assert!(!is_button_press(key(code, 0)));
        }
        assert!(!is_button_press(key(KeyCode::BTN_TOUCH, 1)));
        assert!(!is_button_press(InputEvent::new(
            EventType::RELATIVE.0,
            0,
            1
        )));
        assert!(!is_button_press(InputEvent::new(
            EventType::RELATIVE.0,
            evdev::RelativeAxisCode::REL_WHEEL.0,
            1
        )));
    }

    #[test]
    fn taps_include_multiple_fingers_and_ignore_motion() {
        let mut tap = detector();
        for slot in 0..2 {
            tap.process(abs(AbsoluteAxisCode::ABS_MT_SLOT, slot));
            tap.process(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, slot + 10));
            tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100));
            tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 200));
        }
        assert!(!tap.process(key(KeyCode::BTN_TOUCH, 1)));
        assert!(!tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 115)));
        assert!(tap.process(key(KeyCode::BTN_TOUCH, 0)));

        tap.process(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 20));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 200));
        tap.process(key(KeyCode::BTN_TOUCH, 1));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 150));
        assert!(!tap.process(key(KeyCode::BTN_TOUCH, 0)));
    }

    #[test]
    fn physical_button_suppresses_touch_release() {
        let mut tap = detector();
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 200));
        tap.process(key(KeyCode::BTN_TOUCH, 1));
        tap.cancel();
        assert!(!tap.process(key(KeyCode::BTN_TOUCH, 0)));
    }

    #[test]
    fn long_contact_is_not_a_tap() {
        let mut tap = detector();
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 200));
        tap.process(key(KeyCode::BTN_TOUCH, 1));
        tap.started = Some(Instant::now() - TAP_DURATION - Duration::from_millis(1));
        assert!(!tap.process(key(KeyCode::BTN_TOUCH, 0)));
    }

    #[test]
    fn scroll_cancels_tap_even_with_little_finger_travel() {
        let mut tap = detector();
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100));
        tap.process(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 200));
        tap.process(key(KeyCode::BTN_TOUCH, 1));
        tap.process(InputEvent::new(
            EventType::RELATIVE.0,
            RelativeAxisCode::REL_WHEEL_HI_RES.0,
            15,
        ));
        assert!(!tap.process(key(KeyCode::BTN_TOUCH, 0)));
    }
}
