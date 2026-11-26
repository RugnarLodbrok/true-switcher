use std::sync::Arc;
use std::{collections::HashMap, io};

use regex::RegexSet;

use crate::{
    config::{Configuration, RuntimeSettings},
    dictionary::{DictionaryAutoSwitch, DictionaryDecision},
    layout::Layout,
    word::Word,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSwitchKind {
    Regex,
    Dictionary,
}

pub(crate) enum AutoSwitchDecision {
    Keep,
    Ambiguous(f64),
    Switch(Arc<Layout>, AutoSwitchKind),
}

struct CompiledRule {
    target: Arc<Layout>,
    patterns: RegexSet,
}

#[derive(Default)]
pub struct AutoSwitch {
    rules: Vec<CompiledRule>,
    pub(crate) punctuation: crate::punctuation::Punctuation,
    pub(crate) dictionaries: DictionaryAutoSwitch,
}

impl AutoSwitch {
    pub fn new(
        settings: &Configuration,
        learned: crate::learned_rules::LearnedRules,
    ) -> io::Result<Self> {
        let mut rules = Self::compile(settings, crate::kde::target_layout)?;
        rules.dictionaries = DictionaryAutoSwitch::load(
            settings,
            crate::kde::target_layout,
            crate::kde::layout_count,
        )?;
        rules.dictionaries.learned = learned;
        Ok(rules)
    }

    pub(crate) fn compile(
        settings: &Configuration,
        resolve: impl Fn(&str) -> io::Result<Arc<Layout>>,
    ) -> io::Result<Self> {
        let mut rules = Vec::new();
        for rule in &settings.switch_rules {
            let patterns = RegexSet::new(&rule.patterns).map_err(|error| {
                io::Error::other(format!(
                    "Invalid switch_rules patterns for {}: {error}",
                    rule.target
                ))
            })?;
            let target = resolve(&rule.target)?;
            rules.push(CompiledRule { target, patterns });
        }
        Ok(Self {
            rules,
            punctuation: crate::punctuation::Punctuation::new(&settings.punctuation)?,
            ..Self::default()
        })
    }

    pub(crate) fn decision(
        &self,
        word: &Word,
        current: &Layout,
        settings: RuntimeSettings,
        is_word_end: bool,
        preceding_ambiguous_score: impl FnOnce(&Layout) -> f64,
    ) -> AutoSwitchDecision {
        if settings.is_pattern_switch
            && let Some(target) = self.target(word, current)
        {
            return AutoSwitchDecision::Switch(target, AutoSwitchKind::Regex);
        }
        if !settings.is_auto_switch {
            return AutoSwitchDecision::Keep;
        }

        match self
            .dictionaries
            .decision(word, current, is_word_end, preceding_ambiguous_score)
        {
            DictionaryDecision::Keep => AutoSwitchDecision::Keep,
            _ if settings.is_pattern_switch && self.matches_current(word, current) => {
                AutoSwitchDecision::Keep
            }
            DictionaryDecision::Switch(target) => {
                AutoSwitchDecision::Switch(target, AutoSwitchKind::Dictionary)
            }
            DictionaryDecision::Ambiguous(delta_score) => {
                AutoSwitchDecision::Ambiguous(delta_score)
            }
        }
    }

    pub(crate) fn update_layouts(&mut self, layouts: Vec<Layout>) {
        if self.punctuation.update_layouts(layouts) {
            self.dictionaries
                .learned
                .set_strong_punctuation(self.punctuation.strong_characters_by_layout());
        }
    }

    pub(crate) fn target(&self, word: &Word, current: &Layout) -> Option<Arc<Layout>> {
        let mut conversions = HashMap::new();
        self.rules.iter().find_map(|rule| {
            if rule.target.name() == current.name() {
                return None;
            }
            let text = conversions
                .entry(rule.target.as_ref())
                .or_insert_with(|| Self::pattern_text(word, &rule.target));
            rule.patterns
                .is_match(text.as_ref().ok()?)
                .then(|| rule.target.clone())
        })
    }

    pub(crate) fn matches_current(&self, word: &Word, current: &Layout) -> bool {
        let mut text = None;
        self.rules
            .iter()
            .filter(|rule| rule.target.name() == current.name())
            .any(|rule| {
                text.get_or_insert_with(|| Self::pattern_text(word, current))
                    .as_ref()
                    .is_ok_and(|text| rule.patterns.is_match(text))
            })
    }

    fn pattern_text(word: &Word, layout: &Layout) -> Result<String, crate::xkb::ConversionError> {
        let mut text = layout.to_text(word.strokes())?;
        text.extend(std::iter::repeat_n(' ', word.trailing_spaces));
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word::{Stroke, WordStroke};
    use evdev::KeyCode;

    fn parse_config(text: &str) -> Result<Configuration, String> {
        crate::config::from_contents(&format!("switch_key = 'KEY_PAUSE'\n{text}"))
    }

    fn resolve(name: &str) -> io::Result<Arc<Layout>> {
        Ok(Arc::new(Layout::new(name, "", "", "")))
    }

    fn word(keys: &[KeyCode]) -> Word {
        Word {
            entries: keys
                .iter()
                .map(|&key| WordStroke {
                    stroke: Stroke { key, shift: false },
                    layout: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn validates_rules_even_when_disabled() {
        let settings = parse_config("[[switch_rules]]\ntarget = 'us'\npatterns = ['[']").unwrap();
        assert!(AutoSwitch::compile(&settings, resolve).is_err());
        let settings = parse_config("[[switch_rules]]\ntarget = 'missing'\npatterns = []").unwrap();
        assert!(
            AutoSwitch::compile(&settings, |_| Err(io::Error::other("absent layout"))).is_err()
        );
    }

    #[test]
    fn reuses_successes_and_errors_only_within_one_target_call() {
        let bad = Arc::new(Layout::new(
            "us",
            "invalid-variant-for-true-switcher",
            "",
            "",
        ));
        let good = resolve("gb").unwrap();
        let rules = AutoSwitch {
            rules: [
                (bad.clone(), "a"),
                (bad, "a"),
                (good.clone(), "z"),
                (good, "a"),
            ]
            .into_iter()
            .map(|(target, pattern)| CompiledRule {
                target,
                patterns: RegexSet::new([pattern]).unwrap(),
            })
            .collect(),
            ..Default::default()
        };
        let word = word(&[KeyCode::KEY_A]);
        let current = resolve("ru").unwrap();
        crate::xkb::CONVERSIONS.with(|count| count.set(0));
        for expected in [2, 4] {
            assert_eq!(rules.target(&word, &current).unwrap().name(), "gb");
            assert_eq!(crate::xkb::CONVERSIONS.with(|count| count.get()), expected);
        }
    }

    #[test]
    fn conversion_failure_continues_rules_without_using_partial_text() {
        let settings = parse_config("pattern_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^a']\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['^a']\n[[switch_rules]]\ntarget = 'ru'\npatterns = ['^ф']").unwrap();
        let switch = AutoSwitch::compile(&settings, |name| {
            Ok(Arc::new(Layout::new(
                name,
                if name == "us" { "intl" } else { "" },
                "",
                "",
            )))
        })
        .unwrap();
        let word = word(&[KeyCode::KEY_A, KeyCode::KEY_APOSTROPHE]);
        let current = resolve("de").unwrap();
        assert_eq!(switch.target(&word, &current).unwrap().name(), "gb");
    }

    #[test]
    fn matches_converted_prefix_and_respects_rule_order_and_current_layout() {
        let settings = parse_config("pattern_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^/[a-z]']\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['n']").unwrap();
        let switch = AutoSwitch::compile(&settings, resolve).unwrap();
        let ru = resolve("ru").unwrap();
        let prefix = word(&[KeyCode::KEY_SLASH, KeyCode::KEY_N]);
        assert_eq!(switch.target(&prefix, &ru).unwrap().name(), "us");
        assert_eq!(
            switch
                .target(&prefix, &resolve("us").unwrap())
                .unwrap()
                .name(),
            "gb"
        );
        let embedded = word(&[KeyCode::KEY_A, KeyCode::KEY_SLASH, KeyCode::KEY_N]);
        assert_eq!(switch.target(&embedded, &ru).unwrap().name(), "gb");
        assert!(switch.target(&word(&[KeyCode::KEY_SLASH]), &ru).is_none());
    }

    #[test]
    fn matches_the_actual_number_of_trailing_spaces() {
        let settings = parse_config(
            "pattern_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^foo $', '^foo  $']",
        )
        .unwrap();
        let rules = AutoSwitch::compile(&settings, resolve).unwrap();
        let current = resolve("ru").unwrap();
        let mut word = word(&[KeyCode::KEY_F, KeyCode::KEY_O, KeyCode::KEY_O]);
        assert!(rules.target(&word, &current).is_none());
        word.trailing_spaces = 1;
        assert_eq!(rules.target(&word, &current).unwrap().name(), "us");
        word.trailing_spaces = 2;
        assert_eq!(rules.target(&word, &current).unwrap().name(), "us");
        word.trailing_spaces = 3;
        assert!(rules.target(&word, &current).is_none());
    }

    #[test]
    fn matches_current_layout_rules_with_the_same_trailing_space_as_forward_rules() {
        let settings = parse_config(
            "pattern_switch = true\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['^a$']\n\
             [[switch_rules]]\ntarget = 'us'\npatterns = ['^z $']\n\
             [[switch_rules]]\ntarget = 'us'\npatterns = ['^a $']",
        )
        .unwrap();
        let rules = AutoSwitch::compile(&settings, resolve).unwrap();
        let current = resolve("us").unwrap();
        let mut word = word(&[KeyCode::KEY_A]);
        assert!(!rules.matches_current(&word, &current));
        assert_eq!(rules.target(&word, &current).unwrap().name(), "gb");

        word.trailing_spaces = 1;
        assert!(rules.matches_current(&word, &current));
        assert!(!rules.matches_current(&word, &resolve("gb").unwrap()));
        assert!(rules.target(&word, &current).is_none());
    }
}
