use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use evdev::KeyCode;
use serde::{Deserialize, Deserializer, de};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

const CONFIG_DIRECTORY: &str = ".config/true-switcher";
const CONFIG_FILE: &str = "config.toml";
const LOG_LEVEL_KEY: &str = "log_level";
const DEFAULT_DICTIONARY_THRESHOLD: f64 = 15.0;
pub(crate) const DEFAULT_MOUSE_MOTION_PAUSE_MS: u64 = 500;
pub(crate) const DEFAULT_PUNCTUATION: &str = r"[`~<>\p{P}]";
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
    #[serde(default = "default_mouse_motion_pause_ms")]
    pub mouse_motion_pause_ms: u64,
    #[serde(default = "enabled")]
    pub auto_switch_sound: bool,
    #[serde(default)]
    pub switch_rules: Vec<SwitchRule>,
    #[serde(default)]
    pub dict_skip_pattern: String,
    #[serde(default = "default_punctuation")]
    pub punctuation: String,
    #[serde(default, deserialize_with = "deserialize_ambiguity_score_floor")]
    pub ambiguity_score_floor: Option<f64>,
    #[serde(default)]
    pub dict: Vec<DictionaryEntry>,
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
    fn value_mut(&mut self, setting: RuntimeSetting) -> &mut bool {
        match setting {
            RuntimeSetting::AutoSwitch => &mut self.is_auto_switch,
            RuntimeSetting::PatternSwitch => &mut self.is_pattern_switch,
            RuntimeSetting::ManualSwitchSound => &mut self.is_manual_switch_sound,
            RuntimeSetting::AutoSwitchSound => &mut self.is_auto_switch_sound,
        }
    }

    pub(crate) fn toggle(&mut self, setting: RuntimeSetting) -> bool {
        let value = self.value_mut(setting);
        *value = !*value;
        *value
    }
}

impl RuntimeSetting {
    fn key(self) -> &'static str {
        match self {
            Self::AutoSwitch => "auto_switch",
            Self::PatternSwitch => "pattern_switch",
            Self::ManualSwitchSound => "manual_switch_sound",
            Self::AutoSwitchSound => "auto_switch_sound",
        }
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
pub struct DictionaryEntry {
    pub target: String,
    #[serde(flatten)]
    fields: toml::Table,
}

impl DictionaryEntry {
    pub(crate) fn settings(&self) -> io::Result<DictionarySettings> {
        toml::Value::Table(self.fields.clone())
            .try_into()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("dict {:?}: {error}", self.target),
                )
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DictionarySettings {
    pub file: PathBuf,
    pub threshold: f64,
}

fn enabled() -> bool {
    true
}

fn default_log_level() -> log::LevelFilter {
    log::LevelFilter::Info
}

fn default_punctuation() -> String {
    DEFAULT_PUNCTUATION.to_owned()
}

fn default_mouse_motion_pause_ms() -> u64 {
    DEFAULT_MOUSE_MOTION_PAUSE_MS
}

fn deserialize_ambiguity_score_floor<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && value <= 0.0 {
        Ok(Some(value))
    } else {
        Err(de::Error::custom(
            "ambiguity_score_floor must be finite and nonpositive",
        ))
    }
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
    snapshot: Mutex<DocumentMut>,
}

impl ConfigurationFile {
    pub(crate) fn directory(&self) -> &Path {
        self.path
            .parent()
            .expect("config path includes a directory")
    }

    pub(crate) fn learned_rules_path(&self) -> PathBuf {
        self.path.with_file_name(crate::learned_rules::FILE_NAME)
    }

    pub(crate) fn save_setting(
        &self,
        setting: RuntimeSetting,
        mut settings: RuntimeSettings,
    ) -> io::Result<()> {
        self.update(|document| {
            set_value(
                document.as_table_mut(),
                setting.key(),
                (*settings.value_mut(setting)).into(),
            );
            Ok(())
        })
    }

    pub(crate) fn save_log_level(&self, level: log::LevelFilter) -> io::Result<()> {
        self.update(|document| {
            set_value(
                document.as_table_mut(),
                LOG_LEVEL_KEY,
                level.as_str().to_ascii_lowercase().into(),
            );
            Ok(())
        })
    }

    pub(crate) fn dictionary_skip_pattern(&self) -> io::Result<String> {
        let snapshot = self.snapshot.lock().unwrap();
        let document = self.current_document(&snapshot)?;
        Ok(from_contents(&document.to_string())
            .map_err(io::Error::other)?
            .dict_skip_pattern)
    }

    pub(crate) fn connect_dictionaries<'a>(
        &self,
        dictionaries: impl IntoIterator<Item = (&'a str, &'a Path)>,
        skip_pattern: &str,
    ) -> io::Result<()> {
        self.update(|document| {
            let current = from_contents(&document.to_string()).map_err(io::Error::other)?;
            if current.dict_skip_pattern != skip_pattern {
                return Err(io::Error::other(
                    "dict_skip_pattern changed during dictionary download",
                ));
            }
            let records = document
                .entry("dict")
                .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
            for (target, path) in dictionaries {
                let path = path
                    .to_str()
                    .ok_or_else(|| io::Error::other("Dictionary path is not UTF-8"))?;
                update_dictionary(records, target, path)?;
            }
            set_value(
                document.as_table_mut(),
                RuntimeSetting::AutoSwitch.key(),
                false.into(),
            );
            Ok(())
        })
    }

    fn current_document(&self, snapshot: &DocumentMut) -> io::Result<DocumentMut> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => contents.parse().map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(snapshot.clone()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn update(
        &self,
        edit: impl FnOnce(&mut DocumentMut) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut snapshot = self.snapshot.lock().unwrap();
        let mut document = self.current_document(&snapshot)?;
        edit(&mut document)?;
        fs::create_dir_all(self.directory())?;
        let mut temporary = tempfile::NamedTempFile::new_in(self.directory())?;
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        *snapshot = document;
        Ok(())
    }
}

fn set_value(table: &mut dyn TableLike, key: &str, mut value: Value) {
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
        *table.get_mut(key).expect("existing TOML field") = Item::Value(value);
    } else {
        table.insert(key, Item::Value(value));
    }
}

fn update_dictionary(records: &mut Item, target: &str, path: &str) -> io::Result<()> {
    let is_target =
        |table: &dyn TableLike| table.get("target").and_then(Item::as_str) == Some(target);
    match records {
        Item::ArrayOfTables(tables) => {
            if let Some(table) = tables.iter_mut().find(|table| is_target(*table)) {
                set_value(table, "file", path.into());
            } else {
                let mut table = Table::new();
                set_new_dictionary(&mut table, target, path);
                tables.push(table);
            }
        }
        Item::Value(Value::Array(tables)) => {
            let existing = tables
                .iter_mut()
                .filter_map(Value::as_inline_table_mut)
                .find(|table| is_target(*table));
            if let Some(table) = existing {
                set_value(table, "file", path.into());
            } else {
                let mut table = InlineTable::new();
                set_new_dictionary(&mut table, target, path);
                tables.push(Value::InlineTable(table));
            }
        }
        _ => return Err(io::Error::other("Expected a list of dictionaries")),
    }
    Ok(())
}

fn set_new_dictionary(table: &mut dyn TableLike, target: &str, path: &str) {
    set_value(table, "target", target.into());
    set_value(table, "file", path.into());
    set_value(table, "threshold", DEFAULT_DICTIONARY_THRESHOLD.into());
}

pub fn load() -> Result<(Configuration, ConfigurationFile), String> {
    let path =
        config_path().ok_or_else(|| "Cannot find user configuration: HOME is unset".to_owned())?;
    load_from(&path)
}

pub(crate) fn load_from(path: &Path) -> Result<(Configuration, ConfigurationFile), String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let config =
        from_contents(&contents).map_err(|error| format!("{}: {error}", path.display()))?;
    crate::punctuation::Punctuation::new(&config.punctuation)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let document = contents
        .parse()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok((
        config,
        ConfigurationFile {
            path: path.to_path_buf(),
            snapshot: Mutex::new(document),
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
    fn rejects_punctuation_that_is_not_a_set_of_single_characters() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        for pattern in ["[", "[!?]+", ";;|жж", ";?", "^;", ";|"] {
            fs::write(
                &path,
                format!("switch_key = 'KEY_PAUSE'\npunctuation = '{pattern}'"),
            )
            .unwrap();
            let error = load_from(&path).err().expect(pattern);
            assert!(error.contains("Invalid punctuation"), "{pattern}: {error}");
        }
    }

    #[test]
    fn reads_and_validates_ambiguity_score_floor() {
        const BASE: &str = "switch_key = 'KEY_PAUSE'";
        for contents in [BASE, include_str!("../installer/config.example.toml")] {
            assert_eq!(from_contents(contents).unwrap().ambiguity_score_floor, None);
        }
        for (value, expected) in [("-12.5", -12.5), ("0", 0.0)] {
            let contents = format!("{BASE}\nambiguity_score_floor = {value}");
            assert_eq!(
                from_contents(&contents).unwrap().ambiguity_score_floor,
                Some(expected)
            );
        }
        for value in ["'low'", "true", "1", "inf", "-inf", "nan"] {
            let contents = format!("{BASE}\nambiguity_score_floor = {value}");
            let error = from_contents(&contents).err().expect(value);
            assert!(
                error.to_string().contains("ambiguity_score_floor"),
                "{error}"
            );
        }
    }

    #[test]
    fn saves_only_the_selected_field_and_restores_the_last_successful_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        let original = "switch_key = 'KEY_PAUSE'\nauto_switch = true\n";
        fs::write(&path, original).unwrap();
        let (_, file) = load_from(&path).unwrap();
        let edited = format!("{original}'manual_switch_sound'  =  false   # Inline\n");
        fs::write(&path, &edited).unwrap();

        let mut settings = RuntimeSettings::default();
        settings.toggle(RuntimeSetting::ManualSwitchSound);
        file.save_setting(RuntimeSetting::ManualSwitchSound, settings)
            .unwrap();
        let saved = edited.replace("=  false", "=  true");
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);

        fs::write(&path, "invalid TOML").unwrap();
        assert!(file.save_log_level(log::LevelFilter::Debug).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid TOML");

        fs::remove_file(&path).unwrap();
        file.save_log_level(log::LevelFilter::Trace).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{saved}log_level = \"trace\"\n")
        );
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
        let (_, file) = load_from(&path).unwrap();
        file.save_log_level(log::LevelFilter::Trace).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("log_level  =  \"trace\"   # Logging\n"));
        assert_eq!(
            load_from(&path).unwrap().0.log_level,
            log::LevelFilter::Trace
        );
    }

    #[test]
    fn connects_dictionaries_in_existing_inline_arrays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        for (array, threshold) in [
            ("[]", "15.0"),
            (
                "[{ target = 'us', file = 'old.txt', threshold = 5.0 }]",
                "5.0",
            ),
        ] {
            fs::write(
                &path,
                format!("switch_key = 'KEY_PAUSE'\ndict = {array} # Keep\n"),
            )
            .unwrap();
            let (_, file) = load_from(&path).unwrap();
            file.connect_dictionaries(
                [
                    ("us", Path::new("dicts/en.txt")),
                    ("ru", Path::new("dicts/ru.txt")),
                ],
                "",
            )
            .unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert!(saved.contains("dict = ["));
            assert!(saved.contains("] # Keep"));
            assert!(saved.contains(&format!("threshold = {threshold}")));
            assert!(saved.contains("dicts/en.txt"));
            assert!(saved.contains("dicts/ru.txt"));
            assert_eq!(load_from(&path).unwrap().0.dict.len(), 2);
        }
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
    fn reads_mouse_motion_pause() {
        for (setting, expected) in [
            ("", 500),
            ("mouse_motion_pause_ms = 0", 0),
            ("mouse_motion_pause_ms = 1200", 1200),
        ] {
            let contents = format!("switch_key = 'KEY_F12'\n{setting}");
            assert_eq!(
                from_contents(&contents).unwrap().mouse_motion_pause_ms,
                expected
            );
        }
        assert!(from_contents("switch_key = 'KEY_F12'\nmouse_motion_pause_ms = -1").is_err());
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
