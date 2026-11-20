use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::ops::Range;
use std::sync::Arc;

use crate::config::Configuration;
use crate::layout::Layout;
use crate::word::{Stroke, Word};

const MAX_LAYOUTS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prefix {
    Node(usize),
    Missing,
    ConversionFailed,
}

struct Node {
    frequency: u64,
    transitions: Range<usize>,
}

struct Transition {
    character: char,
    child: usize,
}

struct DictionaryTrie {
    nodes: Vec<Node>,
    transitions: Vec<Transition>,
}

impl DictionaryTrie {
    fn read(input: impl BufRead) -> io::Result<Self> {
        let mut records = Vec::new();
        let mut total = 0_u64;
        for (index, line) in input.lines().enumerate() {
            let line = line?;
            let (word, frequency) = parse_record(&line).ok_or_else(|| {
                invalid(format!("Invalid dictionary record on line {}", index + 1))
            })?;
            if word
                .chars()
                .any(|character| !character.to_lowercase().eq([character]))
            {
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
            frequency: total,
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
            for (index, character) in word.chars().enumerate() {
                if index >= shared {
                    let parent = *path.last().unwrap();
                    let child = nodes.len();
                    nodes.push(Node {
                        frequency: 0,
                        transitions: 0..0,
                    });
                    edges.push((parent, character, child));
                    path.push(child);
                }
                nodes[path[index + 1]].frequency += frequency;
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
            Prefix::Node(index) => {
                Some(self.nodes[index].frequency as f64 / self.nodes[0].frequency as f64)
            }
            Prefix::Missing => Some(0.0),
            Prefix::ConversionFailed => None,
        }
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

struct Dictionary {
    target: Arc<Layout>,
    trie: DictionaryTrie,
    slip: f64,
    grip: f64,
}

#[derive(Default)]
pub(crate) struct DictionaryAutoSwitch {
    dictionaries: Vec<Dictionary>,
}

impl DictionaryAutoSwitch {
    pub(crate) fn load(
        settings: &Configuration,
        resolve: impl Fn(&str) -> io::Result<Arc<Layout>>,
        layout_count: impl FnOnce() -> io::Result<usize>,
    ) -> io::Result<Self> {
        if settings.dict.len() > MAX_LAYOUTS {
            return Err(invalid("At most two dictionaries may be configured"));
        }
        if settings.auto_switch && !settings.dict.is_empty() && layout_count()? > MAX_LAYOUTS {
            return Err(invalid(
                "Dictionary auto-switch supports at most two KDE layouts",
            ));
        }
        let mut targets = HashSet::new();
        let mut dictionaries = Vec::new();
        for setting in &settings.dict {
            if !targets.insert(&setting.target) {
                return Err(invalid(format!(
                    "Duplicate dictionary target {:?}",
                    setting.target
                )));
            }
            for (name, value) in [("slip", setting.slip), ("grip", setting.grip)] {
                if !(0.0..=1.0).contains(&value) {
                    return Err(invalid(format!(
                        "dict {:?}: {name} must be between 0 and 1",
                        setting.target
                    )));
                }
            }
            let target = resolve(&setting.target)?;
            let trie = std::fs::File::open(&setting.file)
                .and_then(|file| DictionaryTrie::read(BufReader::new(file)))
                .map_err(|error| invalid(format!("{}: {error}", setting.file.display())))?;
            if settings.auto_switch {
                dictionaries.push(Dictionary {
                    target,
                    trie,
                    slip: setting.slip,
                    grip: setting.grip,
                });
            }
        }
        Ok(Self { dictionaries })
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

    fn score(&self, word: &Word, index: usize) -> Option<f64> {
        let prefix = *word.dictionary_prefixes.get(index)?.last()?;
        self.dictionaries[index].trie.score(prefix)
    }

    pub(crate) fn target(&self, word: &Word, current: &Layout) -> Option<Arc<Layout>> {
        let current_index = self
            .dictionaries
            .iter()
            .position(|dictionary| dictionary.target.name() == current.name());
        if self.dictionaries.len() == MAX_LAYOUTS && current_index.is_none() {
            return None;
        }
        let (index, alternative) = self
            .dictionaries
            .iter()
            .enumerate()
            .find(|(index, _)| Some(*index) != current_index)?;
        if let Some(current) = current_index
            && self.score(word, current)? >= self.dictionaries[current].slip
        {
            return None;
        }
        (self.score(word, index)? > alternative.grip).then(|| alternative.target.clone())
    }

    pub(crate) fn trace_scores(
        &self,
        word: &Word,
        current: Option<&Layout>,
        target: Option<&Layout>,
    ) {
        if self.dictionaries.is_empty() || !log::log_enabled!(log::Level::Trace) {
            return;
        }
        let scores = self
            .dictionaries
            .iter()
            .enumerate()
            .map(|(index, dictionary)| {
                let score = self
                    .score(word, index)
                    .map_or_else(|| "unavailable".to_owned(), |score| format!("{score:e}"));
                format!(
                    "{} score={} slip={:e} grip={:e}",
                    dictionary.target.name(),
                    score,
                    dictionary.slip,
                    dictionary.grip,
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        log::trace!(
            "Dictionary prefix scores: current={} [{}] switch_target={}",
            current.map_or("unknown", Layout::name),
            scores,
            target.map_or("none", Layout::name),
        );
    }
}

#[cfg(test)]
pub(crate) fn test_dictionaries(entries: &[(&str, &str, &str, f64, f64)]) -> DictionaryAutoSwitch {
    DictionaryAutoSwitch {
        dictionaries: entries
            .iter()
            .map(|&(name, variant, records, slip, grip)| Dictionary {
                target: Arc::new(Layout::new(name, variant, "", "")),
                trie: DictionaryTrie::read(records.as_bytes()).unwrap(),
                slip,
                grip,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;

    fn trie(records: &str) -> DictionaryTrie {
        DictionaryTrie::read(records.as_bytes()).unwrap()
    }

    #[test]
    fn aggregates_sorted_unicode_prefixes_and_normalizes_frequencies() {
        for records in [
            "коты 3\nall 2\nкот 1\na 2\n",
            "all 20\nкот 10\na 20\nкоты 30\n",
        ] {
            let trie = trie(records);
            let root = Prefix::Node(0);
            assert_eq!(trie.score(trie.advance(root, "КО")), Some(0.5));
            assert_eq!(trie.score(trie.advance(root, "кот")), Some(0.5));
            assert_eq!(trie.score(trie.advance(root, "коты")), Some(0.375));
            assert_eq!(trie.score(trie.advance(root, "AL")), Some(0.25));
            assert_eq!(trie.score(trie.advance(root, "кот#")), Some(0.0));
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
    fn skips_capitalized_words_before_calculating_the_total() {
        let trie = trie("cat 2\nCat 18446744073709551615\ncoDe 4\nКОТ 3\nмир 6\n");
        let root = Prefix::Node(0);
        assert_eq!(trie.nodes[0].frequency, 8);
        assert_eq!(trie.score(trie.advance(root, "CAT")), Some(0.25));
        assert_eq!(trie.score(trie.advance(root, "мир")), Some(0.75));
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
            "A 1",
            "a 1\na 2",
            "a 18446744073709551615\nb 1",
        ] {
            assert!(
                DictionaryTrie::read(records.as_bytes()).is_err(),
                "{records:?}"
            );
        }
        assert!(DictionaryTrie::read(&b"a 1\n\xff 1"[..]).is_err());
    }

    #[test]
    fn two_dictionaries_use_strict_thresholds_in_both_directions() {
        let score = |current, key, slip, grip| {
            let dictionaries = test_dictionaries(&[
                ("us", "", "a 1\nb 3", slip, grip),
                ("ru", "", "ф 1\nи 3", slip, grip),
            ]);
            let mut word = Word::default();
            dictionaries.push(&mut word, &Stroke { key, shift: true });
            dictionaries
                .target(&word, &Layout::new(current, "", "", ""))
                .map(|target| target.name().to_owned())
        };
        for (current, alternative) in [("us", "ru"), ("ru", "us")] {
            assert_eq!(
                score(current, KeyCode::KEY_A, 0.3, 0.2).as_deref(),
                Some(alternative)
            );
            assert_eq!(score(current, KeyCode::KEY_A, 0.25, 0.2), None);
            assert_eq!(score(current, KeyCode::KEY_A, 0.3, 0.25), None);
        }
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
        let dictionary = |target, file: &std::path::Path, slip, grip| {
            format!(
                "[[dict]]\ntarget = '{target}'\nfile = {:?}\nslip = {slip}\ngrip = {grip}\n",
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
        let valid = dictionary("us", &path, "0", "1");
        assert_eq!(compile(&valid, 2, true).unwrap().dictionaries.len(), 1);
        assert!(compile(&valid, 3, true).is_err());
        assert!(compile(&valid, 3, false).unwrap().dictionaries.is_empty());
        assert!(compile(&format!("{valid}{valid}"), 2, true).is_err());
        assert!(compile(&format!("{valid}{valid}{valid}"), 2, true).is_err());
        for threshold in ["-0.01", "1.01", "nan", "inf", "-inf"] {
            assert!(compile(&dictionary("us", &path, threshold, "0.5"), 2, true).is_err());
            assert!(compile(&dictionary("us", &path, "0.5", threshold), 2, true).is_err());
        }
        assert!(compile(&dictionary("invalid", &path, "0", "1"), 2, true).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(compile(&valid, 2, true).is_err());
    }
}
