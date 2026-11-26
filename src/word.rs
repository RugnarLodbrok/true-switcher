use crate::dictionary::{DictionaryAutoSwitch, Prefix};
use evdev::KeyCode;
use std::fmt;
use std::sync::Arc;

pub(crate) const WORD_END: &str = " ";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stroke {
    pub key: KeyCode,
    pub shift: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub entries: Vec<WordStroke>,
    pub trailing_spaces: usize,
    pub(crate) leading_punctuation: usize,
    pub(crate) trailing_punctuation: usize,
    pub(crate) dictionary_prefixes: Vec<Vec<Prefix>>,
    pub(crate) has_dead_key: bool,
    pub(crate) ambiguous_delta_score: Option<f64>,
    pub(crate) is_range_corrected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordStroke {
    pub stroke: Stroke,
    pub layout: Option<Arc<crate::layout::Layout>>,
}

impl Word {
    pub(crate) fn push_stroke(&mut self, entry: WordStroke, dictionaries: &DictionaryAutoSwitch) {
        dictionaries.push_prefixes(&mut self.dictionary_prefixes, &entry.stroke);
        self.entries.push(entry);
    }

    pub(crate) fn pop_stroke(&mut self) {
        if self.trailing_punctuation > 0 {
            self.trailing_punctuation -= 1;
        } else if self.has_search_text() {
            for stack in &mut self.dictionary_prefixes {
                stack.pop();
            }
        } else {
            self.leading_punctuation -= 1;
        }
        self.entries.pop();
    }

    pub(crate) fn has_search_text(&self) -> bool {
        self.entries.len() > self.leading_punctuation + self.trailing_punctuation
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.trailing_spaces > 0 || self.trailing_punctuation > 0
    }

    pub(crate) fn search_strokes(&self) -> impl Iterator<Item = &Stroke> {
        self.entries[self.leading_punctuation..self.entries.len() - self.trailing_punctuation]
            .iter()
            .map(|entry| &entry.stroke)
    }

    pub fn strokes(&self) -> impl Iterator<Item = &Stroke> {
        self.entries.iter().map(|entry| &entry.stroke)
    }

    pub(crate) fn has_dead_key_in(&self, layout: &crate::layout::Layout) -> bool {
        let space = Stroke {
            key: KeyCode::KEY_SPACE,
            shift: false,
        };
        let strokes = self
            .strokes()
            .chain((self.trailing_spaces > 0).then_some(&space));
        crate::xkb::has_dead_key(layout, strokes) == Ok(true)
    }

    pub fn text(&self) -> Result<String, crate::xkb::ConversionError> {
        let mut text = String::new();
        let mut start = 0;
        while start < self.entries.len() {
            let layout = self.entries[start].layout.as_ref().ok_or_else(|| {
                let error = crate::xkb::ConversionError::UnknownLayout {
                    key: self.entries[start].stroke.key,
                };
                log::error!("true-switcher: text conversion: {error}");
                error
            })?;
            let end = (start + 1..self.entries.len())
                .find(|&index| self.entries[index].layout.as_ref() != Some(layout))
                .unwrap_or(self.entries.len());
            text.push_str(
                &layout.to_text(self.entries[start..end].iter().map(|entry| &entry.stroke))?,
            );
            start = end;
        }
        Ok(text)
    }
}

impl fmt::Display for Word {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, stroke) in self.strokes().enumerate() {
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
