use super::*;

#[test]
fn accepts_only_identifiers_in_xkb_layout_section() {
    let registry = "! model\n pc105 PC\n! layout\n us English\n gb British\n ru Russian\n! variant\n phonetic ru: Phonetic\n";
    for name in ["us", "gb", "ru"] {
        validate_identifier(registry, name).unwrap();
    }
    for name in [
        "en",
        "us,ru",
        "ru(phonetic)",
        "phonetic",
        "pc105",
        "",
        "../us",
    ] {
        assert!(validate_identifier(registry, name).is_err());
    }
}

fn strokes() -> Vec<Stroke> {
    [KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_B]
        .into_iter()
        .enumerate()
        .map(|(index, key)| Stroke {
            key,
            shift: index == 0,
        })
        .collect()
}

#[test]
fn converts_shift_and_layout_without_changing_physical_keys() {
    let us = Layout {
        name: "us".into(),
        ..Layout::default()
    };
    let ru = Layout {
        name: "ru".into(),
        ..Layout::default()
    };
    assert_eq!(us.to_text(&strokes()).as_deref(), Ok("Ghb"));
    assert_eq!(ru.to_text(&strokes()).as_deref(), Ok("При"));
    assert_eq!(
        us.to_text(&[Stroke {
            key: KeyCode::KEY_COMMA,
            shift: true
        }])
        .as_deref(),
        Ok("<")
    );
}

fn layout() -> Layout {
    Layout {
        name: "us".into(),
        ..Default::default()
    }
}

fn stroke(key: KeyCode) -> Stroke {
    Stroke { key, shift: false }
}

#[test]
fn identifies_each_invalid_parameter_before_calling_xkb() {
    for field in ["name", "variant", "model", "options"] {
        let mut layout = layout();
        match field {
            "name" => layout.name.push('\0'),
            "variant" => layout.variant.push('\0'),
            "model" => layout.model.push('\0'),
            "options" => layout.options.push('\0'),
            _ => unreachable!(),
        }
        assert_eq!(
            layout.to_text(&strokes()),
            Err(ConversionError::InvalidParameter { field })
        );
    }
}

#[test]
fn distinguishes_compilation_empty_text_and_control_character_errors() {
    let invalid = Layout {
        variant: "true-switcher-nonexistent-variant".into(),
        ..layout()
    };
    assert_eq!(
        invalid.to_text(&strokes()),
        Err(ConversionError::KeymapCompilationFailed)
    );
    for (key, expected) in [
        (
            KeyCode::KEY_LEFT,
            ConversionError::NoTextForKey {
                key: KeyCode::KEY_LEFT,
            },
        ),
        (
            KeyCode::KEY_ENTER,
            ConversionError::ControlCharacter {
                key: KeyCode::KEY_ENTER,
            },
        ),
    ] {
        assert_eq!(
            layout().to_text(&[stroke(KeyCode::KEY_A), stroke(key)]),
            Err(expected)
        );
    }
}

#[test]
fn dead_key_aborts_conversion_instead_of_losing_part_of_the_word() {
    let international = Layout {
        variant: "intl".into(),
        ..layout()
    };
    assert_eq!(
        international.to_text(&[
            stroke(KeyCode::KEY_A),
            stroke(KeyCode::KEY_APOSTROPHE),
            stroke(KeyCode::KEY_E)
        ]),
        Err(ConversionError::NoTextForKey {
            key: KeyCode::KEY_APOSTROPHE
        })
    );
}

#[test]
fn requires_a_valid_shift_mask_only_for_shifted_strokes() {
    let mut stroke = stroke(KeyCode::KEY_A);
    assert_eq!(shift_mask(&stroke, xkb::MOD_INVALID), Ok(0));
    stroke.shift = true;
    assert_eq!(
        shift_mask(&stroke, xkb::MOD_INVALID),
        Err(ConversionError::ShiftUnavailable { key: stroke.key })
    );
}

struct TestLogger;

thread_local! {
    static MESSAGES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl log::Log for TestLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        MESSAGES.with(|messages| messages.borrow_mut().push(record.args().to_string()));
    }

    fn flush(&self) {}
}

fn logged_error(
    convert: impl FnOnce() -> Result<String, ConversionError>,
) -> (ConversionError, String) {
    static LOGGER: TestLogger = TestLogger;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
    MESSAGES.with(|messages| messages.borrow_mut().clear());
    let error = convert().unwrap_err();
    let messages = MESSAGES.with(|messages| messages.take());
    assert_eq!(
        messages.len(),
        1,
        "one log entry per conversion failure: {messages:?}"
    );
    (error, messages.into_iter().next().unwrap())
}

#[test]
fn logs_errors_once_when_propagated_through_layout_and_word() {
    use crate::word::{Word, WordStroke};
    use std::sync::Arc;

    let mut word = Word {
        entries: vec![WordStroke {
            stroke: stroke(KeyCode::KEY_A),
            layout: None,
        }],
        ..Default::default()
    };
    let (error, message) = logged_error(|| word.text());
    assert_eq!(
        error,
        ConversionError::UnknownLayout {
            key: KeyCode::KEY_A
        }
    );
    assert!(message.contains("KEY_A"));

    for (layout, key, context) in [
        (
            Layout {
                options: "\0".into(),
                ..layout()
            },
            KeyCode::KEY_A,
            "options",
        ),
        (
            Layout {
                variant: "true-switcher-nonexistent-variant".into(),
                ..layout()
            },
            KeyCode::KEY_A,
            "compilation",
        ),
        (layout(), KeyCode::KEY_LEFT, "KEY_LEFT"),
        (layout(), KeyCode::KEY_ENTER, "KEY_ENTER"),
    ] {
        word.entries[0] = WordStroke {
            stroke: stroke(key),
            layout: Some(Arc::new(layout)),
        };
        let (_, message) = logged_error(|| word.text());
        assert!(message.contains(context), "{message}");
        assert!(message.contains("layout=Layout"), "{message}");
    }
}
