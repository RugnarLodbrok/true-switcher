use std::{io, path::Path, sync::Arc};

use evdev::KeyCode;

use crate::{
    config::{Configuration, ConfigurationFile},
    dictionary::{DICTIONARY_COUNT, DictionaryAutoSwitch, DictionaryLoadObserver},
    history::{History, HistoryEvent},
    kde,
    layout::Layout,
    learned_rules::LearnedRules,
    punctuation::Punctuation,
    word::Word,
    xkb,
};

mod cli;
mod progress;
mod request;
mod table;

pub(crate) use cli::{COMMAND, run};
use request::{Candidate, Direction, Request};
use table::ScoreTable;

struct ScoreSession {
    dictionaries: DictionaryAutoSwitch,
    punctuation: Punctuation,
    directions: [Direction; DICTIONARY_COUNT],
    is_learned_enabled: bool,
}

struct ResolvedRequest<'a> {
    word: &'a str,
    direction: &'a Direction,
    strokes: Vec<&'a Candidate>,
}

impl ScoreSession {
    fn from_config(
        config: &Configuration,
        config_file: &ConfigurationFile,
        kde_path: &Path,
        is_learned_enabled: bool,
        observer: Option<&mut DictionaryLoadObserver<'_>>,
    ) -> io::Result<Self> {
        if config.dict.len() != DICTIONARY_COUNT {
            return Err(io::Error::other(
                "Exactly two dictionaries must be configured",
            ));
        }
        let layouts = kde::configured_layouts_from(kde_path)?;
        let mut dictionaries = DictionaryAutoSwitch::load_with_progress(
            config,
            |name| resolve_layout(&layouts, kde_path, name),
            || Ok(layouts.len()),
            observer,
        )?;
        let mut punctuation = Punctuation::new(&config.punctuation)?;
        punctuation.update_layouts(layouts);
        if is_learned_enabled {
            dictionaries.learned = LearnedRules::load(config_file.learned_rules_path())?;
            dictionaries
                .learned
                .set_strong_punctuation(punctuation.strong_characters_by_layout());
        }
        let [first, second] = dictionaries
            .layout_pair()
            .expect("validated dictionary pair");
        let directions = [
            Direction::new(first, second, &punctuation),
            Direction::new(second, first, &punctuation),
        ];
        Ok(Self {
            dictionaries,
            punctuation,
            directions,
            is_learned_enabled,
        })
    }

    fn resolve_request<'a>(&'a self, line: &'a str) -> Result<ResolvedRequest<'a>, String> {
        let request = Request::parse(line)?;
        let direction = self.directions.iter().find(|direction| {
            direction.from.name() == request.from && direction.to.name() == request.to
        }).ok_or_else(|| format!(
            "Invalid direction: {:?} to {:?}. Use two different dictionary layouts ({}, {}).",
            request.from, request.to, self.directions[0].from.name(), self.directions[0].to.name(),
        ))?;
        Ok(ResolvedRequest {
            word: request.word,
            direction,
            strokes: direction.resolve(&request)?,
        })
    }

    fn trace(&self, line: &str, threshold_override: Option<f64>) -> Result<String, String> {
        let request = self.resolve_request(line)?;
        let direction = request.direction;
        let threshold = threshold_override.unwrap_or_else(|| {
            self.dictionaries
                .threshold(&direction.to)
                .expect("configured target layout")
        });
        let mut table = ScoreTable::new(direction.from.name(), direction.to.name(), threshold);
        self.visit_prefixes(request, |word, from_text, to_text, is_word_end| {
            let comparison = self
                .dictionaries
                .comparison(word, &direction.from, is_word_end);
            table.push(from_text, to_text, is_word_end, comparison);
        })?;
        Ok(table.render(self.is_learned_enabled))
    }

    fn visit_prefixes(
        &self,
        request: ResolvedRequest<'_>,
        mut visit: impl FnMut(&Word, &str, Option<&str>, bool),
    ) -> Result<(), String> {
        let direction = request.direction;
        let mut history = History::default();
        let mut to_text = Some(String::new());
        for ((offset, character), candidate) in request.word.char_indices().zip(request.strokes) {
            let stroke = candidate.stroke;
            let event = history.process(
                stroke.key,
                stroke.shift,
                Some(direction.from.clone()),
                &self.punctuation,
            );
            if matches!(event, HistoryEvent::WordEnd) {
                return Err(format!(
                    "{character:?} closed the word before the first space."
                ));
            }
            match (&mut to_text, &candidate.to_text) {
                (Some(text), Some(next)) => text.push_str(next),
                _ => to_text = None,
            }
            let prefix = &request.word[..offset + character.len_utf8()];
            visit(
                history.latest_word().expect("text stroke creates a word"),
                prefix,
                to_text.as_deref(),
                false,
            );
        }
        history.process(
            KeyCode::KEY_SPACE,
            false,
            Some(direction.from.clone()),
            &self.punctuation,
        );
        visit(
            history.latest_word().expect("the request includes a word"),
            request.word,
            to_text.as_deref(),
            true,
        );
        Ok(())
    }
}

fn resolve_layout(layouts: &[Layout], kde_path: &Path, name: &str) -> io::Result<Arc<Layout>> {
    let layout = layouts
        .iter()
        .find(|layout| layout.name() == name)
        .ok_or_else(|| {
            io::Error::other(format!(
                "Layout {name:?} is absent from {}",
                kde_path.display()
            ))
        })?;
    xkb::validate_keymap(layout)
        .map_err(|error| io::Error::other(format!("Cannot load layout {name:?}: {error}")))?;
    Ok(Arc::new(layout.clone()))
}

#[cfg(test)]
mod tests;
