use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use evdev::KeyCode;
use serde::{Deserialize, Deserializer, de};

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
#[derive(Deserialize)]
pub struct Configuration {
    #[serde(deserialize_with = "deserialize_switch_key")]
    pub switch_key: KeyCode,
    #[serde(default)]
    pub manual_switch_sound: bool,
    #[serde(default)]
    pub auto_switch: bool,
    #[serde(default = "enabled")]
    pub auto_switch_sound: bool,
    #[serde(default)]
    pub switch_rules: Vec<SwitchRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchRule {
    pub target: String,
    pub patterns: Vec<String>,
}

fn enabled() -> bool {
    true
}

fn deserialize_switch_key<'de, D: Deserializer<'de>>(deserializer: D) -> Result<KeyCode, D::Error> {
    let value = String::deserialize(deserializer)?;
    KeyCode::from_str(&value)
        .ok()
        .filter(|key| value.starts_with("KEY_") && *key != KeyCode::KEY_RESERVED)
        .ok_or_else(|| de::Error::custom(format!("Invalid switch_key: {value:?}")))
}

pub fn load() -> Result<Configuration, String> {
    let path =
        config_path().ok_or_else(|| "Cannot find user configuration: HOME is unset".to_owned())?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    from_contents(&contents).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn from_contents(contents: &str) -> Result<Configuration, String> {
    toml::from_str(contents).map_err(|error| format!("Invalid configuration: {error}"))
}

fn config_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(CONFIG_DIRECTORY).join(CONFIG_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_switch_key_and_sound() {
        let config = from_contents("switch_key = 'KEY_F12'").unwrap();
        assert_eq!(config.switch_key, KeyCode::KEY_F12);
        assert!(!config.manual_switch_sound);
        assert!(!config.auto_switch);
        assert!(config.auto_switch_sound);
        assert!(config.switch_rules.is_empty());
        assert!(
            from_contents("switch_key = 'KEY_F12'\nmanual_switch_sound = true")
                .unwrap()
                .manual_switch_sound
        );
        for text in [
            "",
            "switch_key = 'invalid'",
            "switch_key = 'KEY_RESERVED'",
            "switch_key = KEY_F12",
            "switch_key = 'KEY_F12'\nswitch_key = 'KEY_PAUSE'",
            "switch_key = 'KEY_F12'\nmanual_switch_sound = 'true'",
            "switch_key = 'KEY_F12'\nauto_switch = 'true'",
            "switch_key = 'KEY_F12'\nswitch_rules = [1]",
            "switch_key = 'KEY_F12'\n[[switch_rules]]\ntarget = ['us']\npatterns = []",
            "switch_key = 'KEY_F12'\n[[switch_rules]]\ntarget = 'ru'\npatterns = []\nvariant = 'phonetic'",
        ] {
            assert!(from_contents(text).is_err(), "{text}");
        }
    }
}
