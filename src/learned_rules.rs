use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::word::WORD_END;

type Counts = BTreeMap<String, BTreeMap<String, i64>>;
pub(crate) const FILE_NAME: &str = "learned-rules.toml";

#[derive(Default)]
pub(crate) struct LearnedRules {
    path: Option<PathBuf>,
    counts: Counts,
    tries: BTreeMap<String, LearnedTrie>,
    strong_punctuation: BTreeMap<String, HashSet<char>>,
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
        let mut rules = Self {
            path: Some(path),
            counts,
            ..Self::default()
        };
        rules.rebuild_tries();
        Ok(rules)
    }

    pub(crate) fn set_strong_punctuation(&mut self, punctuation: BTreeMap<String, HashSet<char>>) {
        if self.strong_punctuation != punctuation {
            self.strong_punctuation = punctuation;
            self.rebuild_tries();
        }
    }

    fn rebuild_tries(&mut self) {
        self.tries = self
            .counts
            .iter()
            .map(|(layout, words)| {
                let mut trie = LearnedTrie::default();
                for (word, &count) in words {
                    trie.add(
                        trim_punctuation(word, self.strong_punctuation.get(layout)),
                        count as u128,
                    );
                }
                (layout.clone(), trie)
            })
            .collect();
    }

    pub(crate) fn trie(&self, layout: &str) -> Option<&LearnedTrie> {
        self.tries.get(layout)
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
        let indexed_word = trim_punctuation(word, self.strong_punctuation.get(layout));
        self.tries
            .entry(layout.to_owned())
            .or_default()
            .add(indexed_word, 1);
        Ok(())
    }
}

fn trim_punctuation<'a>(word: &'a str, punctuation: Option<&HashSet<char>>) -> &'a str {
    match punctuation {
        Some(punctuation) => word.trim_matches(|character| punctuation.contains(&character)),
        None => word,
    }
}

#[derive(Default)]
struct LearnedNode {
    count: u128,
    children: BTreeMap<char, usize>,
}

pub(crate) struct LearnedTrie {
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
        if word.is_empty() {
            return;
        }
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

    pub(crate) fn count(&self, text: &str) -> u128 {
        self.find_node(text)
            .map_or(0, |node| self.nodes[node].count)
    }

    fn find_node(&self, text: &str) -> Option<usize> {
        let mut node = 0;
        for character in text.chars().flat_map(char::to_lowercase) {
            node = *self.nodes[node].children.get(&character)?;
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;

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
    fn rebuilds_the_learned_index_from_original_keys_and_current_layouts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let original = "[us]\n\"/new\" = 3\nnew = 2\n\"<home/config/\" = 7\n\"///\" = 11\n";
        fs::write(&path, original).unwrap();
        let mut rules = LearnedRules::load(path.clone()).unwrap();
        let us = Layout::new("us", "", "pc105", "");
        let ru = Layout::new("ru", "", "pc105", "");
        let mut punctuation =
            crate::punctuation::Punctuation::new(crate::config::DEFAULT_PUNCTUATION).unwrap();
        punctuation.update_layouts(vec![us.clone(), ru]);
        rules.set_strong_punctuation(punctuation.strong_characters_by_layout());

        let indexed_count = |rules: &LearnedRules, key: &str| {
            let trie = &rules.tries["us"];
            trie.find_node(key).map(|node| trie.nodes[node].count)
        };
        assert_eq!(indexed_count(&rules, "new "), Some(5));
        assert_eq!(indexed_count(&rules, "/new "), None);
        assert_eq!(indexed_count(&rules, "<home/config "), Some(7));
        assert_eq!(indexed_count(&rules, " "), None);
        assert_eq!(indexed_count(&rules, ""), Some(12));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        rules.increment("us", "new").unwrap();
        assert_eq!(indexed_count(&rules, "new "), Some(6));
        assert_eq!(rules.counts["us"]["/new"], 3);
        assert_eq!(rules.counts["us"]["new"], 3);

        punctuation.update_layouts(vec![us]);
        rules.set_strong_punctuation(punctuation.strong_characters_by_layout());
        assert_eq!(indexed_count(&rules, "<home/config "), None);
        assert_eq!(indexed_count(&rules, "home/config "), Some(7));
        assert_eq!(rules.counts["us"]["<home/config/"], 7);
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        fs::write(&path, "[ru]\n\"кот\" = 2\n\"котик\" = 3\n").unwrap();
        let mut rules = LearnedRules::load(path).unwrap();
        let trie = rules.trie("ru").unwrap();
        assert_eq!(trie.count("КОТ"), 5);
        assert_eq!(trie.count("КОТ "), 2);
        assert_eq!(trie.count("г"), 0);
        rules.increment("ru", "кот").unwrap();
        let trie = rules.trie("ru").unwrap();
        assert_eq!(trie.count("КОТ"), 6);
        assert_eq!(trie.count("КОТ "), 3);
    }
}
