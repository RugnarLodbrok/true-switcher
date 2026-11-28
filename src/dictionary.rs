use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use regex::Regex;

use crate::config::Configuration;
use crate::layout::Layout;
use crate::learned_rules::{LearnedRules, LearnedTrie};
use crate::punctuation;
use crate::word::{WORD_END, Word};

pub(crate) const DICTIONARY_COUNT: usize = 2;

pub(crate) enum DictionaryLoadProgress {
    Reading { bytes_read: u64, total_bytes: u64 },
    BuildingIndex,
}

pub(crate) type DictionaryLoadObserver<'a> = dyn FnMut(&Path, DictionaryLoadProgress) + 'a;

// Give learned-only matches the weight of a rare token (about 1 in 262,000).
// A fixed log2 probability keeps this weight independent of dictionary frequency units.
const LEARNED_BASE_SCORE: f64 = -18.0;
const AMBIGUOUS_WORD_DSCORE_MULTIPLIER: f64 = 1.0;

struct Node<T> {
    value: T,
    transitions: Range<usize>,
}

struct Transition {
    character: char,
    child: usize,
}

struct DictionaryTrie {
    nodes: Vec<Node<f64>>,
    transitions: Vec<Transition>,
}

impl DictionaryTrie {
    #[cfg(test)]
    fn read(input: impl BufRead, skip_pattern: Option<&Regex>) -> io::Result<Self> {
        Self::read_with_progress(input, skip_pattern, 0, |_| {})
    }

    fn read_with_progress(
        mut input: impl BufRead,
        skip_pattern: Option<&Regex>,
        total_bytes: u64,
        mut report: impl FnMut(DictionaryLoadProgress),
    ) -> io::Result<Self> {
        let mut records = Vec::new();
        let mut total = 0_u64;
        let mut line = String::new();
        let mut line_number = 0;
        let mut bytes_read = 0;
        report(DictionaryLoadProgress::Reading {
            bytes_read,
            total_bytes,
        });
        loop {
            line.clear();
            let consumed = input.read_line(&mut line)?;
            if consumed == 0 {
                break;
            }
            line_number += 1;
            bytes_read += consumed as u64;
            report(DictionaryLoadProgress::Reading {
                bytes_read,
                total_bytes,
            });
            let (word, frequency) = parse_record(&line).ok_or_else(|| {
                invalid(format!("Invalid dictionary record on line {line_number}"))
            })?;
            if skip_pattern.is_some_and(|pattern| pattern.is_match(word)) {
                continue;
            }
            total = total
                .checked_add(frequency)
                .ok_or_else(|| invalid("Dictionary frequency sum overflows u64"))?;
            records.push((word.to_owned(), frequency));
        }
        if total == 0 {
            return Err(invalid("Dictionary frequency sum must be positive"));
        }
        report(DictionaryLoadProgress::BuildingIndex);
        records.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if let Some(duplicate) = records.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(invalid(format!(
                "Duplicate dictionary word {:?}",
                duplicate[0].0
            )));
        }
        Ok(Self::build(records, total))
    }

    fn build(records: Vec<(String, u64)>, total: u64) -> Self {
        let mut nodes = vec![Node {
            value: total,
            transitions: 0..0,
        }];
        let mut edges = Vec::new();
        let mut previous = String::new();
        let mut path = vec![0];
        for (word, frequency) in records {
            let shared = word
                .chars()
                .zip(previous.chars())
                .take_while(|(a, b)| a == b)
                .count();
            path.truncate(shared + 1);
            for (index, character) in word.chars().chain(WORD_END.chars()).enumerate() {
                if index >= shared {
                    let parent = *path.last().unwrap();
                    let child = nodes.len();
                    nodes.push(Node {
                        value: 0,
                        transitions: 0..0,
                    });
                    edges.push((parent, character, child));
                    path.push(child);
                }
                nodes[path[index + 1]].value += frequency;
            }
            previous = word;
        }
        edges.sort_unstable_by_key(|&(parent, character, _)| (parent, character));
        let mut transitions = Vec::with_capacity(edges.len());
        for (parent, character, child) in edges {
            let range = &mut nodes[parent].transitions;
            if range.start == range.end {
                range.start = transitions.len();
            }
            transitions.push(Transition { character, child });
            range.end = transitions.len();
        }
        let nodes = nodes
            .into_iter()
            .map(|node| Node {
                value: (node.value as f64 / total as f64).log2(),
                transitions: node.transitions,
            })
            .collect();
        Self { nodes, transitions }
    }

    fn score(&self, text: &str) -> f64 {
        let mut node = 0;
        for character in text.chars().flat_map(char::to_lowercase) {
            let transitions = &self.transitions[self.nodes[node].transitions.clone()];
            let Ok(index) = transitions.binary_search_by_key(&character, |edge| edge.character)
            else {
                return f64::NEG_INFINITY;
            };
            node = transitions[index].child;
        }
        self.nodes[node].value
    }
}

fn parse_record(line: &str) -> Option<(&str, u64)> {
    let mut fields = line.split_whitespace();
    let word = fields.next()?;
    let count = fields.next()?;
    if fields.next().is_some() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((word, count.parse().ok()?))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn compile_full_match_pattern(pattern: &str, setting: &str) -> io::Result<Option<Regex>> {
    if pattern.is_empty() {
        return Ok(None);
    }
    let invalid_pattern = |error| invalid(format!("Invalid {setting}: {error}"));
    Regex::new(pattern).map_err(invalid_pattern)?; // Reject unmatched groups before wrapping.
    // The newline terminates a trailing verbose-mode comment and is ignored otherwise.
    Regex::new(&format!("\\A(?:{pattern}(?x)\n)\\z"))
        .map(Some)
        .map_err(invalid_pattern)
}

struct Dictionary {
    target: Arc<Layout>,
    trie: DictionaryTrie,
    threshold: f64,
}

struct LayoutIndexes<'a> {
    dictionary: &'a DictionaryTrie,
    learned: Option<&'a LearnedTrie>,
}

struct ScoredKey<'a> {
    key: Cow<'a, str>,
    total: f64,
}

struct PunctuationRuns {
    full: Regex,
    leading: Regex,
    trailing: Regex,
    separator: Regex,
}

impl PunctuationRuns {
    fn new(pattern: &str) -> io::Result<Option<Self>> {
        let Some(pattern) = punctuation::validate_pattern(pattern)? else {
            return Ok(None);
        };
        let run = format!("(?:{pattern})+");
        let compile = |prefix, suffix| {
            Regex::new(&format!("{prefix}{run}{suffix}")).map_err(punctuation::invalid_pattern)
        };
        Ok(Some(Self {
            full: compile(r"\A", r"\z")?,
            leading: compile(r"\A", "")?,
            trailing: compile("", r"\z")?,
            separator: compile("", "")?,
        }))
    }

    fn trim_edges<'a>(&self, text: &'a str) -> (&'a str, bool) {
        let start = self.leading.find(text).map_or(0, |matched| matched.end());
        let remainder = &text[start..];
        let end = self
            .trailing
            .find(remainder)
            .map_or(remainder.len(), |matched| matched.start());
        (&remainder[..end], end < remainder.len())
    }
}

impl LayoutIndexes<'_> {
    fn score(&self, key: &str) -> f64 {
        let (base, learned) = self.score_separated(key);
        base + learned as f64
    }

    fn score_separated(&self, key: &str) -> (f64, u128) {
        let dictionary = self.dictionary.score(key);
        let learned = self.learned.map_or(0, |trie| trie.count(key));
        let base = if dictionary == f64::NEG_INFINITY && learned > 0 {
            LEARNED_BASE_SCORE
        } else {
            dictionary
        };
        (base, learned)
    }

    fn score_text<'a>(&self, text: &'a str, is_exact: bool) -> ScoredKey<'a> {
        let key = if is_exact {
            Cow::Owned(format!("{text}{WORD_END}"))
        } else {
            Cow::Borrowed(text)
        };
        ScoredKey {
            total: self.score(&key),
            key,
        }
    }

    fn score_parts<'a>(
        &self,
        parts: impl Iterator<Item = &'a str>,
        is_last_exact: bool,
    ) -> Option<ScoredKey<'a>> {
        let mut parts = parts.filter(|part| !part.is_empty()).peekable();
        let first = parts.next()?;
        parts.peek()?;
        let mut weakest = self.score_text(first, true);
        while let Some(part) = parts.next() {
            let candidate = self.score_text(part, is_last_exact || parts.peek().is_some());
            if candidate.total < weakest.total {
                weakest = candidate;
            }
        }
        Some(weakest)
    }
}

pub(crate) struct DictionaryComparison<'a> {
    current_threshold: f64,
    alternative_dictionary: &'a Dictionary,
    pub(crate) delta_score: f64,
    pub(crate) current_base_score: f64,
    pub(crate) alternative_base_score: f64,
    learned_current: u128,
    learned_alternative: u128,
}

impl DictionaryComparison<'_> {
    pub(crate) fn learned_difference(&self) -> (Ordering, u128) {
        (
            self.learned_alternative.cmp(&self.learned_current),
            self.learned_alternative.abs_diff(self.learned_current),
        )
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum DictionaryDecision {
    Keep,
    Ambiguous(f64),
    DeferredByPunctuation,
    Switch(Arc<Layout>),
}

#[derive(Default)]
pub(crate) struct DictionaryAutoSwitch {
    dictionaries: Vec<Dictionary>,
    ambiguity_score_floor: Option<f64>,
    punctuation: Option<PunctuationRuns>,
    pub(crate) learned: LearnedRules,
}

impl DictionaryAutoSwitch {
    pub(crate) fn load(
        settings: &Configuration,
        resolve: impl Fn(&str) -> io::Result<Arc<Layout>>,
        layout_count: impl FnOnce() -> io::Result<usize>,
    ) -> io::Result<Self> {
        Self::load_with_progress(settings, resolve, layout_count, None)
    }

    pub(crate) fn load_with_progress(
        settings: &Configuration,
        resolve: impl Fn(&str) -> io::Result<Arc<Layout>>,
        layout_count: impl FnOnce() -> io::Result<usize>,
        mut observer: Option<&mut DictionaryLoadObserver<'_>>,
    ) -> io::Result<Self> {
        if settings.dict.len() != DICTIONARY_COUNT
            && (settings.auto_switch || !settings.dict.is_empty())
        {
            return Err(invalid("Exactly two dictionaries must be configured"));
        }
        if settings.auto_switch && layout_count()? > DICTIONARY_COUNT {
            return Err(invalid(
                "Dictionary auto-switch supports at most two KDE layouts",
            ));
        }
        let skip_pattern =
            compile_full_match_pattern(&settings.dict_skip_pattern, "dict_skip_pattern")?;
        let punctuation = PunctuationRuns::new(&settings.punctuation)?;
        let mut targets = HashSet::new();
        let mut dictionaries = Vec::new();
        for setting in &settings.dict {
            if !targets.insert(&setting.target) {
                return Err(invalid(format!(
                    "Duplicate dictionary target {:?}",
                    setting.target
                )));
            }
            if !setting.threshold.is_finite() || setting.threshold <= 0.0 {
                return Err(invalid(format!(
                    "dict {:?}: threshold must be finite and positive",
                    setting.target
                )));
            }
            let target = resolve(&setting.target)?;
            let trie = std::fs::File::open(&setting.file)
                .and_then(|file| {
                    let total_bytes = observer
                        .as_ref()
                        .and_then(|_| file.metadata().ok())
                        .map_or(0, |metadata| metadata.len());
                    DictionaryTrie::read_with_progress(
                        BufReader::new(file),
                        skip_pattern.as_ref(),
                        total_bytes,
                        |progress| {
                            if let Some(report) = observer.as_deref_mut() {
                                report(&setting.file, progress);
                            }
                        },
                    )
                })
                .map_err(|error| invalid(format!("{}: {error}", setting.file.display())))?;
            dictionaries.push(Dictionary {
                target,
                trie,
                threshold: setting.threshold,
            });
        }
        Ok(Self {
            dictionaries,
            ambiguity_score_floor: settings.ambiguity_score_floor,
            punctuation,
            ..Self::default()
        })
    }

    pub(crate) fn is_pair_loaded(&self) -> bool {
        self.dictionaries.len() == DICTIONARY_COUNT
    }

    pub(crate) fn layout_pair(&self) -> Option<[&Arc<Layout>; DICTIONARY_COUNT]> {
        let [first, second] = self.dictionaries.as_slice() else {
            return None;
        };
        Some([&first.target, &second.target])
    }

    pub(crate) fn threshold(&self, layout: &Layout) -> Option<f64> {
        self.dictionaries
            .iter()
            .find(|dictionary| dictionary.target.name() == layout.name())
            .map(|dictionary| dictionary.threshold)
    }

    fn layout_score(&self, word: &Word, index: usize, is_word_end: bool) -> Option<(f64, u128)> {
        if !word.has_search_text() {
            return None;
        }
        let dictionary = &self.dictionaries[index];
        let text = crate::xkb::strokes_text(&dictionary.target, word.search_strokes()).ok()?;
        let indexes = LayoutIndexes {
            dictionary: &dictionary.trie,
            learned: self.learned.trie(dictionary.target.name()),
        };
        let mut best = indexes.score_text(&text, is_word_end);
        let Some(punctuation) = &self.punctuation else {
            return Some(indexes.score_separated(&best.key));
        };
        let (remainder, has_trailing_punctuation) = punctuation.trim_edges(&text);
        let is_remainder_exact = is_word_end || has_trailing_punctuation;
        if !remainder.is_empty() && remainder.len() < text.len() {
            let trimmed = indexes.score_text(remainder, is_remainder_exact);
            if trimmed.total > best.total {
                best = trimmed;
            }
        }
        if let Some(split) =
            indexes.score_parts(punctuation.separator.split(remainder), is_remainder_exact)
            && split.total > best.total
        {
            best = split;
        }
        Some(indexes.score_separated(&best.key))
    }

    pub(crate) fn comparison(
        &self,
        word: &Word,
        current: &Layout,
        is_word_end: bool,
    ) -> Option<DictionaryComparison<'_>> {
        let current_index = self
            .dictionaries
            .iter()
            .position(|dictionary| dictionary.target.name() == current.name())?;
        let alternative_index = 1 - current_index;
        let (current_score, learned_current) =
            self.layout_score(word, current_index, is_word_end)?;
        let (alternative_score, learned_alternative) =
            self.layout_score(word, alternative_index, is_word_end)?;
        let current = current_score + learned_current as f64;
        let alternative = alternative_score + learned_alternative as f64;
        let delta_score = if current == f64::NEG_INFINITY && alternative == f64::NEG_INFINITY {
            0.0
        } else {
            alternative - current
        };
        Some(DictionaryComparison {
            current_threshold: self.dictionaries[current_index].threshold,
            alternative_dictionary: &self.dictionaries[alternative_index],
            delta_score,
            current_base_score: current_score,
            alternative_base_score: alternative_score,
            learned_current,
            learned_alternative,
        })
    }

    pub(crate) fn decision(
        &self,
        word: &Word,
        current: &Layout,
        is_word_end: bool,
        preceding_ambiguous_score: impl FnOnce(&Layout) -> f64,
    ) -> DictionaryDecision {
        let Some(comparison) = self.comparison(word, current, is_word_end) else {
            return DictionaryDecision::Keep;
        };
        let alternative = comparison.alternative_dictionary;
        let has_current_score = comparison.current_base_score.is_finite();
        let has_alternative_score = comparison.alternative_base_score.is_finite();
        let switch_delta_score = if has_current_score && has_alternative_score {
            comparison.delta_score
                + AMBIGUOUS_WORD_DSCORE_MULTIPLIER * preceding_ambiguous_score(&alternative.target)
        } else {
            comparison.delta_score
        };
        let is_switch = switch_delta_score > alternative.threshold;
        let ambiguity_lower_bound = self
            .ambiguity_score_floor
            .unwrap_or(-comparison.current_threshold);
        let is_ambiguous = is_word_end
            && has_current_score
            && has_alternative_score
            && comparison.delta_score >= ambiguity_lower_bound
            && comparison.delta_score <= alternative.threshold;
        if !is_switch && !is_ambiguous {
            return DictionaryDecision::Keep;
        }
        if self.punctuation.as_ref().is_some_and(|punctuation| {
            current
                .to_text(word.strokes())
                .is_ok_and(|text| punctuation.full.is_match(&text))
        }) {
            if is_word_end
                && is_switch
                && !word.has_dead_key_in(current)
                && !word.has_dead_key_in(&alternative.target)
            {
                return DictionaryDecision::DeferredByPunctuation;
            }
            return DictionaryDecision::Keep;
        }
        if is_switch {
            DictionaryDecision::Switch(alternative.target.clone())
        } else if word.has_dead_key_in(current) || word.has_dead_key_in(&alternative.target) {
            DictionaryDecision::Keep
        } else {
            DictionaryDecision::Ambiguous(comparison.delta_score)
        }
    }

    #[cfg(test)]
    fn target(&self, word: &Word, current: &Layout, is_word_end: bool) -> Option<Arc<Layout>> {
        match self.decision(word, current, is_word_end, |_| 0.0) {
            DictionaryDecision::Switch(target) => Some(target),
            _ => None,
        }
    }

    pub(crate) fn log_score(&self, word: &Word, current: Option<&Layout>, is_word_end: bool) {
        if self.dictionaries.is_empty() || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let Some(comparison) =
            current.and_then(|layout| self.comparison(word, layout, is_word_end))
        else {
            log::debug!("dict switch score: unavailable");
            return;
        };

        let delta = comparison.delta_score;
        let current = comparison.current_base_score.abs();
        let alternative = comparison.alternative_base_score.abs();
        if current.is_infinite() && alternative.is_infinite() {
            log::debug!("dict switch score: {delta:+6.2} (both layout scores missing)");
            return;
        }

        let (ordering, learned) = comparison.learned_difference();
        let correction = if learned == 0 {
            String::new()
        } else {
            let sign = if ordering.is_gt() { '+' } else { '-' };
            format!(" {sign} {learned:2}")
        };
        log::debug!(
            "dict switch score: {delta:+6.2} = {current:5.2} - {alternative:5.2}{correction}"
        );
    }
}

#[cfg(test)]
pub(crate) fn test_dictionaries(
    entries: [(&str, &str, &str, f64); DICTIONARY_COUNT],
) -> DictionaryAutoSwitch {
    DictionaryAutoSwitch {
        ambiguity_score_floor: None,
        dictionaries: entries
            .iter()
            .map(|&(name, variant, records, threshold)| Dictionary {
                target: Arc::new(Layout::new(name, variant, "", "")),
                trie: DictionaryTrie::read(records.as_bytes(), None).unwrap(),
                threshold,
            })
            .collect(),
        punctuation: PunctuationRuns::new(crate::config::DEFAULT_PUNCTUATION).unwrap(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word::{Stroke, WordStroke};
    use evdev::KeyCode;

    fn trie(records: &str) -> DictionaryTrie {
        DictionaryTrie::read(records.as_bytes(), None).unwrap()
    }

    #[test]
    fn reports_consumed_bytes_including_skipped_records_and_line_endings() {
        let skipped = "skip 1\r\n";
        let records = format!("{skipped}слово 2\nlast 3");
        let total = records.len() as u64;
        let mut events = Vec::new();
        let skip_pattern = Regex::new("^skip$").unwrap();
        DictionaryTrie::read_with_progress(
            records.as_bytes(),
            Some(&skip_pattern),
            total,
            |progress| events.push(progress),
        )
        .unwrap();

        let (building, reading) = events.split_last().unwrap();
        assert!(matches!(building, DictionaryLoadProgress::BuildingIndex));
        let bytes_read: Vec<_> = reading
            .iter()
            .map(|event| match event {
                DictionaryLoadProgress::Reading {
                    bytes_read,
                    total_bytes,
                } => {
                    assert_eq!(*total_bytes, total);
                    *bytes_read
                }
                DictionaryLoadProgress::BuildingIndex => panic!("expected reading progress"),
            })
            .collect();
        assert_eq!(
            bytes_read,
            [
                0,
                skipped.len() as u64,
                total - "last 3".len() as u64,
                total,
            ]
        );
    }

    #[test]
    fn aggregates_sorted_unicode_prefixes_and_normalizes_frequencies() {
        for records in [
            "коты 3\nall 2\nкот 1\na 2\n",
            "all 20\nкот 10\na 20\nкоты 30\n",
        ] {
            let trie = trie(records);
            assert_eq!(trie.score("КО"), -1.0);
            assert_eq!(trie.score("кот"), -1.0);
            assert_eq!(trie.score("коты"), 0.375_f64.log2());
            assert_eq!(trie.score("AL"), -2.0);
            assert_eq!(trie.score("кот#"), f64::NEG_INFINITY);
            for node in &trie.nodes {
                assert!(
                    trie.transitions[node.transitions.clone()]
                        .windows(2)
                        .all(|pair| pair[0].character < pair[1].character)
                );
            }
        }
    }

    #[test]
    fn loads_capitalized_words_without_a_skip_pattern() {
        let trie = trie("abc1 5\nApple 10\n");
        assert_eq!(trie.score("abc1 "), (5.0_f64 / 15.0).log2());
        assert_eq!(trie.transitions[0].character, 'A');
    }

    #[test]
    fn skips_matching_uppercase_words_before_calculating_the_total() {
        let pattern = compile_full_match_pattern(r".*\p{Lu}.*", "dict_skip_pattern").unwrap();
        let records = "cat 2\nCat 18446744073709551615\ncoDe 4\nКОТ 3\nмир 6\n";
        let trie = DictionaryTrie::read(records.as_bytes(), pattern.as_ref()).unwrap();
        assert_eq!(trie.score("CAT"), -2.0);
        assert_eq!(trie.score("мир"), 0.75_f64.log2());
        assert_eq!(trie.score("code"), f64::NEG_INFINITY);
        assert_eq!(trie.score("кот"), f64::NEG_INFINITY);
    }

    #[test]
    fn rejects_invalid_dictionary_records() {
        for records in [
            "",
            "a 0",
            "a -1",
            "bad",
            "a 1 2",
            "a 1\na 2",
            "a 18446744073709551615\nb 1",
        ] {
            assert!(
                DictionaryTrie::read(records.as_bytes(), None).is_err(),
                "{records:?}"
            );
        }
        assert!(DictionaryTrie::read(&b"a 1\n\xff 1"[..], None).is_err());
    }

    #[test]
    fn matches_the_whole_word_field() {
        for (pattern, records, retained, probability, skipped) in [
            (
                "cat",
                "cat 10\nbobcat 5\ncatfish 1\n",
                "bobcat ",
                5.0 / 6.0,
                "cat ",
            ),
            (
                "cat|cats",
                "cat 10\ncats 1\nbobcat 5\n",
                "bobcat ",
                1.0,
                "cats ",
            ),
            (
                "(?x)cat # comment",
                "cat 10\nbobcat 5\n",
                "bobcat ",
                1.0,
                "cat ",
            ),
            ("^10$", "10 2\napple 10\n", "apple ", 1.0_f64, "10 "),
        ] {
            let skip_pattern = compile_full_match_pattern(pattern, "dict_skip_pattern").unwrap();
            let trie = DictionaryTrie::read(records.as_bytes(), skip_pattern.as_ref()).unwrap();
            assert_eq!(trie.score(skipped), f64::NEG_INFINITY, "{pattern:?}");
            assert_eq!(trie.score(retained), probability.log2(), "{pattern:?}");
        }
    }

    fn load_dictionaries_with_settings(
        settings: &str,
        us_records: &str,
        ru_records: &str,
    ) -> io::Result<DictionaryAutoSwitch> {
        let directory = tempfile::tempdir().unwrap();
        let us = directory.path().join("us.txt");
        let ru = directory.path().join("ru.txt");
        std::fs::write(&us, us_records).unwrap();
        std::fs::write(&ru, ru_records).unwrap();
        let config = crate::config::from_contents(&format!(
            "switch_key = 'KEY_PAUSE'\nauto_switch = false\n{settings}\n\
             [[dict]]\ntarget = 'us'\nfile = {us:?}\nthreshold = 5.0\n\
             [[dict]]\ntarget = 'ru'\nfile = {ru:?}\nthreshold = 7.0\n"
        ))
        .unwrap();
        DictionaryAutoSwitch::load(
            &config,
            |name| Ok(Arc::new(Layout::new(name, "", "", ""))),
            || Ok(2),
        )
    }

    #[test]
    fn filters_both_dictionaries_when_loading_configuration() {
        let dictionaries = load_dictionaries_with_settings(
            "dict_skip_pattern = '.*[0-9].*'",
            "abc1 100\napple 10\n",
            "абв2 200\nабрикос 20\n",
        )
        .unwrap();
        for (dictionary, skipped, retained) in [
            (&dictionaries.dictionaries[0], "abc1", "apple "),
            (&dictionaries.dictionaries[1], "абв2", "абрикос "),
        ] {
            assert_eq!(dictionary.trie.score(skipped), f64::NEG_INFINITY);
            assert_eq!(dictionary.trie.score(retained), 0.0);
        }
    }

    #[test]
    fn reports_dictionary_filter_configuration_errors() {
        for (pattern, records, expected) in [
            ("[", "кот 1", "Invalid dict_skip_pattern"),
            (")(?:", "кот 1", "Invalid dict_skip_pattern"),
            (
                "кот",
                "кот 1",
                "ru.txt: Dictionary frequency sum must be positive",
            ),
            (
                "^bad$",
                "bad -1",
                "ru.txt: Invalid dictionary record on line 1",
            ),
        ] {
            let error = load_dictionaries_with_settings(
                &format!("dict_skip_pattern = '{pattern}'"),
                "cat 1",
                records,
            )
            .err()
            .expect("dictionary configuration should fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn loads_punctuation_from_configuration() {
        for (settings, expected) in [
            ("", [true, true, true, true, true, false]),
            (
                r"punctuation = '[./\\]'",
                [true, false, false, false, false, false],
            ),
            ("punctuation = ''", [false; 6]),
        ] {
            let dictionaries = load_dictionaries_with_settings(settings, "cat 1", "кот 1").unwrap();
            for (token, matches) in ["../", "!?", "~", "<", ">", "v1.2"]
                .into_iter()
                .zip(expected)
            {
                assert_eq!(
                    dictionaries
                        .punctuation
                        .as_ref()
                        .is_some_and(|punctuation| punctuation.full.is_match(token)),
                    matches,
                    "{settings}: {token:?}"
                );
            }
        }
        let error = load_dictionaries_with_settings("punctuation = '['", "cat 1", "кот 1")
            .err()
            .expect("invalid punctuation regex should fail to load");
        assert!(error.to_string().contains("Invalid punctuation"), "{error}");
        let runs = PunctuationRuns::new(";|ж").unwrap().unwrap();
        assert!(runs.full.is_match(";ж;"));
        assert!(!runs.full.is_match(";жa"));
    }

    #[test]
    fn custom_or_empty_punctuation_changes_dictionary_blocking() {
        let current = Layout::new("us", "", "", "");
        for (settings, should_switch) in [
            ("", false),
            (r"punctuation = '[./\\]'", true),
            ("punctuation = ''", true),
        ] {
            let dictionaries = load_dictionaries_with_settings(settings, "cat 1", "ёж 1").unwrap();
            let mut word = word_from_keys(&[KeyCode::KEY_GRAVE, KeyCode::KEY_SEMICOLON]);
            for is_first_space in [false, true] {
                word.trailing_spaces = usize::from(is_first_space);
                assert_eq!(
                    dictionaries
                        .target(&word, &current, is_first_space)
                        .as_deref()
                        .map(Layout::name),
                    should_switch.then_some("ru"),
                    "{settings}, first space: {is_first_space}"
                );
            }
        }
    }

    fn word_from_keys(keys: &[KeyCode]) -> Word {
        let mut word = Word::default();
        for &key in keys {
            push_key(&mut word, Stroke { key, shift: false });
        }
        word
    }

    fn push_key(word: &mut Word, stroke: Stroke) {
        word.entries.push(WordStroke {
            stroke,
            layout: None,
        });
    }

    #[test]
    fn scores_the_best_dictionary_key_with_or_without_leading_punctuation() {
        for (records, prefix_score, exact_score) in [
            (".git 2\ngithub 6", 0.75_f64.log2(), -2.0),
            (".git 6\ngit 2", 0.75_f64.log2(), 0.75_f64.log2()),
            ("git 8", 0.0, 0.0),
        ] {
            let dictionaries =
                test_dictionaries([("us", "", records, 5.0), ("ru", "", "юпше 1", 5.0)]);
            let word = word_from_keys(&[
                KeyCode::KEY_DOT,
                KeyCode::KEY_G,
                KeyCode::KEY_I,
                KeyCode::KEY_T,
            ]);
            assert_eq!(
                dictionaries.layout_score(&word, 0, false),
                Some((prefix_score, 0))
            );
            assert_eq!(
                dictionaries.layout_score(&word, 0, true),
                Some((exact_score, 0))
            );
        }
    }

    #[test]
    fn scores_only_a_nonempty_remainder_of_the_leading_punctuation_match() {
        for (settings, keys, score) in [
            (
                "",
                &[
                    KeyCode::KEY_DOT,
                    KeyCode::KEY_G,
                    KeyCode::KEY_I,
                    KeyCode::KEY_T,
                ][..],
                0.0,
            ),
            (
                "punctuation = ''",
                &[
                    KeyCode::KEY_DOT,
                    KeyCode::KEY_G,
                    KeyCode::KEY_I,
                    KeyCode::KEY_T,
                ][..],
                f64::NEG_INFINITY,
            ),
            (
                r"punctuation = '\.|,'",
                &[
                    KeyCode::KEY_DOT,
                    KeyCode::KEY_DOT,
                    KeyCode::KEY_G,
                    KeyCode::KEY_I,
                    KeyCode::KEY_T,
                ][..],
                0.0,
            ),
            (
                r"punctuation = '\.|,'",
                &[
                    KeyCode::KEY_A,
                    KeyCode::KEY_COMMA,
                    KeyCode::KEY_G,
                    KeyCode::KEY_I,
                    KeyCode::KEY_T,
                ][..],
                f64::NEG_INFINITY,
            ),
            (
                "",
                &[KeyCode::KEY_SEMICOLON, KeyCode::KEY_SEMICOLON][..],
                f64::NEG_INFINITY,
            ),
        ] {
            let dictionaries = load_dictionaries_with_settings(settings, "git 1", "кот 1").unwrap();
            let word = word_from_keys(keys);
            for is_first_space in [false, true] {
                assert_eq!(
                    dictionaries.layout_score(&word, 0, is_first_space),
                    Some((score, 0))
                );
            }
        }
    }

    #[test]
    fn scores_trailing_weak_punctuation_as_an_exact_trimmed_key() {
        use crate::history::{History, HistoryEvent};
        use crate::punctuation::Punctuation;
        use KeyCode as K;

        let dictionaries = test_dictionaries([
            ("us", "", "git 2\ngithub 5\ngit;more 1", 5.0),
            ("ru", "", "пшежня 1", 5.0),
        ]);
        let [us, ru] = dictionaries
            .layout_pair()
            .unwrap()
            .map(|layout| layout.as_ref());
        let mut punctuation = Punctuation::new(crate::config::DEFAULT_PUNCTUATION).unwrap();
        punctuation.update_layouts(vec![us.clone(), ru.clone()]);
        for end in [K::KEY_SPACE, K::KEY_SLASH] {
            for leading in [None, Some(K::KEY_SEMICOLON)] {
                let mut history = History::default();
                for key in
                    leading
                        .into_iter()
                        .chain([K::KEY_G, K::KEY_I, K::KEY_T, K::KEY_SEMICOLON])
                {
                    assert!(matches!(
                        history.process(key, false, None, &punctuation),
                        HistoryEvent::Stroke
                    ));
                }
                let word = history.latest_word().unwrap();
                assert_eq!(dictionaries.layout_score(word, 0, false), Some((-2.0, 0)));
                if leading.is_none() {
                    assert_eq!(dictionaries.layout_score(word, 1, false), Some((0.0, 0)));
                }
                assert!(matches!(
                    history.process(end, false, None, &punctuation),
                    HistoryEvent::WordEnd
                ));
                assert_eq!(
                    dictionaries.layout_score(history.latest_word().unwrap(), 0, true),
                    Some((-2.0, 0))
                );
            }
        }
    }

    #[test]
    fn only_configured_trailing_punctuation_enables_exact_trimmed_lookup() {
        use KeyCode as K;

        for (settings, score) in [(r"punctuation = '\$'", 0.0), ("", f64::NEG_INFINITY)] {
            let dictionaries = load_dictionaries_with_settings(settings, "git 1", "кот 1").unwrap();
            let mut word = word_from_keys(&[K::KEY_G, K::KEY_I, K::KEY_T]);
            push_key(
                &mut word,
                Stroke {
                    key: K::KEY_4,
                    shift: true,
                },
            );
            assert_eq!(dictionaries.layout_score(&word, 0, false), Some((score, 0)));
        }

        let dictionaries = test_dictionaries([
            ("us", "", "github 1\ngit;hub 1", 5.0),
            ("ru", "", "кот 1", 5.0),
        ]);
        let mut word = word_from_keys(&[K::KEY_G, K::KEY_I, K::KEY_T, K::KEY_SEMICOLON]);
        assert_eq!(
            dictionaries
                .punctuation
                .as_ref()
                .unwrap()
                .trim_edges("git;"),
            ("git", true)
        );
        assert_eq!(dictionaries.layout_score(&word, 0, false), Some((-1.0, 0)));
        assert_eq!(
            dictionaries.layout_score(&word, 0, true),
            Some((f64::NEG_INFINITY, 0))
        );
        for key in [K::KEY_H, K::KEY_U, K::KEY_B] {
            push_key(&mut word, Stroke { key, shift: false });
        }
        assert_eq!(
            dictionaries
                .punctuation
                .as_ref()
                .unwrap()
                .trim_edges("git;hub"),
            ("git;hub", false)
        );
        assert_eq!(dictionaries.layout_score(&word, 0, true), Some((-1.0, 0)));
    }

    #[test]
    fn splits_in_the_layout_with_internal_punctuation_and_scores_the_last_part_exactly_at_the_end()
    {
        use KeyCode as K;

        let dictionaries = test_dictionaries([
            ("us", "", "foo 4\nbar 1\nbart 3", 5.0),
            ("ru", "", "кот 1", 5.0),
        ]);
        let ru = Layout::new("ru", "", "", "");
        let mut word = word_from_keys(&[
            K::KEY_F,
            K::KEY_O,
            K::KEY_O,
            K::KEY_DOT,
            K::KEY_B,
            K::KEY_A,
            K::KEY_R,
        ]);
        assert_eq!(dictionaries.layout_score(&word, 0, false), Some((-1.0, 0)));
        assert_eq!(dictionaries.layout_score(&word, 0, true), Some((-3.0, 0)));
        assert_eq!(
            dictionaries.layout_score(&word, 1, false),
            Some((f64::NEG_INFINITY, 0))
        );
        assert_eq!(dictionaries.target(&word, &ru, false).unwrap().name(), "us");

        push_key(
            &mut word,
            Stroke {
                key: K::KEY_SEMICOLON,
                shift: false,
            },
        );
        assert_eq!(dictionaries.layout_score(&word, 0, false), Some((-3.0, 0)));
    }

    #[test]
    fn splits_at_different_characters_in_the_two_layouts() {
        use KeyCode as K;

        let dictionaries = test_dictionaries([
            ("us", "", "foo 2\nbar$baz 6", 5.0),
            ("ru", "", "ащщжифк 4\nифя 1\nя 3", 5.0),
        ]);
        let us = Layout::new("us", "", "", "");
        let mut word = word_from_keys(&[
            K::KEY_F,
            K::KEY_O,
            K::KEY_O,
            K::KEY_SEMICOLON,
            K::KEY_B,
            K::KEY_A,
            K::KEY_R,
        ]);
        push_key(
            &mut word,
            Stroke {
                key: K::KEY_4,
                shift: true,
            },
        );
        for key in [K::KEY_B, K::KEY_A, K::KEY_Z] {
            push_key(&mut word, Stroke { key, shift: false });
        }
        let comparison = dictionaries.comparison(&word, &us, false).unwrap();
        assert_eq!(comparison.current_base_score, -2.0);
        assert_eq!(comparison.alternative_base_score, -3.0);
        assert_eq!(comparison.delta_score, -1.0);
    }

    #[test]
    fn scores_split_parts_by_total_and_falls_back_when_a_part_is_missing() {
        use KeyCode as K;

        let mut dictionaries = test_dictionaries([
            ("us", "", "a 1\na.b 1\nb 2\nz 4", 5.0),
            ("ru", "", "кот 1", 5.0),
        ]);
        let word = word_from_keys(&[K::KEY_A, K::KEY_DOT, K::KEY_B]);
        dictionaries.learned.increment("us", "a").unwrap();
        assert_eq!(dictionaries.layout_score(&word, 0, true), Some((-3.0, 1)));
        dictionaries.learned.increment("us", "a").unwrap();
        assert_eq!(dictionaries.layout_score(&word, 0, true), Some((-2.0, 0)));

        let dictionaries = test_dictionaries([
            ("us", "", "a.b 1\napple 1\nb 2\nz 4", 5.0),
            ("ru", "", "кот 1", 5.0),
        ]);
        assert_eq!(dictionaries.layout_score(&word, 0, false), Some((-3.0, 0)));
    }

    #[test]
    fn trailing_weak_punctuation_looks_up_exact_learned_counts() {
        use KeyCode as K;

        let mut dictionaries =
            test_dictionaries([("us", "", "git 1\nz 1", 5.0), ("ru", "", "кот 1", 5.0)]);
        let plain = word_from_keys(&[K::KEY_G, K::KEY_I, K::KEY_T]);
        let mut punctuated = plain.clone();
        push_key(
            &mut punctuated,
            Stroke {
                key: K::KEY_SEMICOLON,
                shift: false,
            },
        );

        dictionaries.learned.increment("us", "git;").unwrap();
        assert_eq!(dictionaries.layout_score(&plain, 0, true), Some((-1.0, 0)));

        for key in ["git", "git", "github", "github", "github"] {
            dictionaries.learned.increment("us", key).unwrap();
        }
        assert_eq!(dictionaries.layout_score(&plain, 0, false), Some((-1.0, 6)));
        for is_word_end in [false, true] {
            assert_eq!(
                dictionaries.layout_score(&punctuated, 0, is_word_end),
                Some((-1.0, 2))
            );
        }
    }

    #[test]
    fn comparison_uses_dictionary_and_learned_scores_from_the_same_key() {
        use KeyCode as K;
        use std::cmp::Ordering::Less;

        let us = Layout::new("us", "", "", "");
        let ru = Layout::new("ru", "", "", "");
        let mut dictionaries = test_dictionaries([
            ("us", "", ";git 1\ngit 2\ngithub 5", 5.0),
            ("ru", "", "жпше 1\nя 7", 5.0),
        ]);
        let word = word_from_keys(&[K::KEY_SEMICOLON, K::KEY_G, K::KEY_I, K::KEY_T]);

        for key in ["git", "git", "github", "github", "github"] {
            dictionaries.learned.increment("us", key).unwrap();
        }
        for (repetitions, exact_score, exact_count) in [(1, -2.0, 2), (5, -3.0, 6)] {
            for _ in 0..repetitions {
                dictionaries.learned.increment("us", ";git").unwrap();
            }
            for (is_word_end, us_score, count) in [
                (false, 0.875_f64.log2(), 5),
                (true, exact_score, exact_count),
            ] {
                let comparison = dictionaries.comparison(&word, &us, is_word_end).unwrap();
                assert_eq!(comparison.current_base_score, us_score);
                assert_eq!(comparison.alternative_base_score, -3.0);
                assert_eq!(comparison.learned_difference(), (Less, count));
                assert_eq!(comparison.delta_score, -3.0 - (us_score + count as f64));
                let reverse = dictionaries.comparison(&word, &ru, is_word_end).unwrap();
                assert_eq!(reverse.delta_score, -comparison.delta_score);
            }
        }
    }

    #[test]
    fn a_learned_only_key_can_win_but_the_full_key_wins_ties() {
        use KeyCode as K;

        let us = Layout::new("us", "", "", "");
        let mut dictionaries = test_dictionaries([
            ("us", "", ";git 1\nz 4095", 4.0),
            ("ru", "", "жпше 1\nя 127", 4.0),
        ]);
        let word = word_from_keys(&[K::KEY_SEMICOLON, K::KEY_G, K::KEY_I, K::KEY_T]);
        let mut count = 0;
        for (next_count, expected_base, expected_delta) in
            [(3, -12.0, 5.0), (6, -12.0, 5.0), (8, -18.0, 3.0)]
        {
            for _ in count..next_count {
                dictionaries.learned.increment("us", "git").unwrap();
            }
            count = next_count;
            let comparison = dictionaries.comparison(&word, &us, true).unwrap();
            assert_eq!(comparison.current_base_score, expected_base);
            assert_eq!(comparison.delta_score, expected_delta);
            assert_eq!(
                dictionaries.target(&word, &us, true).is_some(),
                expected_delta > 4.0
            );
        }
    }

    #[test]
    fn terminal_space_matches_only_the_complete_word() {
        let trie = trie("cat 2\ncats 6\ndog 8");
        assert_eq!(trie.score("cat"), -1.0);
        assert_eq!(trie.score("cat "), -3.0);
    }

    #[test]
    fn layout_indexes_score_the_same_key_in_both_tries() {
        let dictionary = trie("cat 2\ncats 6\ndog 8");
        let mut learned = LearnedRules::default();
        learned.increment("us", "cat").unwrap();
        learned.increment("us", "cats").unwrap();
        learned.increment("us", "owl").unwrap();
        let indexes = LayoutIndexes {
            dictionary: &dictionary,
            learned: learned.trie("us"),
        };
        for (key, separated, total) in [
            ("CAT", (-1.0, 2), 1.0),
            ("CAT ", (-3.0, 1), -2.0),
            ("owl ", (-18.0, 1), -17.0),
            ("CATZ", (f64::NEG_INFINITY, 0), f64::NEG_INFINITY),
        ] {
            assert_eq!(indexes.score_separated(key), separated);
            assert_eq!(indexes.score(key), total);
        }
    }

    #[test]
    fn comparison_is_unavailable_when_a_search_stroke_cannot_be_converted() {
        let dictionaries =
            test_dictionaries([("us", "intl", "apple 1", 5.0), ("ru", "", "я 1", 5.0)]);
        let word = word_from_keys(&[KeyCode::KEY_A, KeyCode::KEY_APOSTROPHE]);
        for (name, variant) in [("us", "intl"), ("ru", "")] {
            let current = Layout::new(name, variant, "", "");
            for is_word_end in [false, true] {
                assert!(
                    dictionaries
                        .comparison(&word, &current, is_word_end)
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn score_grows_as_both_prefix_frequencies_fall() {
        let dictionaries = test_dictionaries([
            ("us", "", "a 128\nap 128", 5.0),
            ("ru", "", "ф 31\nфз 1\nя 96", 7.0),
        ]);
        let current = Layout::new("ru", "", "", "");
        let mut word = Word::default();
        for (key, expected_score, expected_target) in [
            (KeyCode::KEY_A, 2.0, None),
            (KeyCode::KEY_P, 6.0, Some("us")),
        ] {
            push_key(&mut word, Stroke { key, shift: false });
            assert_eq!(
                dictionaries
                    .comparison(&word, &current, false)
                    .unwrap()
                    .delta_score,
                expected_score
            );
            let target = dictionaries.target(&word, &current, false);
            assert_eq!(target.as_deref().map(Layout::name), expected_target);
        }
    }

    #[test]
    fn ambiguity_uses_inclusive_delta_bounds_and_optional_lower_bound() {
        use DictionaryDecision::{Ambiguous, Keep, Switch};

        let mut dictionaries = test_dictionaries([
            ("us", "", "a 1\nb 4\nz 1048571", 2.0),
            ("ru", "", "ф 4\nи 1\nя 1048571", 2.0),
        ]);
        let us = Layout::new("us", "", "", "");
        let a = word_from_keys(&[KeyCode::KEY_A]);
        let b = word_from_keys(&[KeyCode::KEY_B]);
        for (word, delta_score) in [(&a, 2.0), (&b, -2.0)] {
            assert_eq!(dictionaries.decision(word, &us, false, |_| 0.0), Keep);
            assert_eq!(
                dictionaries.decision(word, &us, true, |_| 0.0),
                Ambiguous(delta_score)
            );
        }

        dictionaries.ambiguity_score_floor = Some(-1.0);
        assert_eq!(
            dictionaries.decision(&a, &us, true, |_| 0.0),
            Ambiguous(2.0)
        );
        assert_eq!(
            dictionaries.decision(&a, &us, true, |_| -4.0),
            Ambiguous(2.0)
        );
        assert_eq!(dictionaries.decision(&b, &us, true, |_| 2.0), Keep);

        dictionaries.ambiguity_score_floor = Some(-2.0);
        assert_eq!(
            dictionaries.decision(&b, &us, true, |_| 0.0),
            Ambiguous(-2.0)
        );
        dictionaries.learned.increment("ru", "ф").unwrap();
        dictionaries.learned.increment("us", "b").unwrap();
        assert!(matches!(
            dictionaries.decision(&a, &us, true, |_| 0.0),
            Switch(_)
        ));
        assert_eq!(dictionaries.decision(&b, &us, true, |_| 0.0), Keep);

        dictionaries.ambiguity_score_floor = Some(-3.0);
        assert_eq!(
            dictionaries.decision(&b, &us, true, |_| 0.0),
            Ambiguous(-3.0)
        );
    }

    #[test]
    fn ambiguity_accepts_a_learned_baseline_and_rejects_two_missing_scores() {
        use DictionaryDecision::{Ambiguous, Keep};

        let mut dictionaries =
            test_dictionaries([("us", "", "a 1\nz 131071", 2.0), ("ru", "", "я 1", 2.0)]);
        let us = Layout::new("us", "", "", "");
        let a = word_from_keys(&[KeyCode::KEY_A]);
        let missing = word_from_keys(&[KeyCode::KEY_B]);
        assert_eq!(dictionaries.decision(&a, &us, true, |_| 0.0), Keep);
        assert_eq!(dictionaries.decision(&missing, &us, true, |_| 3.0), Keep);

        dictionaries.learned.increment("ru", "ф").unwrap();
        assert_eq!(
            dictionaries.decision(&a, &us, true, |_| 0.0),
            Ambiguous(0.0)
        );
        assert_eq!(dictionaries.decision(&missing, &us, true, |_| 0.0), Keep);
    }

    #[test]
    fn two_dictionaries_use_strict_destination_thresholds_in_both_directions() {
        for (source, destination, key) in
            [("ru", "us", KeyCode::KEY_A), ("us", "ru", KeyCode::KEY_B)]
        {
            for (source_threshold, destination_threshold, expected) in [
                (4.0, 2.0, Some(destination)),
                (2.0, 3.0, None),
                (2.0, 4.0, None),
            ] {
                let thresholds = |name| {
                    if name == source {
                        source_threshold
                    } else {
                        destination_threshold
                    }
                };
                let dictionaries = test_dictionaries([
                    ("us", "", "a 8\nb 1", thresholds("us")),
                    ("ru", "", "ф 1\nи 8", thresholds("ru")),
                ]);
                let mut word = Word::default();
                push_key(&mut word, Stroke { key, shift: true });
                let target = dictionaries.target(&word, &Layout::new(source, "", "", ""), false);
                assert_eq!(target.as_deref().map(Layout::name), expected);
            }
        }
    }

    #[test]
    fn zero_frequencies_only_switch_to_a_present_prefix() {
        let dictionaries =
            test_dictionaries([("us", "", "a 1\nc 1000", 5.0), ("ru", "", "и 1", 7.0)]);
        let current = Layout::new("ru", "", "", "");
        for (key, expected_score, expected_target) in [
            (KeyCode::KEY_A, Some(f64::INFINITY), Some("us")),
            (KeyCode::KEY_B, Some(f64::NEG_INFINITY), None),
            (KeyCode::KEY_X, Some(0.0), None),
        ] {
            let mut word = Word::default();
            push_key(&mut word, Stroke { key, shift: false });
            assert_eq!(
                dictionaries
                    .comparison(&word, &current, false)
                    .map(|comparison| comparison.delta_score),
                expected_score
            );
            let target = dictionaries.target(&word, &current, false);
            assert_eq!(target.as_deref().map(Layout::name), expected_target);
        }
    }

    #[test]
    fn first_space_scores_complete_words() {
        let dictionaries = test_dictionaries([
            ("us", "", "a 8\nap 1\nbp 2\nc 8\nd 13", 3.0),
            ("ru", "", "ф 1\nфз 8\nиз 2\nсз 8\nя 13", 3.0),
        ]);
        let current = Layout::new("ru", "", "", "");
        for (key, expected_score, expected_target) in [
            (KeyCode::KEY_A, Some(3.0), None),
            (KeyCode::KEY_B, Some(0.0), None),
            (KeyCode::KEY_C, Some(f64::INFINITY), Some("us")),
        ] {
            let mut word = Word::default();
            push_key(&mut word, Stroke { key, shift: false });
            let prefix_score = dictionaries
                .comparison(&word, &current, false)
                .unwrap()
                .delta_score;
            assert_eq!(prefix_score, 0.0);
            let score = dictionaries
                .comparison(&word, &current, true)
                .map(|comparison| comparison.delta_score);
            assert_eq!(score, expected_score);
            let target = dictionaries.target(&word, &current, true);
            assert_eq!(target.as_deref().map(Layout::name), expected_target);
        }
    }

    #[test]
    fn learning_assigns_a_baseline_and_moves_the_same_word_in_both_directions() {
        let us = Layout::new("us", "", "", "");
        let ru = Layout::new("ru", "", "", "");
        let stroke = Stroke {
            key: KeyCode::KEY_A,
            shift: false,
        };
        for scale in [1, 10] {
            let mut dictionaries = test_dictionaries([
                ("us", "", &format!("a {scale}\nb {}", 65_535 * scale), 2.0),
                ("ru", "", &format!("я {}\nф 0", 8 * scale), 2.0),
            ]);
            let mut word = Word::default();
            push_key(&mut word, stroke);
            assert_eq!(
                dictionaries
                    .comparison(&word, &ru, false)
                    .unwrap()
                    .delta_score,
                f64::INFINITY
            );

            dictionaries.learned.increment("ru", "ф").unwrap();
            assert_eq!(
                dictionaries.decision(&word, &ru, true, |_| 0.0),
                DictionaryDecision::Ambiguous(1.0)
            );
            for is_first_space in [false, true] {
                assert_eq!(
                    dictionaries.layout_score(&word, 1, is_first_space),
                    Some((LEARNED_BASE_SCORE, 1))
                );
                assert_eq!(
                    dictionaries
                        .comparison(&word, &ru, is_first_space)
                        .unwrap()
                        .delta_score,
                    1.0
                );
                assert_eq!(
                    dictionaries
                        .comparison(&word, &us, is_first_space)
                        .unwrap()
                        .delta_score,
                    -1.0
                );
                assert!(dictionaries.target(&word, &ru, is_first_space).is_none());
            }

            dictionaries.learned.increment("us", "a").unwrap();
            dictionaries.learned.increment("us", "a").unwrap();
            assert_eq!(
                dictionaries
                    .comparison(&word, &ru, true)
                    .unwrap()
                    .delta_score,
                3.0
            );
            assert_eq!(dictionaries.target(&word, &ru, true).unwrap().name(), "us");
        }
    }

    #[test]
    fn installer_template_loads_before_and_after_configuring_dictionaries() {
        let contents = include_str!("../installer/config.example.toml");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let load = || {
            let (config, _) = crate::config::load_from(&path).unwrap();
            assert!(!config.auto_switch);
            DictionaryAutoSwitch::load(
                &config,
                |name| Ok(Arc::new(Layout::new(name, "", "", ""))),
                || Ok(DICTIONARY_COUNT),
            )
            .unwrap()
        };
        std::fs::write(&path, contents).unwrap();
        assert!(!load().is_pair_loaded());

        let (settings, dictionaries) = contents.split_once("# [[dict]]").unwrap();
        let uncommented = dictionaries
            .lines()
            .map(|line| line.strip_prefix("# ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let files = directory.path().join("dictionaries");
        std::fs::create_dir(&files).unwrap();
        std::fs::write(files.join("en_full.txt"), "apple 1\n").unwrap();
        std::fs::write(files.join("ru_full.txt"), "яблоко 1\n").unwrap();
        std::fs::write(&path, format!("{settings}[[dict]]{uncommented}\n")).unwrap();
        assert!(load().is_pair_loaded());
    }

    #[test]
    fn validates_dictionary_settings_and_layout_count() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "true-switcher-dict-{}-{}.txt",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::write(&path, "a 1\n").unwrap();
        let dictionary = |target, file: &std::path::Path, threshold| {
            format!(
                "[[dict]]\ntarget = '{target}'\nfile = {:?}\nthreshold = {threshold}\n",
                file
            )
        };
        let compile = |contents: &str, count, is_enabled| {
            let settings = crate::config::from_contents(&format!(
                "switch_key = 'KEY_PAUSE'\nauto_switch = {is_enabled}\n{contents}",
            ))
            .unwrap();
            DictionaryAutoSwitch::load(
                &settings,
                |name| match name {
                    "us" | "ru" => Ok(Arc::new(Layout::new(name, "", "", ""))),
                    _ => Err(invalid("Invalid test layout")),
                },
                || Ok(count),
            )
        };
        let us = dictionary("us", &path, "5.0");
        let ru = dictionary("ru", &path, "7.0");
        let valid = format!("{us}{ru}");
        assert_eq!(compile(&valid, 2, true).unwrap().dictionaries.len(), 2);
        assert!(compile(&valid, 3, true).is_err());
        assert_eq!(compile(&valid, 3, false).unwrap().dictionaries.len(), 2);
        for empty in ["", "dict = []"] {
            assert!(compile(empty, 2, false).unwrap().dictionaries.is_empty());
            let error = compile(empty, 2, true).err().unwrap();
            assert_eq!(
                error.to_string(),
                "Exactly two dictionaries must be configured"
            );
        }
        for setting in ["dict_skip_pattern", "punctuation"] {
            let error = compile(&format!("{setting} = '['"), 2, false)
                .err()
                .unwrap();
            assert!(error.to_string().contains(setting));
        }
        for invalid_config in [us.clone(), format!("{us}{us}"), format!("{valid}{us}")] {
            for is_enabled in [true, false] {
                assert!(compile(&invalid_config, 2, is_enabled).is_err());
            }
        }
        for threshold in ["0", "-0.01", "nan", "inf", "-inf"] {
            let invalid_config = format!("{}{}", dictionary("us", &path, threshold), ru);
            assert!(compile(&invalid_config, 2, true).is_err());
        }
        let invalid_target = format!("{}{}", dictionary("invalid", &path, "5.0"), ru);
        assert!(compile(&invalid_target, 2, true).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(compile(&valid, 2, true).is_err());
    }
}
