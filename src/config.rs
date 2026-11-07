use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use evdev::KeyCode;

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
const SWITCH_KEY_SETTING: &str = "switch_key";
const DEFAULT_SWITCH_KEY: KeyCode = KeyCode::KEY_PAUSE;

pub struct Configuration {
    pub switch_key: KeyCode,
    pub is_manual_switch_sound: bool,
    pub warning: Option<String>,
    pub auto_switch: Result<crate::auto_switch::Settings, String>,
}

pub fn load() -> Configuration {
    let result = config_path()
        .ok_or_else(|| "Cannot find user configuration: HOME is unset.".to_owned())
        .and_then(|path| {
            fs::read_to_string(&path)
                .map_err(|error| format!("Cannot read {}: {error}", path.display()))
        });
    from_contents(result)
}

fn from_contents(contents: Result<String, String>) -> Configuration {
    let auto_switch = contents.as_ref().map_or_else(
        |_| Ok(crate::auto_switch::Settings::default()),
        |text| crate::auto_switch::Settings::parse(&without_switch_key(text)),
    );
    let is_manual_switch_sound = contents
        .as_ref()
        .ok()
        .and_then(|text| {
            text.parse::<toml::Table>().ok().and_then(|table| {
                table
                    .get("manual_switch_sound")
                    .and_then(toml::Value::as_bool)
            })
        })
        .unwrap_or(false);
    let result = contents.and_then(|contents| {
        parse_switch_key(&contents)
            .ok_or_else(|| "Missing or invalid switch_key in config.toml.".to_owned())
    });
    match result {
        Ok(switch_key) => Configuration {
            auto_switch,
            switch_key,
            is_manual_switch_sound,
            warning: None,
        },
        Err(error) => Configuration {
            auto_switch,
            switch_key: DEFAULT_SWITCH_KEY,
            is_manual_switch_sound,
            warning: Some(format!(
                "{error} Using KEY_PAUSE. Fix the configuration and select Reload in the tray menu."
            )),
        },
    }
}

fn without_switch_key(contents: &str) -> String {
    let mut is_root = true;
    contents
        .lines()
        .filter(|line| {
            if line.trim_start().starts_with('[') {
                is_root = false;
            }
            !is_root
                || line
                    .split_once('=')
                    .is_none_or(|(key, _)| key.trim() != SWITCH_KEY_SETTING)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn config_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(CONFIG_DIRECTORY).join(CONFIG_FILE))
}

fn parse_switch_key(contents: &str) -> Option<KeyCode> {
    let mut switch_key = None;
    for line in contents.lines() {
        let line = line.split('#').next()?.trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != SWITCH_KEY_SETTING {
            continue;
        }
        if switch_key.is_some() {
            return None;
        }
        let value = value.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        let key = KeyCode::from_str(value).ok()?;
        if !value.starts_with("KEY_") || key == KeyCode::KEY_RESERVED {
            return None;
        }
        switch_key = Some(key);
    }
    switch_key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_key_has_no_warning() {
        let config = from_contents(Ok("switch_key = \"KEY_F12\" # comment".into()));
        assert_eq!(config.switch_key, KeyCode::KEY_F12);
        assert!(config.warning.is_none());
    }

    #[test]
    fn manual_sound_is_disabled_by_default_and_enabled_only_by_true() {
        for (value, expected) in [
            ("", false),
            ("manual_switch_sound = false", false),
            ("manual_switch_sound = true", true),
            ("manual_switch_sound = \"true\"", false),
        ] {
            let config = from_contents(Ok(format!("switch_key = \"KEY_PAUSE\"\n{value}")));
            assert_eq!(config.is_manual_switch_sound, expected);
        }
        assert!(!from_contents(Err("unavailable".into())).is_manual_switch_sound);
    }

    #[test]
    fn missing_invalid_and_duplicate_keys_warn_and_fall_back() {
        for contents in [
            "",
            "switch_key = \"invalid\"",
            "switch_key = \"KEY_RESERVED\"",
            "switch_key = KEY_F12",
            "switch_key = \"KEY_F12\"\nswitch_key = \"KEY_PAUSE\"",
        ] {
            let config = from_contents(Ok(contents.into()));
            assert_eq!(config.switch_key, DEFAULT_SWITCH_KEY);
            assert!(config.warning.unwrap().contains("Using KEY_PAUSE"));
            assert!(config.auto_switch.is_ok());
        }
        let config = from_contents(Err("Permission denied".into()));
        assert_eq!(config.switch_key, DEFAULT_SWITCH_KEY);
        assert!(config.warning.unwrap().contains("Permission denied"));
    }
}
