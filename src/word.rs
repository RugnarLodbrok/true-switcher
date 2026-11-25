use evdev::KeyCode;
use std::fmt;
use std::sync::Arc;

pub(crate) const WORD_END: &str = " ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub key: KeyCode,
    pub shift: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub entries: Vec<WordStroke>,
    pub trailing_spaces: usize,
    pub(crate) dictionary_prefixes: Vec<Vec<crate::dictionary::Prefix>>,
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
