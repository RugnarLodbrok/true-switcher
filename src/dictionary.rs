use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::ops::Range;
use std::sync::Arc;

use regex::Regex;

use crate::config::Configuration;
use crate::layout::Layout;
use crate::word::{Stroke, WORD_END, Word};

pub(crate) const DICTIONARY_COUNT: usize = 2;

// Give learned-only matches the weight of a rare token (about 1 in 262,000).
// A fixed log2 probability keeps this weight independent of dictionary frequency units.
const LEARNED_BASE_SCORE: f64 = -18.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prefix {
    Node(usize),
    Missing,
    ConversionFailed,
}

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
    fn read(input: impl BufRead, skip_pattern: Option<&Regex>) -> io::Result<Self> {
        let mut records = Vec::new();
        let mut total = 0_u64;
        for (index, line) in input.lines().enumerate() {
            let line = line?;
            let (word, frequency) = parse_record(&line).ok_or_else(|| {
                invalid(format!("Invalid dictionary record on line {}", index + 1))
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

    fn advance(&self, prefix: Prefix, text: &str) -> Prefix {
        let Prefix::Node(mut node) = prefix else {
            return prefix;
        };
        for character in text.chars().flat_map(char::to_lowercase) {
            let transitions = &self.transitions[self.nodes[node].transitions.clone()];
            let Ok(index) = transitions.binary_search_by_key(&character, |edge| edge.character)
            else {
                return Prefix::Missing;
            };
            node = transitions[index].child;
        }
        Prefix::Node(node)
    }

    fn score(&self, prefix: Prefix) -> Option<f64> {
        match prefix {
            Prefix::Node(index) => Some(self.nodes[index].value),
            Prefix::Missing => Some(f64::NEG_INFINITY),
            Prefix::ConversionFailed => None,
        }
    }

    fn word_score(&self, prefix: Prefix, is_first_space: bool) -> Option<f64> {
        let prefix = if is_first_space {
            self.advance(prefix, WORD_END)
        } else {
            prefix
        };
        self.score(prefix)
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
    compile_anchored_pattern(pattern, setting, r"\z")
}

fn compile_anchored_pattern(
    pattern: &str,
    setting: &str,
    end_anchor: &str,
) -> io::Result<Option<Regex>> {
    if pattern.is_empty() {
        return Ok(None);
    }
    let invalid_pattern = |error| invalid(format!("Invalid {setting}: {error}"));
    Regex::new(pattern).map_err(invalid_pattern)?; // Reject unmatched groups before wrapping.
    // The newline terminates a trailing verbose-mode comment and is ignored otherwise.
    Regex::new(&format!("\\A(?:{pattern}(?x)\n){end_anchor}"))
        .map(Some)
        .map_err(invalid_pattern)
}

struct Dictionary {
    target: Arc<Layout>,
    trie: DictionaryTrie,
    threshold: f64,
}

struct DictionaryComparison<'a> {
    current_threshold: f64,
    alternative_dictionary: &'a Dictionary,
    delta_score: f64,
    current_score: f64,
    alternative_score: f64,
    learned_current: u128,
    learned_alternative: u128,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DictionaryDecision {
    Keep,
    Ambiguous,
    Switch(Arc<Layout>),
}

fn score_with_learned_baseline(score: f64, learned: u128) -> f64 {
    if score == f64::NEG_INFINITY && learned > 0 {
        LEARNED_BASE_SCORE
    } else {
        score
    }
}

#[derive(Default)]
pub(crate) struct DictionaryAutoSwitch {
    dictionaries: Vec<Dictionary>,
    ambiguity_score_floor: f64,
    full_punctuation: Option<Regex>,
    leading_punctuation: Option<Regex>,
    pub(crate) learned: crate::learned_rules::LearnedRules,
}

impl DictionaryAutoSwitch {
    pub(crate) fn load(
        settings: &Configuration,
        resolve: impl Fn(&str) -> io::Result<Arc<Layout>>,
        layout_count: impl FnOnce() -> io::Result<usize>,
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
        let full_punctuation = compile_full_match_pattern(&settings.punctuation, "punctuation")?;
        let leading_punctuation =
            compile_anchored_pattern(&settings.punctuation, "punctuation", "")?;
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
                .and_then(|file| DictionaryTrie::read(BufReader::new(file), skip_pattern.as_ref()))
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
            full_punctuation,
            leading_punctuation,
            ..Self::default()
        })
    }

    pub(crate) fn is_pair_loaded(&self) -> bool {
        self.dictionaries.len() == DICTIONARY_COUNT
    }

    pub(crate) fn push(&self, word: &mut Word, stroke: &Stroke) {
        word.dictionary_prefixes
            .resize_with(self.dictionaries.len(), Vec::new);
        for (dictionary, stack) in self.dictionaries.iter().zip(&mut word.dictionary_prefixes) {
            let previous = stack.last().copied().unwrap_or(Prefix::Node(0));
            let next = if previous == Prefix::ConversionFailed {
                previous
            } else {
                match crate::xkb::stroke_text(&dictionary.target, stroke) {
                    Ok(text) => dictionary.trie.advance(previous, &text),
                    Err(_) => Prefix::ConversionFailed,
                }
            };
            stack.push(next);
        }
    }

    fn layout_score(&self, word: &Word, index: usize, is_first_space: bool) -> Option<(f64, u128)> {
        let dictionary = &self.dictionaries[index];
        let prefix = *word.dictionary_prefixes.get(index)?.last()?;
        let mut score = dictionary.trie.word_score(prefix, is_first_space)?;
        if let Some(remainder_score) =
            self.punctuation_remainder_score(word, dictionary, is_first_space)
        {
            score = score.max(remainder_score);
        }
        let learned = self.learned.count(&dictionary.target, word, is_first_space);
        Some((score, learned))
    }

    fn punctuation_remainder_score(
        &self,
        word: &Word,
        dictionary: &Dictionary,
        is_first_space: bool,
    ) -> Option<f64> {
        let pattern = self.leading_punctuation.as_ref()?;
        let text = dictionary.target.to_text(word.strokes()).ok()?;
        let matched = pattern.find(&text)?;
        if matched.is_empty() || matched.end() == text.len() {
            return None;
        }
        let trie = &dictionary.trie;
        let prefix = trie.advance(Prefix::Node(0), &text[matched.end()..]);
        trie.word_score(prefix, is_first_space)
    }

    fn comparison(
        &self,
        word: &Word,
        current: &Layout,
        is_first_space: bool,
    ) -> Option<DictionaryComparison<'_>> {
        let current_index = self
            .dictionaries
            .iter()
            .position(|dictionary| dictionary.target.name() == current.name())?;
        let alternative_index = 1 - current_index;
        let (current_score, learned_current) =
            self.layout_score(word, current_index, is_first_space)?;
        let (alternative_score, learned_alternative) =
            self.layout_score(word, alternative_index, is_first_space)?;
        let current = score_with_learned_baseline(current_score, learned_current);
        let alternative = score_with_learned_baseline(alternative_score, learned_alternative);
        let delta_score = if current == f64::NEG_INFINITY && alternative == f64::NEG_INFINITY {
            0.0
        } else {
            let learned_delta = if learned_alternative >= learned_current {
                (learned_alternative - learned_current) as f64
            } else {
                -((learned_current - learned_alternative) as f64)
            };
            alternative - current + learned_delta
        };
        Some(DictionaryComparison {
            current_threshold: self.dictionaries[current_index].threshold,
            alternative_dictionary: &self.dictionaries[alternative_index],
            delta_score,
            current_score,
            alternative_score,
            learned_current,
            learned_alternative,
        })
    }

    pub(crate) fn decision(
        &self,
        word: &Word,
        current: &Layout,
        is_first_space: bool,
    ) -> DictionaryDecision {
        let Some(comparison) = self.comparison(word, current, is_first_space) else {
            return DictionaryDecision::Keep;
        };
        let alternative = comparison.alternative_dictionary;
        let is_switch = comparison.delta_score > alternative.threshold;
        let is_ambiguous = is_first_space
            && comparison.current_score >= self.ambiguity_score_floor
            && comparison.alternative_score >= self.ambiguity_score_floor
            && comparison.delta_score >= -comparison.current_threshold
            && comparison.delta_score <= alternative.threshold;
        if !is_switch && !is_ambiguous {
            return DictionaryDecision::Keep;
        }
        if self.full_punctuation.as_ref().is_some_and(|pattern| {
            current
                .to_text(word.strokes())
                .is_ok_and(|text| pattern.is_match(&text))
        }) {
            return DictionaryDecision::Keep;
        }
        if is_switch {
            DictionaryDecision::Switch(alternative.target.clone())
        } else if word.has_dead_key_in(current) || word.has_dead_key_in(&alternative.target) {
            DictionaryDecision::Keep
        } else {
            DictionaryDecision::Ambiguous
        }
    }

    #[cfg(test)]
    fn target(&self, word: &Word, current: &Layout, is_first_space: bool) -> Option<Arc<Layout>> {
        match self.decision(word, current, is_first_space) {
            DictionaryDecision::Switch(target) => Some(target),
            _ => None,
        }
    }

    pub(crate) fn log_score(&self, word: &Word, current: Option<&Layout>, is_first_space: bool) {
        if self.dictionaries.is_empty() || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let Some(comparison) =
            current.and_then(|layout| self.comparison(word, layout, is_first_space))
        else {
            log::debug!("dict switch score: unavailable");
            return;
        };

        let delta = comparison.delta_score;
        let current =
            score_with_learned_baseline(comparison.current_score, comparison.learned_current).abs();
        let alternative = score_with_learned_baseline(
            comparison.alternative_score,
            comparison.learned_alternative,
        )
        .abs();
        if current.is_infinite() && alternative.is_infinite() {
            log::debug!("dict switch score: {delta:+6.2} (both layout scores missing)");
            return;
        }

        let learned = comparison
            .learned_alternative
            .abs_diff(comparison.learned_current);
        let correction = if learned == 0 {
            String::new()
        } else {
            let sign = if comparison.learned_alternative > comparison.learned_current {
                '+'
            } else {
                '-'
            };
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
        ambiguity_score_floor: crate::config::DEFAULT_AMBIGUITY_SCORE_FLOOR,
        dictionaries: entries
            .iter()
            .map(|&(name, variant, records, threshold)| Dictionary {
                target: Arc::new(Layout::new(name, variant, "", "")),
                trie: DictionaryTrie::read(records.as_bytes(), None).unwrap(),
                threshold,
            })
            .collect(),
        full_punctuation: compile_full_match_pattern(
            crate::config::DEFAULT_PUNCTUATION,
            "punctuation",
        )
        .unwrap(),
        leading_punctuation: compile_anchored_pattern(
            crate::config::DEFAULT_PUNCTUATION,
            "punctuation",
            "",
        )
        .unwrap(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;

    fn trie(records: &str) -> DictionaryTrie {
        DictionaryTrie::read(records.as_bytes(), None).unwrap()
    }

    #[test]
    fn aggregates_sorted_unicode_prefixes_and_normalizes_frequencies() {
        for records in [
            "коты 3\nall 2\nкот 1\na 2\n",
            "all 20\nкот 10\na 20\nкоты 30\n",
        ] {
            let trie = trie(records);
            let root = Prefix::Node(0);
            assert_eq!(trie.score(trie.advance(root, "КО")), Some(-1.0));
            assert_eq!(trie.score(trie.advance(root, "кот")), Some(-1.0));
            assert_eq!(
                trie.score(trie.advance(root, "коты")),
                Some(0.375_f64.log2())
            );
            assert_eq!(trie.score(trie.advance(root, "AL")), Some(-2.0));
            assert_eq!(
                trie.score(trie.advance(root, "кот#")),
                Some(f64::NEG_INFINITY)
            );
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
        assert_eq!(
            trie.score(trie.advance(Prefix::Node(0), "abc1 ")),
            Some((5.0_f64 / 15.0).log2())
        );
        assert_eq!(trie.transitions[0].character, 'A');
    }

    #[test]
    fn skips_matching_uppercase_words_before_calculating_the_total() {
        let pattern = compile_full_match_pattern(r".*\p{Lu}.*", "dict_skip_pattern").unwrap();
        let records = "cat 2\nCat 18446744073709551615\ncoDe 4\nКОТ 3\nмир 6\n";
        let trie = DictionaryTrie::read(records.as_bytes(), pattern.as_ref()).unwrap();
        let root = Prefix::Node(0);
        assert_eq!(trie.score(trie.advance(root, "CAT")), Some(-2.0));
        assert_eq!(trie.score(trie.advance(root, "мир")), Some(0.75_f64.log2()));
        assert_eq!(trie.advance(root, "code"), Prefix::Missing);
        assert_eq!(trie.advance(root, "кот"), Prefix::Missing);
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
            let root = Prefix::Node(0);
            assert_eq!(trie.advance(root, skipped), Prefix::Missing, "{pattern:?}");
            assert_eq!(
                trie.score(trie.advance(root, retained)),
                Some(probability.log2()),
                "{pattern:?}"
            );
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
            let root = Prefix::Node(0);
            assert_eq!(dictionary.trie.advance(root, skipped), Prefix::Missing);
            assert_eq!(
                dictionary
                    .trie
                    .score(dictionary.trie.advance(root, retained)),
                Some(0.0)
            );
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
                r"punctuation = '[./\\]+'",
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
                        .full_punctuation
                        .as_ref()
                        .is_some_and(|regex| regex.is_match(token)),
                    matches,
                    "{settings}: {token:?}"
                );
            }
        }
        let error = load_dictionaries_with_settings("punctuation = '['", "cat 1", "кот 1")
            .err()
            .expect("invalid punctuation regex should fail to load");
        assert!(error.to_string().contains("Invalid punctuation"), "{error}");
    }

    #[test]
    fn custom_or_empty_punctuation_changes_dictionary_blocking() {
        use crate::word::WordStroke;

        let current = Layout::new("us", "", "", "");
        for (settings, should_switch) in [
            ("", false),
            (r"punctuation = '[./\\]+'", true),
            ("punctuation = ''", true),
        ] {
            let dictionaries = load_dictionaries_with_settings(settings, "cat 1", "ёж 1").unwrap();
            let mut word = Word::default();
            for key in [KeyCode::KEY_GRAVE, KeyCode::KEY_SEMICOLON] {
                let stroke = Stroke { key, shift: false };
                word.entries.push(WordStroke {
                    stroke,
                    layout: None,
                });
                dictionaries.push(&mut word, &stroke);
            }
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

    fn word_from_keys(dictionaries: &DictionaryAutoSwitch, keys: &[KeyCode]) -> Word {
        let mut word = Word::default();
        for &key in keys {
            let stroke = Stroke { key, shift: false };
            word.entries.push(crate::word::WordStroke {
                stroke,
                layout: None,
            });
            dictionaries.push(&mut word, &stroke);
        }
        word
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
            let word = word_from_keys(
                &dictionaries,
                &[
                    KeyCode::KEY_DOT,
                    KeyCode::KEY_G,
                    KeyCode::KEY_I,
                    KeyCode::KEY_T,
                ],
            );
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
                r"punctuation = '\.{2}|,+'",
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
                r"punctuation = '\.{2}|,+'",
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
            let word = word_from_keys(&dictionaries, keys);
            for is_first_space in [false, true] {
                assert_eq!(
                    dictionaries.layout_score(&word, 0, is_first_space),
                    Some((score, 0))
                );
            }
        }
    }

    #[test]
    fn leading_punctuation_affects_both_layout_scores_but_learned_counts_use_the_full_token() {
        let us = Layout::new("us", "", "", "");
        let ru = Layout::new("ru", "", "", "");
        let mut dictionaries =
            test_dictionaries([("us", "", "git 1", 5.0), ("ru", "", "юпше 1\nя 255", 5.0)]);
        let word = word_from_keys(
            &dictionaries,
            &[
                KeyCode::KEY_DOT,
                KeyCode::KEY_G,
                KeyCode::KEY_I,
                KeyCode::KEY_T,
            ],
        );

        dictionaries.learned.increment("us", "git").unwrap();
        for is_first_space in [false, true] {
            assert_eq!(
                dictionaries
                    .comparison(&word, &us, is_first_space)
                    .unwrap()
                    .delta_score,
                -8.0
            );
            assert_eq!(
                dictionaries
                    .comparison(&word, &ru, is_first_space)
                    .unwrap()
                    .delta_score,
                8.0
            );
        }

        dictionaries.learned.increment("us", ".git").unwrap();
        for is_first_space in [false, true] {
            assert_eq!(
                dictionaries
                    .comparison(&word, &us, is_first_space)
                    .unwrap()
                    .delta_score,
                -9.0
            );
            assert_eq!(
                dictionaries
                    .comparison(&word, &ru, is_first_space)
                    .unwrap()
                    .delta_score,
                9.0
            );
        }
    }

    #[test]
    fn terminal_space_matches_only_the_complete_word() {
        let trie = trie("cat 2\ncats 6\ndog 8");
        let prefix = trie.advance(Prefix::Node(0), "cat");
        assert_eq!(trie.score(prefix), Some(-1.0));
        assert_eq!(trie.score(trie.advance(prefix, WORD_END)), Some(-3.0));
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
            dictionaries.push(&mut word, &Stroke { key, shift: false });
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
    fn ambiguity_uses_both_raw_score_floors_and_inclusive_delta_bounds() {
        use DictionaryDecision::{Ambiguous, Keep, Switch};

        let mut dictionaries = test_dictionaries([
            ("us", "", "a 1\nb 4\nz 11", 2.0),
            ("ru", "", "ф 4\nи 1\nя 11", 2.0),
        ]);
        let us = Layout::new("us", "", "", "");
        let a = word_from_keys(&dictionaries, &[KeyCode::KEY_A]);
        let b = word_from_keys(&dictionaries, &[KeyCode::KEY_B]);
        dictionaries.ambiguity_score_floor = -4.0;
        for word in [&a, &b] {
            assert_eq!(dictionaries.decision(word, &us, false), Keep);
            assert_eq!(dictionaries.decision(word, &us, true), Ambiguous);
        }

        dictionaries.ambiguity_score_floor = -3.99;
        for word in [&a, &b] {
            assert_eq!(dictionaries.decision(word, &us, true), Keep);
        }

        dictionaries.ambiguity_score_floor = -4.0;
        dictionaries.learned.increment("ru", "ф").unwrap();
        dictionaries.learned.increment("us", "b").unwrap();
        assert!(matches!(dictionaries.decision(&a, &us, true), Switch(_)));
        assert_eq!(dictionaries.decision(&b, &us, true), Keep);
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
                dictionaries.push(&mut word, &Stroke { key, shift: true });
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
            dictionaries.push(&mut word, &Stroke { key, shift: false });
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
    fn first_space_scores_complete_words_without_changing_cached_prefixes() {
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
            dictionaries.push(&mut word, &Stroke { key, shift: false });
            let prefixes = word.dictionary_prefixes.clone();
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
            assert_eq!(word.dictionary_prefixes, prefixes);
        }
    }

    #[test]
    fn learning_assigns_a_baseline_and_moves_the_same_word_in_both_directions() {
        use crate::word::WordStroke;

        let us = Layout::new("us", "", "", "");
        let ru = Layout::new("ru", "", "", "");
        let stroke = Stroke {
            key: KeyCode::KEY_A,
            shift: false,
        };
        let original_word = Word {
            entries: vec![WordStroke {
                stroke,
                layout: None,
            }],
            ..Word::default()
        };
        for scale in [1, 10] {
            let mut dictionaries = test_dictionaries([
                ("us", "", &format!("a {scale}\nb {}", 65_535 * scale), 2.0),
                ("ru", "", &format!("я {}\nф 0", 8 * scale), 2.0),
            ]);
            let mut word = original_word.clone();
            dictionaries.push(&mut word, &stroke);
            assert_eq!(
                dictionaries
                    .comparison(&word, &ru, false)
                    .unwrap()
                    .delta_score,
                f64::INFINITY
            );

            dictionaries.learned.increment("ru", "ф").unwrap();
            assert_eq!(
                dictionaries.decision(&word, &ru, true),
                DictionaryDecision::Keep
            );
            for is_first_space in [false, true] {
                assert_eq!(
                    dictionaries.layout_score(&word, 1, is_first_space),
                    Some((f64::NEG_INFINITY, 1))
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
