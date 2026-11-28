use std::{
    collections::{BTreeMap, HashSet},
    io,
};

use regex::Regex;

use crate::{key_codes::text_keys, layout::Layout, word::Stroke};

#[derive(Default)]
pub(crate) struct Punctuation {
    pattern: Option<Regex>,
    layouts: Vec<Layout>,
    strong: HashSet<Stroke>,
}

impl Punctuation {
    pub(crate) fn new(pattern: &str) -> io::Result<Self> {
        Ok(Self {
            pattern: crate::dictionary::compile_full_match_pattern(pattern, "punctuation")?,
            ..Self::default()
        })
    }

    pub(crate) fn update_layouts(&mut self, layouts: Vec<Layout>) -> bool {
        if self.layouts == layouts {
            return false;
        }
        self.strong.clear();
        if let Some(pattern) = &self.pattern
            && !layouts.is_empty()
        {
            self.strong.extend(text_strokes().filter(|stroke| {
                layouts.iter().all(|layout| {
                    crate::xkb::stroke_text(layout, stroke)
                        .is_ok_and(|text| pattern.is_match(&text))
                })
            }));
        }
        self.layouts = layouts;
        true
    }

    pub(crate) fn is_strong(&self, stroke: &Stroke) -> bool {
        self.strong.contains(stroke)
    }

    pub(crate) fn strong_characters_by_layout(&self) -> BTreeMap<String, HashSet<char>> {
        let mut by_layout = BTreeMap::new();
        if self.strong.is_empty() {
            return by_layout;
        }
        for layout in &self.layouts {
            by_layout
                .entry(layout.name().to_owned())
                .or_insert_with(|| {
                    let mut strong = HashSet::new();
                    let mut other = HashSet::new();
                    for stroke in text_strokes() {
                        let Ok(text) = crate::xkb::stroke_text(layout, &stroke) else {
                            continue;
                        };
                        if let Some(character) = single_character(&text) {
                            if self.is_strong(&stroke) {
                                strong.insert(character);
                            } else {
                                other.insert(character);
                            }
                        }
                    }
                    strong.retain(|character| !other.contains(character));
                    strong
                });
        }
        by_layout
    }
}

pub(crate) fn single_character(text: &str) -> Option<char> {
    let mut characters = text.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

pub(crate) fn text_strokes() -> impl Iterator<Item = Stroke> {
    text_keys().flat_map(|key| [false, true].map(move |shift| Stroke { key, shift }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;

    #[test]
    fn uses_the_physical_key_and_shift_across_all_active_layouts() {
        let mut punctuation = Punctuation::new(r"[`~<>\p{P}]+").unwrap();
        let slash = Stroke {
            key: KeyCode::KEY_SLASH,
            shift: false,
        };
        assert!(!punctuation.is_strong(&slash));
        punctuation.update_layouts(vec![
            Layout::new("us", "", "pc105", ""),
            Layout::new("ru", "", "pc105", ""),
        ]);
        assert!(punctuation.is_strong(&slash));
        for shift in [false, true] {
            assert!(punctuation.is_strong(&Stroke {
                key: KeyCode::KEY_KPSLASH,
                shift
            }));
            assert!(punctuation.is_strong(&Stroke {
                key: KeyCode::KEY_MINUS,
                shift
            }));
        }
        assert!(!punctuation.is_strong(&Stroke {
            key: KeyCode::KEY_SEMICOLON,
            shift: false
        }));
        assert!(!punctuation.is_strong(&Stroke {
            key: KeyCode::KEY_COMMA,
            shift: true
        }));
        assert!(punctuation.is_strong(&Stroke {
            key: KeyCode::KEY_102ND,
            shift: false
        }));

        punctuation.update_layouts(vec![Layout::new("us", "", "pc105", "")]);
        assert!(punctuation.is_strong(&Stroke {
            key: KeyCode::KEY_COMMA,
            shift: true
        }));
        punctuation.update_layouts(Vec::new());
        assert!(!punctuation.is_strong(&slash));
    }

    #[test]
    fn empty_punctuation_disables_boundaries() {
        let mut punctuation = Punctuation::new("").unwrap();
        punctuation.update_layouts(vec![Layout::new("us", "", "", "")]);
        assert!(!punctuation.is_strong(&Stroke {
            key: KeyCode::KEY_SLASH,
            shift: false
        }));
    }
}
