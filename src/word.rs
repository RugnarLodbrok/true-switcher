use evdev::KeyCode;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub key: KeyCode,
    pub shift: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Word {
    pub entries: Vec<WordStroke>,
    pub trailing_spaces: usize,
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

    pub fn text(&self) -> Option<String> {
        let mut text = String::new();
        let mut start = 0;
        while start < self.entries.len() {
            let layout = self.entries[start].layout.as_ref()?;
            let end = (start + 1..self.entries.len())
                .find(|&index| self.entries[index].layout.as_ref() != Some(layout))
                .unwrap_or(self.entries.len());
            text.push_str(
                &layout.text(self.entries[start..end].iter().map(|entry| &entry.stroke))?,
            );
            start = end;
        }
        Some(text)
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
