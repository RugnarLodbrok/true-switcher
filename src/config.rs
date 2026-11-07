use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use evdev::KeyCode;

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
const SWITCH_KEY_SETTING: &str = "switch_key";
const MANUAL_SOUND_SETTING: &str = "manual_switch_sound";

pub struct Configuration {
    pub switch_key: KeyCode,
    pub is_manual_switch_sound: bool,
    pub auto_switch: crate::switch_rules::Settings,
}

pub fn load() -> Result<Configuration, String> {
    let path =
        config_path().ok_or_else(|| "Cannot find user configuration: HOME is unset".to_owned())?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    from_contents(&contents).map_err(|error| format!("{}: {error}", path.display()))
}

fn from_contents(contents: &str) -> Result<Configuration, String> {
    let table: toml::Table = contents
        .parse()
        .map_err(|error| format!("Invalid configuration: {error}"))?;
    let value = table
        .get(SWITCH_KEY_SETTING)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("Missing or invalid {SWITCH_KEY_SETTING}"))?;
    let switch_key = KeyCode::from_str(value)
        .ok()
        .filter(|key| value.starts_with("KEY_") && *key != KeyCode::KEY_RESERVED)
        .ok_or_else(|| format!("Invalid {SWITCH_KEY_SETTING}: {value:?}"))?;
    let is_manual_switch_sound = match table.get(MANUAL_SOUND_SETTING) {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("Invalid {MANUAL_SOUND_SETTING}: expected a boolean"))?,
    };
    let auto_switch = crate::switch_rules::Settings::parse(contents)?;
    Ok(Configuration {
        switch_key,
        is_manual_switch_sound,
        auto_switch,
    })
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
        assert!(!config.is_manual_switch_sound);
        assert!(
            from_contents("switch_key = 'KEY_F12'\nmanual_switch_sound = true")
                .unwrap()
                .is_manual_switch_sound
        );
        for text in [
            "",
            "switch_key = 'invalid'",
            "switch_key = 'KEY_RESERVED'",
            "switch_key = KEY_F12",
            "switch_key = 'KEY_F12'\nswitch_key = 'KEY_PAUSE'",
            "switch_key = 'KEY_F12'\nmanual_switch_sound = 'true'",
        ] {
            assert!(from_contents(text).is_err(), "{text}");
        }
    }
}
