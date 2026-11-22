use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use evdev::KeyCode;
use serde::{Deserialize, Deserializer, de};
use toml_edit::DocumentMut;

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
const LOG_LEVEL_KEY: &str = "log_level";
pub(crate) const LOG_LEVELS: [(&str, log::LevelFilter); 6] = [
    ("off", log::LevelFilter::Off),
    ("error", log::LevelFilter::Error),
    ("warn", log::LevelFilter::Warn),
    ("info", log::LevelFilter::Info),
    ("debug", log::LevelFilter::Debug),
    ("trace", log::LevelFilter::Trace),
];

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
    #[serde(default)]
    pub pattern_switch: bool,
    #[serde(default = "enabled")]
    pub auto_switch_sound: bool,
    #[serde(default)]
    pub switch_rules: Vec<SwitchRule>,
    #[serde(default)]
    pub dict_skip_pattern: String,
    #[serde(default)]
    pub dict: Vec<DictionarySettings>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeSettings {
    pub(crate) is_auto_switch: bool,
    pub(crate) is_pattern_switch: bool,
    pub(crate) is_manual_switch_sound: bool,
    pub(crate) is_auto_switch_sound: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RuntimeSetting {
    AutoSwitch,
    PatternSwitch,
    ManualSwitchSound,
    AutoSwitchSound,
}

impl RuntimeSettings {
    pub(crate) fn toggle(&mut self, setting: RuntimeSetting) -> bool {
        let value = match setting {
            RuntimeSetting::AutoSwitch => &mut self.is_auto_switch,
            RuntimeSetting::PatternSwitch => &mut self.is_pattern_switch,
            RuntimeSetting::ManualSwitchSound => &mut self.is_manual_switch_sound,
            RuntimeSetting::AutoSwitchSound => &mut self.is_auto_switch_sound,
        };
        *value = !*value;
        *value
    }
}

impl From<&Configuration> for RuntimeSettings {
    fn from(config: &Configuration) -> Self {
        Self {
            is_auto_switch: config.auto_switch,
            is_pattern_switch: config.pattern_switch,
            is_manual_switch_sound: config.manual_switch_sound,
            is_auto_switch_sound: config.auto_switch_sound,
        }
    }
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
    pub threshold: f64,
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
    LOG_LEVELS
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, level)| *level)
        .ok_or_else(|| de::Error::custom(format!("Invalid log_level: {value:?}")))
}

fn deserialize_switch_key<'de, D: Deserializer<'de>>(deserializer: D) -> Result<KeyCode, D::Error> {
    let value = String::deserialize(deserializer)?;
    KeyCode::from_str(&value)
        .ok()
        .filter(|key| value.starts_with("KEY_") && *key != KeyCode::KEY_RESERVED)
        .ok_or_else(|| de::Error::custom(format!("Invalid switch_key: {value:?}")))
}

pub(crate) struct ConfigurationFile {
    path: PathBuf,
    document: DocumentMut,
}

impl ConfigurationFile {
    pub(crate) fn save(
        &self,
        settings: RuntimeSettings,
        log_level: log::LevelFilter,
    ) -> io::Result<()> {
        let mut document = self.document.clone();
        for (key, mut value) in [
            (
                "auto_switch",
                toml_edit::Value::from(settings.is_auto_switch),
            ),
            ("pattern_switch", settings.is_pattern_switch.into()),
            (
                "manual_switch_sound",
                settings.is_manual_switch_sound.into(),
            ),
            ("auto_switch_sound", settings.is_auto_switch_sound.into()),
            (
                LOG_LEVEL_KEY,
                log_level.as_str().to_ascii_lowercase().into(),
            ),
        ] {
            if let Some(old) = document.get(key).and_then(toml_edit::Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            document[key] = toml_edit::Item::Value(value);
        }

        let directory = self
            .path
            .parent()
            .expect("config path includes a directory");
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

pub fn load() -> Result<(Configuration, ConfigurationFile), String> {
    let path =
        config_path().ok_or_else(|| "Cannot find user configuration: HOME is unset".to_owned())?;
    load_from(&path)
}

pub(crate) fn load_from(path: &Path) -> Result<(Configuration, ConfigurationFile), String> {
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
    let document = contents
        .parse()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok((
        config,
        ConfigurationFile {
            path: path.to_path_buf(),
            document,
        },
    ))
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
                "switch_key = 'KEY_PAUSE'\n[[dict]]\ntarget = 'us'\nfile = '{file}'\nthreshold = 5.0\n"
            )
        };
        fs::write(&path, contents("dictionaries/words.txt")).unwrap();
        let (config, _) = load_from(&path).unwrap();
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
    fn save_replaces_the_file_from_the_loaded_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        let original = "\
# Header
switch_key = 'KEY_PAUSE'
'auto_switch'  =  false   # Inline
unknown = [ 1, 2 ]

[[dict]]
target = 'us'
file = 'words.txt'
threshold = 5.0
# Footer
";
        fs::write(&path, original).unwrap();
        let (config, file) = load_from(&path).unwrap();
        let mut settings = RuntimeSettings::from(&config);
        settings.toggle(RuntimeSetting::AutoSwitch);
        settings.toggle(RuntimeSetting::ManualSwitchSound);
        fs::write(&path, "invalid TOML").unwrap();
        file.save(settings, log::LevelFilter::Debug).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert_eq!(
            saved,
            original.replace("'auto_switch'  =  false", "'auto_switch'  =  true").replace(
                "\n[[dict]]",
                "pattern_switch = false\nmanual_switch_sound = true\nauto_switch_sound = true\nlog_level = \"debug\"\n\n[[dict]]",
            )
        );
        let (saved_config, _) = load_from(&path).unwrap();
        assert!(saved_config.manual_switch_sound);
        assert_eq!(saved_config.log_level, log::LevelFilter::Debug);

        fs::remove_file(&path).unwrap();
        file.save(settings, log::LevelFilter::Debug).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), saved);
    }

    #[test]
    fn save_preserves_log_level_comment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        fs::write(
            &path,
            "switch_key = 'KEY_PAUSE'\nlog_level  =  'info'   # Logging\n",
        )
        .unwrap();
        let (config, file) = load_from(&path).unwrap();
        file.save(RuntimeSettings::from(&config), log::LevelFilter::Trace)
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("log_level  =  \"trace\"   # Logging\n"));
        assert_eq!(
            load_from(&path).unwrap().0.log_level,
            log::LevelFilter::Trace
        );
    }

    #[test]
    fn validates_switch_key_and_sound() {
        let config = from_contents("switch_key = 'KEY_F12'").unwrap();
        assert_eq!(config.switch_key, KeyCode::KEY_F12);
        assert!(!config.manual_switch_sound);
        assert!(!config.auto_switch);
        assert!(!config.pattern_switch);
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
            "switch_key = 'KEY_F12'\npattern_switch = 'true'",
            "switch_key = 'KEY_F12'\nswitch_rules = [1]",
            "switch_key = 'KEY_F12'\n[[switch_rules]]\ntarget = ['us']\npatterns = []",
            "switch_key = 'KEY_F12'\n[[switch_rules]]\ntarget = 'ru'\npatterns = []\nvariant = 'phonetic'",
        ] {
            assert!(from_contents(text).is_err(), "{text}");
        }
    }

    #[test]
    fn reads_dictionary_skip_pattern() {
        for (setting, expected) in [
            ("", ""),
            ("dict_skip_pattern = ''", ""),
            (r"dict_skip_pattern = '.*\p{Lu}.*'", r".*\p{Lu}.*"),
        ] {
            let contents = format!("switch_key = 'KEY_F12'\n{setting}");
            assert_eq!(
                from_contents(&contents).unwrap().dict_skip_pattern,
                expected
            );
        }
        assert!(from_contents("switch_key = 'KEY_F12'\ndict_skip_pattern = true").is_err());
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
