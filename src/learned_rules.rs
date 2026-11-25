use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::layout::Layout;
use crate::word::{WORD_END, Word};

type Counts = BTreeMap<String, BTreeMap<String, i64>>;
pub(crate) const FILE_NAME: &str = "learned-rules.toml";

#[derive(Default)]
pub(crate) struct LearnedRules {
    path: Option<PathBuf>,
    counts: Counts,
    tries: BTreeMap<String, LearnedTrie>,
}

impl LearnedRules {
    pub(crate) fn load(path: PathBuf) -> io::Result<Self> {
        let read = || -> io::Result<Counts> {
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Counts::new());
                }
                Err(error) => return Err(error),
            };
            let counts: Counts = toml::from_str(&text).map_err(io::Error::other)?;
            if counts
                .values()
                .flatten()
                .any(|(word, &count)| count <= 0 || word.is_empty() || word.ends_with(' '))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Expected nonempty words without trailing spaces and positive counts",
                ));
            }
            Ok(counts)
        };
        let counts = read().map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        let tries = counts
            .iter()
            .map(|(layout, words)| {
                let mut trie = LearnedTrie::default();
                for (word, &count) in words {
                    trie.add(word, count as u128);
                }
                (layout.clone(), trie)
            })
            .collect();
        Ok(Self {
            path: Some(path),
            counts,
            tries,
        })
    }

    pub(crate) fn count(&self, layout: &Layout, word: &Word, is_first_space: bool) -> u128 {
        self.tries
            .get(layout.name())
            .and_then(|trie| {
                let node = word.strokes().try_fold(0, |node, stroke| {
                    let text = crate::xkb::stroke_text(layout, stroke).ok()?;
                    trie.advance(node, &text)
                })?;
                let node = if is_first_space {
                    trie.advance(node, WORD_END)?
                } else {
                    node
                };
                Some(trie.nodes[node].count)
            })
            .unwrap_or_default()
    }

    pub(crate) fn increment(&mut self, layout: &str, word: &str) -> io::Result<()> {
        let key = word.chars().flat_map(char::to_lowercase).collect();
        let mut counts = self.counts.clone();
        let count = counts
            .entry(layout.to_owned())
            .or_default()
            .entry(key)
            .or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Learned word count exceeds the TOML integer range"))?;
        if let Some(path) = &self.path {
            let text = toml::to_string(&counts).map_err(io::Error::other)?;
            let directory = path.parent().expect("learned rules path has a parent");
            let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
            temporary.write_all(text.as_bytes())?;
            temporary.persist(path).map_err(|error| error.error)?;
        }
        self.counts = counts;
        self.tries
            .entry(layout.to_owned())
            .or_default()
            .add(word, 1);
        Ok(())
    }
}

#[derive(Default)]
struct LearnedNode {
    count: u128,
    children: BTreeMap<char, usize>,
}

struct LearnedTrie {
    nodes: Vec<LearnedNode>,
}

impl Default for LearnedTrie {
    fn default() -> Self {
        Self {
            nodes: vec![LearnedNode::default()],
        }
    }
}

impl LearnedTrie {
    fn add(&mut self, word: &str, count: u128) {
        let mut node = 0;
        self.nodes[0].count += count;
        for character in word
            .chars()
            .flat_map(char::to_lowercase)
            .chain(WORD_END.chars())
        {
            node = match self.nodes[node].children.get(&character) {
                Some(&child) => child,
                None => {
                    let child = self.nodes.len();
                    self.nodes[node].children.insert(character, child);
                    self.nodes.push(LearnedNode::default());
                    child
                }
            };
            self.nodes[node].count += count;
        }
    }

    fn advance(&self, mut node: usize, text: &str) -> Option<usize> {
        for character in text.chars().flat_map(char::to_lowercase) {
            node = *self.nodes[node].children.get(&character)?;
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_lowercase_counts_for_each_layout_with_quoted_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let mut rules = LearnedRules::load(path.clone()).unwrap();
        rules.increment("us", "Don't").unwrap();
        rules.increment("ru", "DON'T").unwrap();
        rules.increment("us", "DON'T").unwrap();

        let rules = LearnedRules::load(path).unwrap();
        assert_eq!(
            rules.counts["us"],
            BTreeMap::from([("don't".to_owned(), 2)])
        );
        assert_eq!(
            rules.counts["ru"],
            BTreeMap::from([("don't".to_owned(), 1)])
        );
    }

    #[test]
    fn preserves_existing_keys_when_saving_a_lowercase_count() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fs::write(&path, "[ru]\n\"Кот\" = 2\n").unwrap();
        let mut rules = LearnedRules::load(path.clone()).unwrap();
        rules.increment("ru", "КОТ").unwrap();

        let rules = LearnedRules::load(path).unwrap();
        assert_eq!(rules.counts["ru"]["Кот"], 2);
        assert_eq!(rules.counts["ru"]["кот"], 1);
    }

    #[test]
    fn failed_write_does_not_increase_the_in_memory_count() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing/learned-rules.toml");
        let mut rules = LearnedRules::load(path).unwrap();
        assert!(rules.increment("us", "Hello").is_err());
        assert!(rules.counts.is_empty());
        assert!(rules.tries.is_empty());
    }

    #[test]
    fn finds_shared_prefixes_and_exact_words_after_loading_and_updating() {
        use crate::word::{Stroke, WordStroke};
        use evdev::KeyCode;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fs::write(&path, "[ru]\n\"кот\" = 2\n\"котик\" = 3\n").unwrap();
        let mut rules = LearnedRules::load(path).unwrap();
        let layout = Layout::new("ru", "", "", "");
        let word = |keys: &[KeyCode]| Word {
            entries: keys
                .iter()
                .map(|&key| WordStroke {
                    stroke: Stroke { key, shift: true },
                    layout: None,
                })
                .collect(),
            ..Word::default()
        };
        let cat = word(&[KeyCode::KEY_R, KeyCode::KEY_J, KeyCode::KEY_N]);
        let unrelated = word(&[KeyCode::KEY_U]);
        assert_eq!(rules.count(&layout, &cat, false), 5);
        assert_eq!(rules.count(&layout, &cat, true), 2);
        assert_eq!(rules.count(&layout, &unrelated, false), 0);
        rules.increment("ru", "кот").unwrap();
        assert_eq!(rules.count(&layout, &cat, false), 6);
        assert_eq!(rules.count(&layout, &cat, true), 3);
    }
}
