use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use evdev::KeyCode;
use serde::{Deserialize, Deserializer, de};

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
#[derive(Deserialize)]
pub struct Configuration {
    #[serde(
        default = "default_log_level",
        deserialize_with = "deserialize_log_level"
    )]
    pub log_level: log::LevelFilter,
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
    #[serde(default)]
    pub dict: Vec<DictionarySettings>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchRule {
    pub target: String,
    pub patterns: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DictionarySettings {
    pub target: String,
    pub file: PathBuf,
    pub slip: f64,
    pub grip: f64,
}

fn enabled() -> bool {
    true
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

fn deserialize_log_level<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<log::LevelFilter, D::Error> {
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "off" => Ok(log::LevelFilter::Off),
        "error" => Ok(log::LevelFilter::Error),
        "warn" => Ok(log::LevelFilter::Warn),
        "info" => Ok(log::LevelFilter::Info),
        "debug" => Ok(log::LevelFilter::Debug),
        "trace" => Ok(log::LevelFilter::Trace),
        _ => Err(de::Error::custom(format!("Invalid log_level: {value:?}"))),
    }
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
    load_from(&path)
}

fn load_from(path: &Path) -> Result<Configuration, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let mut config =
        from_contents(&contents).map_err(|error| format!("{}: {error}", path.display()))?;
    let directory = path
        .parent()
        .expect("configuration path includes a directory");
    for dictionary in &mut config.dict {
        if !dictionary.file.is_relative() {
            return Err(format!(
                "{}: dict.file must be relative to the configuration directory: {}",
                path.display(),
                dictionary.file.display(),
            ));
        }
        dictionary.file = directory.join(&dictionary.file);
    }
    Ok(config)
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
    fn resolves_dictionary_path_from_config_location() {
        let directory =
            std::env::temp_dir().join(format!("true-switcher-config-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join(CONFIG_FILE);
        let contents = |file: &str| {
            format!(
                "switch_key = 'KEY_PAUSE'\n[[dict]]\ntarget = 'us'\nfile = '{file}'\nslip = 0\ngrip = 1\n"
            )
        };
        fs::write(&path, contents("dictionaries/words.txt")).unwrap();
        let config = load_from(&path).unwrap();
        assert_eq!(
            config.dict[0].file,
            directory.join("dictionaries/words.txt")
        );
        fs::write(&path, contents("/dictionaries/words.txt")).unwrap();
        assert!(load_from(&path).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

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

    #[test]
    fn validates_log_level() {
        assert_eq!(
            from_contents("switch_key = 'KEY_F12'").unwrap().log_level,
            log::LevelFilter::Info
        );
        for (name, expected) in [
            ("off", log::LevelFilter::Off),
            ("error", log::LevelFilter::Error),
            ("warn", log::LevelFilter::Warn),
            ("info", log::LevelFilter::Info),
            ("debug", log::LevelFilter::Debug),
            ("trace", log::LevelFilter::Trace),
        ] {
            let contents = format!("switch_key = 'KEY_F12'\nlog_level = '{name}'");
            assert_eq!(from_contents(&contents).unwrap().log_level, expected);
        }
        for value in ["'invalid'", "'INFO'", "true", "1"] {
            let contents = format!("switch_key = 'KEY_F12'\nlog_level = {value}");
            assert!(from_contents(&contents).is_err(), "{contents}");
        }
    }
}
