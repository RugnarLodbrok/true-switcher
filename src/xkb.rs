use std::{cell::RefCell, collections::HashMap, env, fmt, fs, io, path::PathBuf};

use evdev::KeyCode;
use xkbcommon::xkb;

use crate::{layout::Layout, word::Stroke};

const EVDEV_KEYCODE_OFFSET: u32 = 8;
const XKB_DIRECTORY: &str = "/usr/share/X11/xkb";
const XKB_LAYOUT_REGISTRY: &str = "rules/evdev.lst";
const XKB_RULES: &str = "evdev";

thread_local! {
    static KEYMAPS: RefCell<HashMap<Layout, xkb::Keymap>> = RefCell::new(HashMap::new());
}

#[cfg(test)]
thread_local! {
    pub(crate) static CONVERSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static COMPILATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;

#[derive(Debug, PartialEq, Eq)]
pub enum ConversionError {
    UnknownLayout { key: KeyCode },
    InvalidParameter { field: &'static str },
    KeymapCompilationFailed,
    ShiftUnavailable { key: KeyCode },
    DeadKey { key: KeyCode },
    NoTextForKey { key: KeyCode },
    ControlCharacter { key: KeyCode },
}

impl fmt::Display for ConversionError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLayout { key } => write!(output, "unknown saved layout for {key:?}"),
            Self::InvalidParameter { field } => {
                write!(output, "NUL byte in layout parameter {field}")
            }
            Self::KeymapCompilationFailed => write!(output, "XKB keymap compilation failed"),
            Self::ShiftUnavailable { key } => write!(output, "Shift mask unavailable for {key:?}"),
            Self::DeadKey { key } => write!(output, "dead key for {key:?}"),
            Self::NoTextForKey { key } => write!(output, "no text for {key:?}"),
            Self::ControlCharacter { key } => write!(output, "control character for {key:?}"),
        }
    }
}

impl std::error::Error for ConversionError {}

pub fn to_text<'a>(
    layout: &Layout,
    strokes: impl IntoIterator<Item = &'a Stroke>,
) -> Result<String, ConversionError> {
    #[cfg(test)]
    CONVERSIONS.with(|count| count.set(count.get() + 1));
    strokes_text(layout, strokes).inspect_err(|error| {
        log::error!("true-switcher: text conversion: {error}; layout={layout:?}");
    })
}

fn compile_keymap(layout: &Layout) -> Result<xkb::Keymap, ConversionError> {
    #[cfg(test)]
    COMPILATIONS.with(|count| count.set(count.get() + 1));
    for (field, value) in [
        ("name", layout.name()),
        ("variant", layout.variant()),
        ("model", layout.model()),
        ("options", layout.options()),
    ] {
        if value.contains('\0') {
            return Err(ConversionError::InvalidParameter { field });
        }
    }
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(
        &context,
        XKB_RULES,
        layout.model(),
        layout.name(),
        layout.variant(),
        Some(layout.options().to_owned()),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or(ConversionError::KeymapCompilationFailed)
}

fn shift_mask(stroke: &Stroke, shift_index: u32) -> Result<u32, ConversionError> {
    if stroke.shift {
        1_u32
            .checked_shl(shift_index)
            .ok_or(ConversionError::ShiftUnavailable { key: stroke.key })
    } else {
        Ok(0)
    }
}

pub(crate) fn strokes_text<'a>(
    layout: &Layout,
    strokes: impl IntoIterator<Item = &'a Stroke>,
) -> Result<String, ConversionError> {
    with_keymap(layout, |keymap| convert_with_keymap(keymap, strokes))
}

pub(crate) fn stroke_text(layout: &Layout, stroke: &Stroke) -> Result<String, ConversionError> {
    strokes_text(layout, [stroke])
}

pub(crate) fn validate_keymap(layout: &Layout) -> Result<(), ConversionError> {
    with_keymap(layout, |_| Ok(()))
}

pub(crate) fn has_dead_key<'a>(
    layout: &Layout,
    strokes: impl IntoIterator<Item = &'a Stroke>,
) -> Result<bool, ConversionError> {
    with_keymap(layout, |keymap| {
        let shift_index = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let mut state = xkb::State::new(keymap);
        for stroke in strokes {
            let key = prepare_stroke(&mut state, shift_index, stroke)?;
            if is_dead_key(&state, key) {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

fn with_keymap<T>(
    layout: &Layout,
    convert: impl FnOnce(&xkb::Keymap) -> Result<T, ConversionError>,
) -> Result<T, ConversionError> {
    KEYMAPS.with(|maps| {
        let mut maps = maps.borrow_mut();
        if !maps.contains_key(layout) {
            maps.insert(layout.clone(), compile_keymap(layout)?);
        }
        convert(&maps[layout])
    })
}

fn prepare_stroke(
    state: &mut xkb::State,
    shift_index: u32,
    stroke: &Stroke,
) -> Result<xkb::Keycode, ConversionError> {
    state.update_mask(shift_mask(stroke, shift_index)?, 0, 0, 0, 0, 0);
    Ok(xkb::Keycode::new(
        u32::from(stroke.key.0) + EVDEV_KEYCODE_OFFSET,
    ))
}

fn is_dead_key(state: &xkb::State, key: xkb::Keycode) -> bool {
    use xkb::keysyms::*;
    state.key_get_syms(key).iter().any(|symbol| {
        matches!(
            symbol.raw(),
            KEY_dead_grave..=KEY_dead_currency
                | KEY_dead_a..=KEY_dead_greek
                | KEY_dead_lowline..=KEY_dead_longsolidusoverlay
        )
    })
}

fn convert_with_keymap<'a>(
    keymap: &xkb::Keymap,
    strokes: impl IntoIterator<Item = &'a Stroke>,
) -> Result<String, ConversionError> {
    let shift_index = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    let mut state = xkb::State::new(keymap);
    let mut text = String::new();
    for stroke in strokes {
        let key = prepare_stroke(&mut state, shift_index, stroke)?;
        if is_dead_key(&state, key) {
            return Err(ConversionError::DeadKey { key: stroke.key });
        }
        let value = state.key_get_utf8(key);
        if value.is_empty() {
            return Err(ConversionError::NoTextForKey { key: stroke.key });
        }
        if value.chars().any(char::is_control) {
            return Err(ConversionError::ControlCharacter { key: stroke.key });
        }
        text.push_str(&value);
    }
    Ok(text)
}

pub fn validate_layout_identifier(name: &str) -> io::Result<()> {
    let root = env::var_os("XKB_CONFIG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(XKB_DIRECTORY));
    let registry = fs::read_to_string(root.join(XKB_LAYOUT_REGISTRY))?;
    validate_identifier(&registry, name)
}

fn validate_identifier(registry: &str, name: &str) -> io::Result<()> {
    let mut is_layout_section = false;
    for line in registry.lines().map(str::trim) {
        if line.starts_with('!') {
            is_layout_section = line == "! layout";
        } else if is_layout_section && line.split_whitespace().next() == Some(name) {
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "Invalid XKB layout identifier: {name:?}"
    )))
}
