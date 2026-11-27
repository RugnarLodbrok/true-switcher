use crate::dictionary::DictionaryAutoSwitch;
use crate::key_codes::is_text_key;
use crate::layout::Layout;
use crate::punctuation::Punctuation;
#[cfg(test)]
use crate::word::ChainMember;
use crate::word::{Stroke, Word, WordStroke};
use evdev::KeyCode;
use std::sync::Arc;

#[derive(Default)]
pub struct History {
    words: Vec<Word>,
    is_auto_switch_blocked: bool,
}

pub(crate) enum HistoryEvent {
    Stroke,
    WordEnd,
    LeadingPunctuation,
    AdditionalSeparator,
    Other,
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
            word.chain_member = None;
            for entry in &mut word.entries {
                entry.layout = layout.clone();
            }
        }
    }

    pub(crate) fn process(
        &mut self,
        key: KeyCode,
        is_shift: bool,
        layout: Option<Arc<Layout>>,
        dictionaries: &DictionaryAutoSwitch,
        punctuation: &Punctuation,
    ) -> HistoryEvent {
        match key {
            KeyCode::KEY_SPACE => {
                let Some(word) = self.words.last_mut() else {
                    return HistoryEvent::Other;
                };
                let is_closed = word.is_closed();
                word.trailing_spaces += 1;
                if is_closed {
                    HistoryEvent::AdditionalSeparator
                } else {
                    log::trace!("{word}");
                    HistoryEvent::WordEnd
                }
            }
            KeyCode::KEY_BACKSPACE => {
                self.backspace();
                HistoryEvent::Other
            }
            key if is_text_key(key) => {
                let stroke = Stroke {
                    key,
                    shift: is_shift,
                };
                let is_strong = punctuation.is_strong(&stroke);
                if self
                    .words
                    .last()
                    .is_none_or(|word| word.trailing_spaces > 0 || (word.is_closed() && !is_strong))
                {
                    self.words.push(Word::default());
                    self.is_auto_switch_blocked = false;
                }
                let word = self.words.last_mut().unwrap();
                if !word.has_dead_key {
                    word.has_dead_key = layout.as_ref().is_some_and(|layout| {
                        crate::xkb::has_dead_key(layout, [&stroke]) == Ok(true)
                    });
                }
                let entry = WordStroke { stroke, layout };
                if !is_strong {
                    word.push_stroke(entry, dictionaries);
                    return HistoryEvent::Stroke;
                }
                let is_word_end = word.has_search_text() && !word.is_closed();
                if word.has_search_text() {
                    word.trailing_punctuation += 1;
                } else {
                    word.leading_punctuation += 1;
                }
                word.entries.push(entry);
                if is_word_end {
                    log::trace!("{word}");
                    HistoryEvent::WordEnd
                } else if word.is_closed() {
                    HistoryEvent::AdditionalSeparator
                } else {
                    HistoryEvent::LeadingPunctuation
                }
            }
            _ => HistoryEvent::Other,
        }
    }

    pub fn reset(&mut self) -> Vec<Word> {
        self.is_auto_switch_blocked = false;
        if let Some(word) = self.words.last().filter(|word| !word.is_closed()) {
            log::trace!("{word}");
        }
        log::trace!("");
        std::mem::take(&mut self.words)
    }

    fn backspace(&mut self) {
        let Some(word) = self.words.last_mut() else {
            return;
        };
        let was_closed = word.is_closed();
        if word.trailing_spaces > 0 {
            word.trailing_spaces -= 1;
        } else {
            word.pop_stroke();
        }
        if was_closed && !word.is_closed() {
            word.chain_member = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(history: &mut History, key: KeyCode) {
        history.process(
            key,
            false,
            Some(Arc::new(Layout::new("us", "", "", ""))),
            &DictionaryAutoSwitch::default(),
            &Punctuation::default(),
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
        let words = history.reset();
        assert_eq!(words[0].text().as_deref(), Ok("ab,"));
        assert!(words[0].dictionary_prefixes.is_empty());
    }

    #[test]
    fn layout_and_shift_are_preserved_per_stroke() {
        let mut history = History::default();
        history.process(
            KeyCode::KEY_A,
            true,
            Some(Arc::new(Layout::new("us", "", "", ""))),
            &DictionaryAutoSwitch::default(),
            &Punctuation::default(),
        );
        history.process(
            KeyCode::KEY_B,
            false,
            Some(Arc::new(Layout::new("ru", "", "", ""))),
            &DictionaryAutoSwitch::default(),
            &Punctuation::default(),
        );
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Aи"));
        history.process(
            KeyCode::KEY_C,
            false,
            None,
            &DictionaryAutoSwitch::default(),
            &Punctuation::default(),
        );
        assert!(matches!(
            history.latest_word().unwrap().text(),
            Err(crate::xkb::ConversionError::UnknownLayout { .. })
        ));
        press(&mut history, KeyCode::KEY_BACKSPACE);
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Aи"));
        history.set_latest_layout(Some(Arc::new(Layout::new("us", "", "", ""))));
        assert_eq!(history.latest_word().unwrap().text().as_deref(), Ok("Ab"));
    }

    #[test]
    fn splits_a_path_by_physical_punctuation_without_a_history_reset() {
        use KeyCode as K;
        let us = Arc::new(Layout::new("us", "", "pc105", ""));
        let ru = Arc::new(Layout::new("ru", "", "pc105", ""));
        let mut punctuation = Punctuation::new(r"[`~<>\p{P}]+").unwrap();
        punctuation.update_layouts(vec![(*us).clone(), (*ru).clone()]);
        let dictionaries = DictionaryAutoSwitch::default();
        let mut history = History::default();
        for (layout, keys) in [
            (
                &us,
                &[
                    K::KEY_GRAVE,
                    K::KEY_SLASH,
                    K::KEY_H,
                    K::KEY_O,
                    K::KEY_M,
                    K::KEY_E,
                    K::KEY_SLASH,
                ][..],
            ),
            (
                &ru,
                &[K::KEY_A, K::KEY_F, K::KEY_Q, K::KEY_K, K::KEY_SLASH][..],
            ),
            (&us, &[K::KEY_M, K::KEY_P, K::KEY_3][..]),
        ] {
            for &key in keys {
                history.process(
                    key,
                    key == K::KEY_GRAVE,
                    Some(layout.clone()),
                    &dictionaries,
                    &punctuation,
                );
            }
        }
        let words = history.reset();
        assert_eq!(
            words
                .iter()
                .map(|word| word.text().unwrap())
                .collect::<Vec<_>>(),
            ["~/", "home/", "файл.", "mp3"]
        );
    }

    #[test]
    fn keeps_initial_and_trailing_series_and_reopens_on_the_last_backspace() {
        use KeyCode as K;
        let us = Arc::new(Layout::new("us", "", "", ""));
        let mut punctuation = Punctuation::new("[/]+").unwrap();
        punctuation.update_layouts(vec![(*us).clone()]);
        let dictionaries = DictionaryAutoSwitch::default();
        let mut history = History::default();
        let press = |history: &mut History, key| {
            history.process(key, false, Some(us.clone()), &dictionaries, &punctuation)
        };
        for key in [
            K::KEY_SLASH,
            K::KEY_SLASH,
            K::KEY_N,
            K::KEY_E,
            K::KEY_W,
            K::KEY_SLASH,
            K::KEY_SLASH,
        ] {
            press(&mut history, key);
        }
        history.latest_word_mut().unwrap().chain_member =
            Some(ChainMember::Ambiguous { delta_score: 1.0 });
        press(&mut history, K::KEY_B);
        press(&mut history, K::KEY_BACKSPACE);
        let word = history.latest_word().unwrap();
        assert_eq!(word.text().as_deref(), Ok("//new//"));
        assert_eq!(us.to_text(word.search_strokes()).as_deref(), Ok("new"));
        press(&mut history, K::KEY_BACKSPACE);
        assert!(history.latest_word().unwrap().is_closed());
        assert_eq!(
            history.latest_word().unwrap().chain_member,
            Some(ChainMember::Ambiguous { delta_score: 1.0 })
        );
        press(&mut history, K::KEY_BACKSPACE);
        assert!(!history.latest_word().unwrap().is_closed());
        assert_eq!(history.latest_word().unwrap().chain_member, None);
        press(&mut history, K::KEY_B);
        assert_eq!(
            history.latest_word().unwrap().text().as_deref(),
            Ok("//newb")
        );
    }
}
