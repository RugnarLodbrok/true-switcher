use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use evdev::{
    AttributeSet, EventType, InputEvent, KeyCode, SynchronizationCode, SynchronizationEvent,
    uinput::{VirtualDevice, VirtualDeviceBuilder},
};

use crate::history::is_text_key;
use crate::word::{Stroke, Word};

const DEVICE_PHYS: &std::ffi::CStr = c"true-switcher/keyboard";
const DEVICE_NAME: &str = "true-switcher-keyboard";
// Set to false to manually restore one emit call per key event.
const IS_BATCH_OUTPUT_ENABLED: bool = true;

pub(crate) fn is_own_keyboard(physical_path: Option<&str>, name: Option<&str>) -> bool {
    physical_path.is_some_and(|path| path.as_bytes() == DEVICE_PHYS.to_bytes())
        || name == Some(DEVICE_NAME)
}

fn keyboard_builder() -> io::Result<VirtualDeviceBuilder<'static>> {
    match VirtualDevice::builder()?
        .name(DEVICE_NAME)
        .with_phys(DEVICE_PHYS)
    {
        Ok(builder) => Ok(builder),
        Err(error) => {
            log::warn!(
                "true-switcher: cannot set uinput physical_path: {error}; \
                 falling back to name {DEVICE_NAME}"
            );
            // `with_phys` consumes the builder; its failure closes that descriptor.
            Ok(VirtualDevice::builder()?.name(DEVICE_NAME))
        }
    }
}

/// Physical key output through Linux uinput, independent of KDE and layouts.
/// Keep the device alive between operations. The caller must wait until the
/// desktop has discovered it and physical modifiers have been released before
/// starting output. Coordination with live input belongs to the correction flow.
pub struct KeyboardOutput {
    device: VirtualDevice,
}

impl KeyboardOutput {
    pub fn new() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in KeyCode::KEY_1.0..=KeyCode::KEY_102ND.0 {
            let key = KeyCode::new(code);
            if is_text_key(key) {
                keys.insert(key);
            }
        }
        for key in [
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_LEFTSHIFT,
        ] {
            keys.insert(key);
        }

        let device =
            (|| keyboard_builder()?.with_keys(&keys)?.build())().map_err(|error: io::Error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot create uinput keyboard: {error}"),
                )
            })?;
        Ok(Self { device })
    }

    /// Send one Backspace tap per character to remove, including trailing spaces.
    pub fn erase(&mut self, words: &[Word], is_cancelled: &AtomicBool) -> io::Result<()> {
        self.strokes(erase_strokes(words), is_cancelled)
    }

    /// Replay each stored stroke as a tap, including repeats, then all spaces.
    /// This does not change the active layout or the stored word.
    pub fn replay(&mut self, words: &[Word], is_cancelled: &AtomicBool) -> io::Result<()> {
        self.strokes(replay_strokes(words)?, is_cancelled)
    }

    fn strokes(
        &mut self,
        strokes: impl IntoIterator<Item = Stroke>,
        is_cancelled: &AtomicBool,
    ) -> io::Result<()> {
        emit_cancellable_strokes(strokes, is_cancelled, |stroke| {
            if IS_BATCH_OUTPUT_ENABLED {
                return emit_batch([stroke], |events| self.device.emit(events));
            }
            emit_stroke(stroke, |key, value| {
                self.device
                    .emit(&[InputEvent::new(EventType::KEY.0, key.0, value)])
            })
        })
    }
}

fn emit_cancellable_strokes(
    strokes: impl IntoIterator<Item = Stroke>,
    is_cancelled: &AtomicBool,
    mut emit: impl FnMut(Stroke) -> io::Result<()>,
) -> io::Result<()> {
    for stroke in strokes {
        if is_cancelled.load(Ordering::Relaxed) {
            break;
        }
        emit(stroke)?;
    }
    Ok(())
}

fn emit_batch(
    strokes: impl IntoIterator<Item = Stroke>,
    mut emit: impl FnMut(&[InputEvent]) -> io::Result<()>,
) -> io::Result<()> {
    let mut events = Vec::new();
    let mut keys = AttributeSet::<KeyCode>::new();
    emit_strokes(strokes, |key, value| {
        // Keep the legacy frame boundaries; emit supplies the final SYN_REPORT.
        if !events.is_empty() {
            events.push(*SynchronizationEvent::new(
                SynchronizationCode::SYN_REPORT,
                0,
            ));
        }
        events.push(InputEvent::new(EventType::KEY.0, key.0, value));
        keys.insert(key);
        Ok(())
    })?;
    if events.is_empty() {
        return Ok(());
    }

    let result = emit(&events);
    if result.is_err() {
        // A failed batch may have reached the kernel only in part. Try every
        // possible release separately, even if earlier cleanup writes fail.
        for key in keys.iter() {
            let _ = emit(&[InputEvent::new(EventType::KEY.0, key.0, 0)]);
        }
    }
    result
}

fn erase_strokes(words: &[Word]) -> impl Iterator<Item = Stroke> {
    let count = words
        .iter()
        .map(|word| word.entries.len() + word.trailing_spaces)
        .sum();
    std::iter::repeat_n(
        Stroke {
            key: KeyCode::KEY_BACKSPACE,
            shift: false,
        },
        count,
    )
}

fn replay_strokes(words: &[Word]) -> io::Result<impl Iterator<Item = Stroke> + '_> {
    // Validate the whole range before replaying any of it.
    if words
        .iter()
        .flat_map(Word::strokes)
        .any(|stroke| !is_text_key(stroke.key))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "word contains a non-text key",
        ));
    }
    let space = Stroke {
        key: KeyCode::KEY_SPACE,
        shift: false,
    };
    Ok(words.iter().flat_map(move |word| {
        word.strokes()
            .copied()
            .chain(std::iter::repeat_n(space, word.trailing_spaces))
    }))
}

fn emit_strokes(
    strokes: impl IntoIterator<Item = Stroke>,
    mut emit: impl FnMut(KeyCode, i32) -> io::Result<()>,
) -> io::Result<()> {
    for stroke in strokes {
        emit_stroke(stroke, &mut emit)?;
    }
    Ok(())
}

fn emit_stroke(
    stroke: Stroke,
    mut emit: impl FnMut(KeyCode, i32) -> io::Result<()>,
) -> io::Result<()> {
    let result = (|| {
        if stroke.shift {
            emit(KeyCode::KEY_LEFTSHIFT, 1)?;
        }
        emit(stroke.key, 1)?;
        emit(stroke.key, 0)?;
        if stroke.shift {
            emit(KeyCode::KEY_LEFTSHIFT, 0)?;
        }
        Ok(())
    })();
    if result.is_err() {
        // A write can fail after a press reached the kernel. Try both releases
        // even if one fails, preserving the original error for the caller.
        let _ = emit(stroke.key, 0);
        if stroke.shift {
            let _ = emit(KeyCode::KEY_LEFTSHIFT, 0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_stops_before_next_synthetic_stroke() {
        let is_cancelled = AtomicBool::new(false);
        let mut emitted = Vec::new();
        emit_cancellable_strokes(
            [KeyCode::KEY_BACKSPACE, KeyCode::KEY_A].map(|key| Stroke { key, shift: false }),
            &is_cancelled,
            |stroke| {
                emitted.push(stroke.key);
                is_cancelled.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(emitted, [KeyCode::KEY_BACKSPACE]);
    }

    fn assert_batch_matches_legacy(strokes: Vec<Stroke>) {
        let syn = *SynchronizationEvent::new(SynchronizationCode::SYN_REPORT, 0);
        let mut expected = Vec::new();
        emit_strokes(strokes.iter().copied(), |key, value| {
            expected.push(InputEvent::new(EventType::KEY.0, key.0, value));
            expected.push(syn);
            Ok(())
        })
        .unwrap();

        let mut actual = Vec::new();
        let mut calls = 0;
        emit_batch(strokes, |events| {
            calls += 1;
            assert_ne!(events.last(), Some(&syn));
            actual.extend_from_slice(events);
            // Model the final SYN_REPORT appended by VirtualDevice::emit.
            actual.push(syn);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(actual, expected);
    }

    #[test]
    fn batch_replay_preserves_every_legacy_frame_with_shift_repeats_and_spaces() {
        let word = Word {
            entries: vec![
                Stroke {
                    key: KeyCode::KEY_A,
                    shift: true,
                },
                Stroke {
                    key: KeyCode::KEY_A,
                    shift: true,
                },
                Stroke {
                    key: KeyCode::KEY_COMMA,
                    shift: false,
                },
            ]
            .into_iter()
            .map(|stroke| crate::word::WordStroke {
                stroke,
                layout: None,
            })
            .collect(),
            trailing_spaces: 3,
            ..Default::default()
        };
        assert_batch_matches_legacy(
            replay_strokes(std::slice::from_ref(&word))
                .unwrap()
                .collect(),
        );
    }

    #[test]
    fn batch_erase_preserves_separate_press_and_release_frames_for_each_backspace() {
        assert_batch_matches_legacy(vec![
            Stroke {
                key: KeyCode::KEY_BACKSPACE,
                shift: false
            };
            5
        ]);
    }

    #[test]
    fn empty_batch_emits_nothing() {
        emit_batch(std::iter::empty(), |_| panic!("empty output must not emit")).unwrap();
    }

    #[test]
    fn failed_batch_releases_all_possible_keys_even_when_cleanup_fails() {
        let strokes = [
            Stroke {
                key: KeyCode::KEY_A,
                shift: true,
            },
            Stroke {
                key: KeyCode::KEY_B,
                shift: false,
            },
            Stroke {
                key: KeyCode::KEY_A,
                shift: true,
            },
        ];
        let mut calls = Vec::new();
        let error = emit_batch(strokes, |events| {
            calls.push(events.to_vec());
            Err(io::Error::other(format!("write {}", calls.len())))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "write 1");
        assert_eq!(calls.len(), 4);
        for key in [KeyCode::KEY_A, KeyCode::KEY_B, KeyCode::KEY_LEFTSHIFT] {
            assert!(calls[1..].contains(&vec![InputEvent::new(EventType::KEY.0, key.0, 0)]));
        }
    }

    #[test]
    fn own_keyboard_is_identified_without_excluding_other_virtual_keyboards() {
        assert!(is_own_keyboard(Some("true-switcher/keyboard"), None));
        assert!(is_own_keyboard(None, Some("true-switcher-keyboard")));
        assert!(is_own_keyboard(
            Some("true-switcher/keyboard"),
            Some("true-switcher-keyboard")
        ));
        assert!(is_own_keyboard(
            Some("other/keyboard"),
            Some("true-switcher-keyboard")
        ));
        assert!(!is_own_keyboard(
            Some("other/keyboard"),
            Some("other-keyboard")
        ));
        assert!(!is_own_keyboard(None, None));
        assert!(!is_own_keyboard(None, Some("true-switcher-keyboard-extra")));
        assert!(!is_own_keyboard(
            None,
            Some("prefix-true-switcher-keyboard")
        ));
    }

    #[test]
    fn erase_and_replay_cover_every_word_and_space_in_the_range() {
        let word = |key, trailing_spaces| Word {
            entries: vec![crate::word::WordStroke {
                stroke: Stroke { key, shift: false },
                layout: None,
            }],
            trailing_spaces,
            ..Default::default()
        };
        let words = [
            word(KeyCode::KEY_F, 2),
            word(KeyCode::KEY_B, 1),
            word(KeyCode::KEY_R, 1),
        ];
        assert_eq!(
            erase_strokes(&words)
                .map(|stroke| stroke.key)
                .collect::<Vec<_>>(),
            [KeyCode::KEY_BACKSPACE; 7],
        );
        assert_eq!(
            replay_strokes(&words)
                .unwrap()
                .map(|stroke| stroke.key)
                .collect::<Vec<_>>(),
            [
                KeyCode::KEY_F,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_B,
                KeyCode::KEY_SPACE,
                KeyCode::KEY_R,
                KeyCode::KEY_SPACE
            ],
        );
    }

    #[test]
    fn replay_preserves_word_and_spaces_and_deletion_matches_their_length() {
        let word = Word {
            entries: vec![
                Stroke {
                    key: KeyCode::KEY_COMMA,
                    shift: true,
                };
                2
            ]
            .into_iter()
            .map(|stroke| crate::word::WordStroke {
                stroke,
                layout: None,
            })
            .collect(),
            trailing_spaces: 3,
            ..Default::default()
        };
        let mut events = Vec::new();
        emit_strokes(
            replay_strokes(std::slice::from_ref(&word)).unwrap(),
            |key, value| {
                events.push((key, value));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|(_, value)| *value == 1)
                .copied()
                .collect::<Vec<_>>(),
            [
                (KeyCode::KEY_LEFTSHIFT, 1),
                (KeyCode::KEY_COMMA, 1),
                (KeyCode::KEY_LEFTSHIFT, 1),
                (KeyCode::KEY_COMMA, 1),
                (KeyCode::KEY_SPACE, 1),
                (KeyCode::KEY_SPACE, 1),
                (KeyCode::KEY_SPACE, 1),
            ]
        );
        events.clear();
        emit_strokes(
            std::iter::repeat_n(
                Stroke {
                    key: KeyCode::KEY_BACKSPACE,
                    shift: false,
                },
                word.entries.len() + word.trailing_spaces,
            ),
            |key, value| {
                events.push((key, value));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events,
            [(KeyCode::KEY_BACKSPACE, 1), (KeyCode::KEY_BACKSPACE, 0)].repeat(5)
        );
        assert_eq!(word.entries.len(), 2);
        assert_eq!(word.trailing_spaces, 3);
        assert_eq!(replay_strokes(&[Word::default()]).unwrap().count(), 0);
    }

    #[test]
    fn invalid_word_is_rejected_before_any_stroke_can_be_emitted() {
        let word = Word {
            entries: vec![
                Stroke {
                    key: KeyCode::KEY_A,
                    shift: false,
                },
                Stroke {
                    key: KeyCode::KEY_ENTER,
                    shift: false,
                },
            ]
            .into_iter()
            .map(|stroke| crate::word::WordStroke {
                stroke,
                layout: None,
            })
            .collect(),
            trailing_spaces: 0,
            ..Default::default()
        };
        assert!(
            matches!(replay_strokes(std::slice::from_ref(&word)), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn shifted_repeat_and_following_plain_key_are_separate_taps() {
        let mut events = Vec::new();
        for (key, shift) in [
            (KeyCode::KEY_A, true),
            (KeyCode::KEY_A, true),
            (KeyCode::KEY_B, false),
        ] {
            emit_stroke(Stroke { key, shift }, |key, value| {
                events.push((key, value));
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            events,
            [
                (KeyCode::KEY_LEFTSHIFT, 1),
                (KeyCode::KEY_A, 1),
                (KeyCode::KEY_A, 0),
                (KeyCode::KEY_LEFTSHIFT, 0),
                (KeyCode::KEY_LEFTSHIFT, 1),
                (KeyCode::KEY_A, 1),
                (KeyCode::KEY_A, 0),
                (KeyCode::KEY_LEFTSHIFT, 0),
                (KeyCode::KEY_B, 1),
                (KeyCode::KEY_B, 0),
            ]
        );
    }

    #[test]
    fn failed_output_attempts_to_release_key_and_shift_and_returns_original_error() {
        for failure in 0..4 {
            let mut events = Vec::new();
            let error = emit_stroke(
                Stroke {
                    key: KeyCode::KEY_A,
                    shift: true,
                },
                |key, value| {
                    events.push((key, value));
                    if events.len() > failure {
                        Err(io::Error::other(format!("write {}", events.len())))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.to_string(), format!("write {}", failure + 1));
            assert_eq!(
                &events[events.len() - 2..],
                [(KeyCode::KEY_A, 0), (KeyCode::KEY_LEFTSHIFT, 0)]
            );
        }
    }
}
