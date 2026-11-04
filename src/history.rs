use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evdev::KeyCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub key: KeyCode,
    pub shift: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Word {
    pub strokes: Vec<Stroke>,
    pub trailing_spaces: usize,
    pub layouts: Vec<Option<Arc<crate::layout::Layout>>>,
}

impl Word {
    pub fn text(&self) -> Option<String> {
        if self.layouts.len() != self.strokes.len() {
            return None;
        }
        let mut text = String::new();
        let mut start = 0;
        while start < self.strokes.len() {
            let layout = self.layouts[start].as_ref()?;
            let end = (start + 1..self.strokes.len())
                .find(|&index| self.layouts[index].as_ref() != Some(layout))
                .unwrap_or(self.strokes.len());
            text.push_str(&layout.text(&self.strokes[start..end])?);
            start = end;
        }
        Some(text)
    }
}

impl fmt::Display for Word {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, stroke) in self.strokes.iter().enumerate() {
            if index > 0 {
                write!(output, " ")?;
            }
            if stroke.shift {
                write!(output, "Shift+")?;
            }
            write!(output, "{:?}", stroke.key)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct History {
    words: Vec<Word>,
    layouts: crate::layout::LayoutTracker,
    // Track modifiers per keyboard: releasing Shift on one device must not
    // release a Shift still held on another one.
    modifiers: HashSet<(PathBuf, KeyCode)>,
}

impl History {
    pub fn with_layouts(layouts: crate::layout::LayoutTracker) -> Self {
        Self {
            layouts,
            ..Self::default()
        }
    }

    pub fn previous_word(&self) -> Option<&Word> {
        self.words
            .len()
            .checked_sub(2)
            .and_then(|index| self.words.get(index))
    }

    pub fn set_latest_layout(&mut self, layout: Option<Arc<crate::layout::Layout>>) {
        if let Some(word) = self.words.last_mut() {
            word.layouts = vec![layout; word.strokes.len()];
        }
    }

    pub fn latest_word(&self) -> Option<&Word> {
        self.words.last()
    }

    /// Returns lines for stdout on completion, restoration or reset.
    /// A repeat is another stroke, not a held key in the stored word.
    pub fn process(&mut self, device: &Path, key: KeyCode, value: i32) -> Vec<String> {
        if !matches!(value, 0..=2) {
            return Vec::new();
        }
        if is_modifier(key) {
            if value == 0 {
                self.modifiers.remove(&(device.to_owned(), key));
            } else {
                self.modifiers.insert((device.to_owned(), key));
            }
            return Vec::new();
        }
        if value == 0 || self.modifiers.iter().any(|(_, key)| !is_shift(*key)) {
            return Vec::new();
        }

        match key {
            KeyCode::KEY_TAB | KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER => {
                let mut lines = Vec::new();
                if let Some(word) = self.words.last().filter(|word| word.trailing_spaces == 0) {
                    lines.push(word.to_string());
                }
                // These separators cannot be crossed by a correction.
                lines.push(self.reset());
                lines
            }
            KeyCode::KEY_SPACE => {
                let Some(word) = self.words.last_mut() else {
                    return Vec::new();
                };
                word.trailing_spaces += 1;
                if word.trailing_spaces == 1 {
                    vec![word.to_string()]
                } else {
                    Vec::new()
                }
            }
            KeyCode::KEY_BACKSPACE => self.backspace().into_iter().collect(),
            key if is_text_key(key) => {
                if self
                    .words
                    .last()
                    .is_none_or(|word| word.trailing_spaces > 0)
                {
                    self.words.push(Word::default());
                }
                let shift = self.modifiers.iter().any(|(_, key)| is_shift(*key));
                let word = self.words.last_mut().unwrap();
                word.strokes.push(Stroke { key, shift });
                word.layouts.push(self.layouts.current());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Preserve held modifiers: resetting text history does not release keys.
    pub fn reset(&mut self) -> String {
        self.words.clear();
        String::new()
    }

    fn backspace(&mut self) -> Option<String> {
        let word = self.words.last_mut()?;
        if word.trailing_spaces > 0 {
            word.trailing_spaces -= 1;
            return None;
        }
        word.strokes.pop();
        word.layouts.pop();
        if word.strokes.is_empty() {
            self.words.pop();
            return Some(
                self.words
                    .last()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            );
        }
        None
    }
}

fn is_shift(key: KeyCode) -> bool {
    matches!(key, KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT)
}

fn is_modifier(key: KeyCode) -> bool {
    is_shift(key)
        || matches!(
            key,
            KeyCode::KEY_LEFTCTRL
                | KeyCode::KEY_RIGHTCTRL
                | KeyCode::KEY_LEFTALT
                | KeyCode::KEY_RIGHTALT
                | KeyCode::KEY_LEFTMETA
                | KeyCode::KEY_RIGHTMETA
        )
}

pub(crate) fn is_text_key(key: KeyCode) -> bool {
    // Linux input keycode ranges for the alphanumeric rows, including
    // punctuation. No layout-to-character mapping is involved.
    (KeyCode::KEY_1.0..=KeyCode::KEY_EQUAL.0).contains(&key.0)
        || (KeyCode::KEY_Q.0..=KeyCode::KEY_RIGHTBRACE.0).contains(&key.0)
        || (KeyCode::KEY_A.0..=KeyCode::KEY_GRAVE.0).contains(&key.0)
        || (KeyCode::KEY_BACKSLASH.0..=KeyCode::KEY_SLASH.0).contains(&key.0)
        || matches!(key, KeyCode::KEY_102ND)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(name: &str) -> Arc<crate::layout::Layout> {
        Arc::new(crate::layout::Layout {
            name: name.into(),
            ..Default::default()
        })
    }

    #[test]
    fn previous_word_keeps_its_layout_and_backspace_keeps_layouts_aligned() {
        let layouts = crate::layout::LayoutTracker::default();
        layouts.set(Some(layout("us")));
        let mut history = History::with_layouts(layouts.clone());
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_SPACE);
        layouts.set(Some(layout("ru")));
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_B);
        assert_eq!(
            history.previous_word().unwrap().text().as_deref(),
            Some("a")
        );
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("фи"));
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("ф"));
        history.set_latest_layout(Some(layout("us")));
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("a"));
    }

    #[test]
    fn layout_changes_within_word_preserve_each_typed_character() {
        let layouts = crate::layout::LayoutTracker::default();
        layouts.set(Some(layout("us")));
        let mut history = History::with_layouts(layouts.clone());
        press(&mut history, KeyCode::KEY_A);
        layouts.set(Some(layout("ru")));
        press(&mut history, KeyCode::KEY_A);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("aф"));
        layouts.set(None);
        press(&mut history, KeyCode::KEY_B);
        assert!(history.latest_word().unwrap().text().is_none());
    }

    fn event(history: &mut History, key: KeyCode, value: i32) -> Vec<String> {
        history.process(Path::new("keyboard"), key, value)
    }

    fn press(history: &mut History, key: KeyCode) -> Vec<String> {
        event(history, key, 1)
    }

    #[test]
    fn completes_physical_word_with_punctuation_and_keeps_all_spaces() {
        let mut history = History::default();
        assert!(press(&mut history, KeyCode::KEY_G).is_empty());
        press(&mut history, KeyCode::KEY_COMMA);
        assert_eq!(press(&mut history, KeyCode::KEY_SPACE), ["KEY_G KEY_COMMA"]);
        assert!(event(&mut history, KeyCode::KEY_SPACE, 2).is_empty());
        assert_eq!(history.words[0].trailing_spaces, 2);
        assert_eq!(history.words[0].strokes.len(), 2);
    }

    #[test]
    fn repeats_are_strokes_and_releases_do_not_type() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_A);
        event(&mut history, KeyCode::KEY_A, 2);
        event(&mut history, KeyCode::KEY_A, 0);
        assert_eq!(press(&mut history, KeyCode::KEY_SPACE), ["KEY_A KEY_A"]);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        event(&mut history, KeyCode::KEY_BACKSPACE, 2);
        assert_eq!(history.words[0].strokes.len(), 1);
        assert_eq!(event(&mut history, KeyCode::KEY_BACKSPACE, 2), [""]);
    }

    #[test]
    fn erasing_latest_word_restores_previous_word_and_its_spaces() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        press(&mut history, KeyCode::KEY_SPACE);
        assert!(press(&mut history, KeyCode::KEY_BACKSPACE).is_empty());
        assert_eq!(press(&mut history, KeyCode::KEY_BACKSPACE), ["KEY_A"]);
        assert_eq!(history.words[0].trailing_spaces, 2);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(press(&mut history, KeyCode::KEY_BACKSPACE), [""]);
        assert!(press(&mut history, KeyCode::KEY_BACKSPACE).is_empty());
    }

    #[test]
    fn typing_after_erasing_separator_continues_previous_word() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        press(&mut history, KeyCode::KEY_B);
        assert_eq!(press(&mut history, KeyCode::KEY_SPACE), ["KEY_A KEY_B"]);
        assert_eq!(history.words.len(), 1);
    }

    #[test]
    fn shift_is_saved_per_stroke_and_tracked_across_devices() {
        let mut history = History::default();
        let second = Path::new("second-keyboard");
        press(&mut history, KeyCode::KEY_LEFTSHIFT);
        history.process(second, KeyCode::KEY_LEFTSHIFT, 1);
        press(&mut history, KeyCode::KEY_A);
        event(&mut history, KeyCode::KEY_LEFTSHIFT, 0);
        event(&mut history, KeyCode::KEY_A, 2);
        history.process(second, KeyCode::KEY_LEFTSHIFT, 0);
        press(&mut history, KeyCode::KEY_B);
        assert_eq!(
            press(&mut history, KeyCode::KEY_SPACE),
            ["Shift+KEY_A Shift+KEY_A KEY_B"]
        );
        assert_eq!(
            history.words[0]
                .strokes
                .iter()
                .map(|stroke| stroke.shift)
                .collect::<Vec<_>>(),
            [true, true, false]
        );
    }

    #[test]
    fn hard_separators_complete_then_forget_words() {
        for separator in [KeyCode::KEY_TAB, KeyCode::KEY_ENTER, KeyCode::KEY_KPENTER] {
            let mut history = History::default();
            press(&mut history, KeyCode::KEY_A);
            press(&mut history, KeyCode::KEY_SPACE);
            press(&mut history, KeyCode::KEY_B);
            assert_eq!(press(&mut history, separator), ["KEY_B", ""]);
            assert!(press(&mut history, KeyCode::KEY_BACKSPACE).is_empty());
            press(&mut history, KeyCode::KEY_C);
            press(&mut history, KeyCode::KEY_SPACE);
            assert_eq!(press(&mut history, separator), [""]);
        }
    }

    #[test]
    fn reset_forgets_text_but_preserves_held_shift() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_LEFTSHIFT);
        press(&mut history, KeyCode::KEY_A);
        assert_eq!(history.reset(), "");
        press(&mut history, KeyCode::KEY_B);
        assert_eq!(press(&mut history, KeyCode::KEY_SPACE), ["Shift+KEY_B"]);
    }

    #[test]
    fn shortcuts_caps_lock_and_non_text_keys_do_not_become_strokes() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_LEFTCTRL);
        press(&mut history, KeyCode::KEY_C);
        event(&mut history, KeyCode::KEY_LEFTCTRL, 0);
        press(&mut history, KeyCode::KEY_CAPSLOCK);
        press(&mut history, KeyCode::KEY_F1);
        press(&mut history, KeyCode::KEY_PAUSE);
        press(&mut history, KeyCode::KEY_A);
        assert_eq!(press(&mut history, KeyCode::KEY_SPACE), ["KEY_A"]);
    }
}
