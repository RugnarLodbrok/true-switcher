use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use evdev::KeyCode;

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
const SWITCH_KEY_SETTING: &str = "switch_key";
const DEFAULT_SWITCH_KEY: KeyCode = KeyCode::KEY_PAUSE;

pub fn switch_key() -> KeyCode {
    config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| parse_switch_key(&contents))
        .unwrap_or(DEFAULT_SWITCH_KEY)
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
