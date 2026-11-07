use crate::layout::Layout;
use crate::word::{Stroke, Word, WordStroke};
use evdev::KeyCode;
use std::sync::Arc;

#[derive(Default)]
pub struct History {
    words: Vec<Word>,
}

impl History {
    pub fn previous_word(&self) -> Option<&Word> {
        self.words
            .len()
            .checked_sub(2)
            .and_then(|index| self.words.get(index))
    }

    pub fn latest_word(&self) -> Option<&Word> {
        self.words.last()
    }

    pub fn set_latest_layout(&mut self, layout: Option<Arc<Layout>>) {
        if let Some(word) = self.words.last_mut() {
            for entry in &mut word.entries {
                entry.layout = layout.clone();
            }
        }
    }

    pub fn process(&mut self, key: KeyCode, is_shift: bool, layout: Option<Arc<Layout>>) {
        match key {
            KeyCode::KEY_SPACE => {
                if let Some(word) = self.words.last_mut() {
                    word.trailing_spaces += 1;
                    if word.trailing_spaces == 1 {
                        log::trace!("{word}");
                    }
                }
            }
            KeyCode::KEY_BACKSPACE => self.backspace(),
            key if is_text_key(key) => {
                if self
                    .words
                    .last()
                    .is_none_or(|word| word.trailing_spaces > 0)
                {
                    self.words.push(Word::default());
                }
                self.words.last_mut().unwrap().entries.push(WordStroke {
                    stroke: Stroke {
                        key,
                        shift: is_shift,
                    },
                    layout,
                });
            }
            _ => {}
        }
    }

    pub fn reset(&mut self) -> Vec<Word> {
        if let Some(word) = self.words.last().filter(|word| word.trailing_spaces == 0) {
            log::trace!("{word}");
        }
        log::trace!("");
        std::mem::take(&mut self.words)
    }

    fn backspace(&mut self) {
        let Some(word) = self.words.last_mut() else {
            return;
        };
        if word.trailing_spaces > 0 {
            word.trailing_spaces -= 1;
            return;
        }
        word.entries.pop();
        if word.entries.is_empty() {
            self.words.pop();
            if let Some(word) = self.words.last() {
                log::trace!("{word}");
            } else {
                log::trace!("");
            }
        }
    }
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

    fn press(history: &mut History, key: KeyCode) {
        history.process(
            key,
            false,
            Some(Arc::new(Layout {
                name: "us".into(),
                ..Default::default()
            })),
        );
    }

    #[test]
    fn reset_returns_final_words_in_order_once() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        let words = history.reset();
        assert_eq!(
            words
                .iter()
                .map(|word| word.text().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(words[0].trailing_spaces, 2);
        assert_eq!(words[1].trailing_spaces, 0);
        assert!(history.reset().is_empty());
        assert!(history.latest_word().is_none());
    }

    #[test]
    fn backspace_restores_previous_word_with_its_layout_and_spaces() {
        let mut history = History::default();
        press(&mut history, KeyCode::KEY_A);
        history.set_latest_layout(Some(Arc::new(Layout {
            name: "ru".into(),
            ..Default::default()
        })));
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("ф"));
        assert_eq!(history.latest_word().unwrap().trailing_spaces, 2);
        assert!(history.previous_word().is_none());
        for _ in 0..4 {
            press(&mut history, KeyCode::KEY_BACKSPACE);
        }
        assert!(history.reset().is_empty());
    }

    #[test]
    fn typing_after_erasing_separator_continues_the_word() {
        let mut history = History::default();
        for key in [
            KeyCode::KEY_A,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_B,
            KeyCode::KEY_COMMA,
        ] {
            press(&mut history, key);
        }
        assert_eq!(history.reset()[0].text().as_deref(), Some("ab,"));
    }

    #[test]
    fn layout_and_shift_are_preserved_per_stroke() {
        let mut history = History::default();
        history.process(
            KeyCode::KEY_A,
            true,
            Some(Arc::new(Layout {
                name: "us".into(),
                ..Default::default()
            })),
        );
        history.process(
            KeyCode::KEY_B,
            false,
            Some(Arc::new(Layout {
                name: "ru".into(),
                ..Default::default()
            })),
        );
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("Aи"));
        history.process(KeyCode::KEY_C, false, None);
        assert_eq!(history.latest_word().unwrap().text(), None);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("Aи"));
        history.set_latest_layout(Some(Arc::new(Layout {
            name: "us".into(),
            ..Default::default()
        })));
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Some("Ab"));
    }
}
