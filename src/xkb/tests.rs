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
    let us = Layout::new("us", "", "", "");
    let ru = Layout::new("ru", "", "", "");
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
    Layout::new("us", "", "", "")
}

fn stroke(key: KeyCode) -> Stroke {
    Stroke { key, shift: false }
}

#[test]
fn caches_keymaps_by_all_parameters_per_thread_and_retries_failures() {
    std::thread::spawn(|| {
        let layouts = [
            Layout::new("us", "", "", ""),
            Layout::new("gb", "", "", ""),
            Layout::new("us", "intl", "", ""),
            Layout::new("us", "", "pc105", ""),
            Layout::new("us", "", "", "grp:alt_shift_toggle"),
        ];
        for layout in &layouts {
            layout.to_text(&[stroke(KeyCode::KEY_A)]).unwrap();
            layout.to_text(&[stroke(KeyCode::KEY_A)]).unwrap();
        }
        assert_eq!(COMPILATIONS.with(|count| count.get()), layouts.len());
        assert_eq!(KEYMAPS.with(|maps| maps.borrow().len()), layouts.len());

        let invalid = Layout::new("us", "true-switcher-nonexistent-variant", "", "");
        for _ in 0..2 {
            assert_eq!(
                invalid.to_text(&strokes()),
                Err(ConversionError::KeymapCompilationFailed)
            );
        }
        assert_eq!(COMPILATIONS.with(|count| count.get()), layouts.len() + 2);
        assert_eq!(KEYMAPS.with(|maps| maps.borrow().len()), layouts.len());

        std::thread::spawn(|| {
            assert_eq!(KEYMAPS.with(|maps| maps.borrow().len()), 0);
            layout().to_text(&strokes()).unwrap();
            assert_eq!(COMPILATIONS.with(|count| count.get()), 1);
        })
        .join()
        .unwrap();
        assert_eq!(COMPILATIONS.with(|count| count.get()), layouts.len() + 2);
    })
    .join()
    .unwrap();
}

#[test]
fn identifies_each_invalid_parameter_before_calling_xkb() {
    for field in ["name", "variant", "model", "options"] {
        let layout = match field {
            "name" => Layout::new("\0", "", "", ""),
            "variant" => Layout::new("us", "\0", "", ""),
            "model" => Layout::new("us", "", "\0", ""),
            "options" => Layout::new("us", "", "", "\0"),
            _ => unreachable!(),
        };
        assert_eq!(
            layout.to_text(&strokes()),
            Err(ConversionError::InvalidParameter { field })
        );
    }
}

#[test]
fn distinguishes_compilation_empty_text_and_control_character_errors() {
    let invalid = Layout::new("us", "true-switcher-nonexistent-variant", "", "");
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
    let international = Layout::new("us", "intl", "", "");
    assert_eq!(
        international.to_text(&[
            stroke(KeyCode::KEY_A),
            stroke(KeyCode::KEY_APOSTROPHE),
            stroke(KeyCode::KEY_E)
        ]),
        Err(ConversionError::DeadKey {
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

fn logged_error(
    convert: impl FnOnce() -> Result<String, ConversionError>,
) -> (ConversionError, String) {
    let (result, messages) = crate::test_logging::capture(convert);
    let error = result.unwrap_err();
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
        (Layout::new("us", "", "", "\0"), KeyCode::KEY_A, "options"),
        (
            Layout::new("us", "true-switcher-nonexistent-variant", "", ""),
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
