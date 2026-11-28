use std::{collections::HashMap, fmt::Write as _, sync::Arc};

use crate::{
    layout::Layout,
    punctuation::{Punctuation, single_character, text_strokes},
    word::Stroke,
    xkb::{self, ConversionError},
};

pub(super) struct Request<'a> {
    pub(super) from: &'a str,
    pub(super) to: &'a str,
    pub(super) word: &'a str,
    indices: Vec<&'a str>,
}

impl<'a> Request<'a> {
    pub(super) fn parse(line: &'a str) -> Result<Self, String> {
        let mut parts = line.split_whitespace();
        let (Some(from), Some(to), Some(word)) = (parts.next(), parts.next(), parts.next()) else {
            return Err("Expected FROM TO WORD [INDEX ...].".into());
        };
        Ok(Self {
            from,
            to,
            word,
            indices: parts.collect(),
        })
    }
}

#[derive(Default)]
struct CharacterStrokes {
    candidates: Vec<Candidate>,
    has_dead_key: bool,
}

pub(super) struct Candidate {
    pub(super) stroke: Stroke,
    pub(super) to_text: Option<String>,
    is_strong: bool,
}

pub(super) struct Direction {
    pub(super) from: Arc<Layout>,
    pub(super) to: Arc<Layout>,
    characters: HashMap<char, CharacterStrokes>,
}

impl Direction {
    pub(super) fn new(from: &Arc<Layout>, to: &Arc<Layout>, punctuation: &Punctuation) -> Self {
        let mut direction = Self {
            from: from.clone(),
            to: to.clone(),
            characters: HashMap::new(),
        };
        let mut strokes: Vec<_> = text_strokes().collect();
        strokes.sort_unstable_by_key(|stroke| (stroke.key.0, stroke.shift));
        for stroke in strokes {
            direction.add_stroke(stroke, punctuation);
        }
        direction
    }

    fn add_stroke(&mut self, stroke: Stroke, punctuation: &Punctuation) {
        let Some(character) = xkb::stroke_text(&self.from, &stroke)
            .ok()
            .as_deref()
            .and_then(single_character)
        else {
            return;
        };
        let choices = self.characters.entry(character).or_default();
        let to_text = xkb::stroke_text(&self.to, &stroke);
        if matches!(to_text, Err(ConversionError::DeadKey { .. })) {
            choices.has_dead_key = true;
            return;
        }
        let to_text = to_text.ok();
        let is_strong = punctuation.is_strong(&stroke);
        if !choices
            .candidates
            .iter()
            .any(|choice| choice.to_text == to_text && choice.is_strong == is_strong)
        {
            choices.candidates.push(Candidate {
                stroke,
                to_text,
                is_strong,
            });
        }
    }

    pub(super) fn resolve(&self, request: &Request<'_>) -> Result<Vec<&Candidate>, String> {
        let mut selection = Selection::default();
        for (position, character) in request.word.chars().enumerate() {
            selection.push(
                position,
                character,
                self.characters.get(&character),
                &request.indices,
            );
        }
        if request.indices.len() != selection.ambiguous_count {
            writeln!(
                selection.errors,
                "Expected {} choice indices, got {}.",
                selection.ambiguous_count,
                request.indices.len()
            )
            .unwrap();
        }
        if selection.errors.is_empty() {
            return Ok(selection.strokes);
        }
        let mut message = format!(
            "Cannot resolve word in source layout {} (target: {}):\n{}",
            self.from.name(),
            self.to.name(),
            selection.errors
        );
        if selection.ambiguous_count > 0 {
            message.push_str(&selection.choices);
            message.push_str(
                "Append one zero-based choice index per ambiguous character, from left to right.\n",
            );
        }
        Err(message.trim_end().to_owned())
    }
}

#[derive(Default)]
struct Selection<'a> {
    strokes: Vec<&'a Candidate>,
    errors: String,
    choices: String,
    ambiguous_count: usize,
}

impl<'a> Selection<'a> {
    fn push(
        &mut self,
        position: usize,
        character: char,
        choices: Option<&'a CharacterStrokes>,
        indices: &[&str],
    ) {
        let candidates = choices.map_or(&[][..], |choices| choices.candidates.as_slice());
        match candidates {
            [] => {
                let reason = if choices.is_some_and(|choices| choices.has_dead_key) {
                    "dead key in the target layout is not supported"
                } else {
                    "no supported stroke in the source layout"
                };
                writeln!(
                    self.errors,
                    "  Character index {position}, {character:?}: {reason}."
                )
                .unwrap();
            }
            [candidate] => self.strokes.push(candidate),
            _ => self.push_ambiguous(position, character, candidates, indices),
        }
    }

    fn push_ambiguous(
        &mut self,
        position: usize,
        character: char,
        candidates: &'a [Candidate],
        indices: &[&str],
    ) {
        writeln!(self.choices, "  Character index {position}, {character:?}:").unwrap();
        for (index, candidate) in candidates.iter().enumerate() {
            let shift = if candidate.stroke.shift {
                "Shift"
            } else {
                "no Shift"
            };
            let strong = if candidate.is_strong {
                " (strong pnt)"
            } else {
                ""
            };
            let text = candidate
                .to_text
                .as_ref()
                .map_or_else(|| "n/a".into(), |text| format!("{text:?}"));
            writeln!(
                self.choices,
                "    {index}. {:?} ({shift}) → {text}{strong}",
                candidate.stroke.key
            )
            .unwrap();
        }
        if let Some(index) = indices.get(self.ambiguous_count) {
            let candidate = index
                .parse::<usize>()
                .ok()
                .and_then(|index| candidates.get(index));
            if let Some(candidate) = candidate {
                self.strokes.push(candidate);
            } else {
                writeln!(self.errors, "  Invalid choice index {index:?} for character index {position} (expected 0–{}).", candidates.len() - 1).unwrap();
            }
        }
        self.ambiguous_count += 1;
    }
}
