use std::sync::Arc;
use std::{collections::HashMap, io};

use regex::RegexSet;

use crate::{config::Configuration, layout::Layout, word::Word};

struct CompiledRule {
    target: Arc<Layout>,
    patterns: RegexSet,
}

#[derive(Default)]
pub struct AutoSwitchRules {
    rules: Vec<CompiledRule>,
}

impl AutoSwitchRules {
    pub fn new(settings: &Configuration) -> io::Result<Self> {
        Self::compile(settings, crate::kde::target_layout)
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
            if settings.auto_switch {
                rules.push(CompiledRule { target, patterns });
            }
        }
        Ok(Self { rules })
    }

    pub fn target(&self, word: &Word, current: &Layout) -> Option<Arc<Layout>> {
        let mut conversions = HashMap::new();
        self.rules.iter().find_map(|rule| {
            if rule.target.name() == current.name() {
                return None;
            }
            let text = conversions.entry(rule.target.as_ref()).or_insert_with(|| {
                rule.target.to_text(word.strokes()).map(|mut text| {
                    if word.trailing_spaces > 0 {
                        text.push(' ');
                    }
                    text
                })
            });
            rule.patterns
                .is_match(text.as_ref().ok()?)
                .then(|| rule.target.clone())
        })
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
        assert!(AutoSwitchRules::compile(&settings, resolve).is_err());
        let settings = parse_config("[[switch_rules]]\ntarget = 'missing'\npatterns = []").unwrap();
        assert!(
            AutoSwitchRules::compile(&settings, |_| Err(io::Error::other("absent layout")))
                .is_err()
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
        let rules = AutoSwitchRules {
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
        let settings = parse_config("auto_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^a']\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['^a']\n[[switch_rules]]\ntarget = 'ru'\npatterns = ['^ф']").unwrap();
        let switch = AutoSwitchRules::compile(&settings, |name| {
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
        let settings = parse_config("auto_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^/[a-z]']\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['n']").unwrap();
        let switch = AutoSwitchRules::compile(&settings, resolve).unwrap();
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
    fn matches_one_trailing_space_only_after_successful_conversion() {
        let settings = parse_config(
            "auto_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^foo $', '^foo  $']",
        )
        .unwrap();
        let rules = AutoSwitchRules::compile(&settings, resolve).unwrap();
        let current = resolve("ru").unwrap();
        let mut word = word(&[KeyCode::KEY_F, KeyCode::KEY_O, KeyCode::KEY_O]);
        assert!(rules.target(&word, &current).is_none());
        word.trailing_spaces = 1;
        assert_eq!(rules.target(&word, &current).unwrap().name(), "us");
        word.trailing_spaces = 3;
        assert_eq!(rules.target(&word, &current).unwrap().name(), "us");
    }
}
