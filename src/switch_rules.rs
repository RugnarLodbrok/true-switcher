use std::io;
use std::sync::Arc;

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
        Self::compile(settings, crate::layout::target_layout)
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
        self.rules.iter().find_map(|rule| {
            if rule.target.name == current.name {
                return None;
            }
            let text = rule.target.text(word.strokes())?;
            rule.patterns.is_match(&text).then(|| rule.target.clone())
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
        Ok(Arc::new(Layout {
            name: name.into(),
            ..Default::default()
        }))
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
    fn matches_converted_prefix_and_respects_rule_order_and_current_layout() {
        let settings = parse_config("auto_switch = true\n[[switch_rules]]\ntarget = 'us'\npatterns = ['^/[a-z]']\n[[switch_rules]]\ntarget = 'gb'\npatterns = ['n']").unwrap();
        let switch = AutoSwitchRules::compile(&settings, resolve).unwrap();
        let ru = resolve("ru").unwrap();
        let prefix = word(&[KeyCode::KEY_SLASH, KeyCode::KEY_N]);
        assert_eq!(switch.target(&prefix, &ru).unwrap().name, "us");
        assert_eq!(
            switch
                .target(&prefix, &resolve("us").unwrap())
                .unwrap()
                .name,
            "gb"
        );
        let embedded = word(&[KeyCode::KEY_A, KeyCode::KEY_SLASH, KeyCode::KEY_N]);
        assert_eq!(switch.target(&embedded, &ru).unwrap().name, "gb");
        assert!(switch.target(&word(&[KeyCode::KEY_SLASH]), &ru).is_none());
    }
}
