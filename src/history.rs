use crate::layout::Layout;
use crate::word::{Stroke, Word, WordStroke};
use evdev::KeyCode;
use std::sync::Arc;

#[derive(Default)]
pub struct History {
    words: Vec<Word>,
    is_auto_switch_blocked: bool,
}

impl History {
    pub(crate) fn latest_words(&self, count: usize) -> &[Word] {
        &self.words[self.words.len().saturating_sub(count)..]
    }

    pub(crate) fn preceding_words(&self) -> impl Iterator<Item = &Word> {
        self.words.iter().rev().skip(1)
    }

    pub(crate) fn word_before_latest(&self, count: usize) -> Option<&Word> {
        let preceding_count = self.words.len().checked_sub(count)?;
        self.words[..preceding_count].last()
    }

    pub fn latest_word(&self) -> Option<&Word> {
        self.words.last()
    }

    pub(crate) fn latest_word_mut(&mut self) -> Option<&mut Word> {
        self.words.last_mut()
    }

    pub fn block_auto_switch(&mut self, count: usize) {
        self.is_auto_switch_blocked = count > 0 && self.latest_word().is_some();
        if count > 1 {
            // Backspace can make any word in the corrected range current again.
            let start = self.words.len().saturating_sub(count);
            for word in &mut self.words[start..] {
                word.is_range_corrected = true;
            }
        }
    }

    pub fn is_auto_switch_blocked(&self) -> bool {
        self.is_auto_switch_blocked
            || self
                .latest_word()
                .is_some_and(|word| word.is_range_corrected)
    }

    #[cfg(test)]
    pub fn set_latest_layout(&mut self, layout: Option<Arc<Layout>>) {
        self.set_layout_for_latest_words(1, layout);
    }

    pub(crate) fn set_layout_for_latest_words(
        &mut self,
        count: usize,
        layout: Option<Arc<Layout>>,
    ) {
        let start = self.words.len().saturating_sub(count);
        for word in &mut self.words[start..] {
            word.is_ambiguous = false;
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
                    self.is_auto_switch_blocked = false;
                }
                let word = self.words.last_mut().unwrap();
                let stroke = Stroke {
                    key,
                    shift: is_shift,
                };
                if !word.has_dead_key {
                    word.has_dead_key = layout.as_ref().is_some_and(|layout| {
                        crate::xkb::has_dead_key(layout, [&stroke]) == Ok(true)
                    });
                }
                word.entries.push(WordStroke { stroke, layout });
            }
            _ => {}
        }
    }

    pub fn reset(&mut self) -> Vec<Word> {
        self.is_auto_switch_blocked = false;
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
            if word.trailing_spaces == 0 {
                word.is_ambiguous = false;
            }
            return;
        }
        word.entries.pop();
        for stack in &mut word.dictionary_prefixes {
            stack.pop();
        }
        if word.entries.is_empty() {
            self.words.pop();
            self.is_auto_switch_blocked = false;
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
        history.process(key, false, Some(Arc::new(Layout::new("us", "", "", ""))));
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
        history.set_latest_layout(Some(Arc::new(Layout::new("ru", "", "", ""))));
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("ф"));
        assert_eq!(history.latest_word().unwrap().trailing_spaces, 2);
        assert!(history.word_before_latest(1).is_none());
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
        assert_eq!(history.reset()[0].text().as_deref(), Ok("ab,"));
    }

    #[test]
    fn layout_and_shift_are_preserved_per_stroke() {
        let mut history = History::default();
        history.process(
            KeyCode::KEY_A,
            true,
            Some(Arc::new(Layout::new("us", "", "", ""))),
        );
        history.process(
            KeyCode::KEY_B,
            false,
            Some(Arc::new(Layout::new("ru", "", "", ""))),
        );
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Aи"));
        history.process(KeyCode::KEY_C, false, None);
        assert!(matches!(
            history.latest_word().unwrap().text(),
            Err(crate::xkb::ConversionError::UnknownLayout { .. })
        ));
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Aи"));
        history.set_latest_layout(Some(Arc::new(Layout::new("us", "", "", ""))));
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Ab"));
    }
}
