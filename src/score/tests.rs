use std::fs;

use super::*;
use crate::config;

const DICTIONARY_CONFIG: &str = "\
[[dict]]
target = 'us'
file = 'us.txt'
threshold = 1.5

[[dict]]
target = 'ru'
file = 'ru.txt'
threshold = 2.5
";

const KDE_CONFIG: &str = "[Layout]\nLayoutList=us,ru\nModel=pc105\n";

struct Fixture {
    dir: tempfile::TempDir,
    config: Configuration,
    config_file: ConfigurationFile,
}

impl Fixture {
    fn new(dictionaries: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!("switch_key = 'KEY_PAUSE'\nauto_switch = false\n{dictionaries}"),
        )
        .unwrap();
        fs::write(dir.path().join("us.txt"), "bkbfoo 1\n").unwrap();
        fs::write(dir.path().join("ru.txt"), "или 1\n").unwrap();
        fs::write(dir.path().join("kxkbrc"), KDE_CONFIG).unwrap();
        let (config, config_file) = config::load_from(&config_path).unwrap();
        Self {
            dir,
            config,
            config_file,
        }
    }

    fn load(&self, is_learned_enabled: bool) -> io::Result<ScoreSession> {
        ScoreSession::from_config(
            &self.config,
            &self.config_file,
            &self.dir.path().join("kxkbrc"),
            is_learned_enabled,
            None,
        )
    }
}

#[test]
fn loads_configured_dictionaries_and_layouts_with_auto_switch_disabled() {
    let fixture = Fixture::new(DICTIONARY_CONFIG);
    let session = fixture.load(true).unwrap();
    assert!(session.dictionaries.is_pair_loaded());
    assert_eq!(session.directions[0].from.model(), "pc105");
    assert!(session.punctuation.is_strong(&crate::word::Stroke {
        key: evdev::KeyCode::KEY_SLASH,
        shift: false
    }));
}

#[test]
fn reports_missing_dictionary_pair_and_kde_configuration() {
    assert!(
        Fixture::new("")
            .load(true)
            .err()
            .unwrap()
            .to_string()
            .contains("Exactly two dictionaries")
    );

    let fixture = Fixture::new(DICTIONARY_CONFIG);
    let kde_path = fixture.dir.path().join("kxkbrc");
    fs::remove_file(&kde_path).unwrap();
    let error = fixture.load(true).err().unwrap().to_string();
    assert!(error.contains(kde_path.to_str().unwrap()), "{error}");
    fs::write(kde_path, "[Layout]\nLayoutList=us\n").unwrap();
    let error = fixture.load(true).err().unwrap().to_string();
    assert!(error.contains("Layout \"ru\" is absent"), "{error}");
}

#[test]
fn skips_reading_learned_rules_when_disabled() {
    let fixture = Fixture::new(DICTIONARY_CONFIG);
    fs::write(fixture.config_file.learned_rules_path(), "invalid toml").unwrap();
    assert!(fixture.load(true).is_err());
    assert!(fixture.load(false).is_ok());
}

fn prefixes(
    session: &ScoreSession,
    request: &str,
) -> Result<Vec<(String, Option<String>, bool)>, String> {
    let mut rows = Vec::new();
    session.visit_prefixes(
        session.resolve_request(request)?,
        |_, from, to, is_word_end| {
            rows.push((from.to_owned(), to.map(str::to_owned), is_word_end));
        },
    )?;
    Ok(rows)
}

#[test]
fn resolves_prefixes_in_both_directions_and_adds_a_word_boundary() {
    let session = Fixture::new(DICTIONARY_CONFIG).load(false).unwrap();
    for (request, source, target) in [
        ("us ru bkb", ["b", "bk", "bkb"], ["и", "ил", "или"]),
        ("ru us или", ["и", "ил", "или"], ["b", "bk", "bkb"]),
    ] {
        let rows = prefixes(&session, request).unwrap();
        for (index, (from, to, is_word_end)) in rows.iter().enumerate() {
            assert_eq!(from, source[index.min(2)]);
            assert_eq!(to.as_deref(), Some(target[index.min(2)]));
            assert_eq!(*is_word_end, index == 3);
        }
        assert_eq!(rows.len(), 4);
    }
}

#[test]
fn diagnoses_all_positions_and_resolves_choices_from_left_to_right() {
    let session = Fixture::new(DICTIONARY_CONFIG).load(false).unwrap();
    let error = prefixes(&session, "us ru //🦀").unwrap_err();
    for expected in [
        "index 0, '/'",
        "index 1, '/'",
        "index 2, '🦀'",
        "0. KEY_SLASH",
        "1. KEY_KPSLASH",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    let rows = prefixes(&session, "us ru //tmp 1 0").unwrap();
    assert_eq!(rows.last().unwrap().1.as_deref(), Some("/.еьз"));
    for (request, expected) in [
        ("us ru //tmp 1", "Expected 2 choice indices, got 1"),
        ("us ru //tmp 1 0 0", "Expected 2 choice indices, got 3"),
        ("us ru //tmp 2 x", "Invalid choice index \"2\""),
        ("us ru //tmp 0 x", "Invalid choice index \"x\""),
    ] {
        let error = prefixes(&session, request).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("index 0, '/'"), "{error}");
        assert!(error.contains("index 1, '/'"), "{error}");
    }
}

#[test]
fn rejects_early_word_boundaries_and_target_dead_keys() {
    let fixture = Fixture::new(DICTIONARY_CONFIG);
    let session = fixture.load(false).unwrap();
    assert_eq!(
        prefixes(&session, "us ru home/ 0").unwrap_err(),
        "'/' closed the word before the first space."
    );

    fs::write(
        fixture.dir.path().join("kxkbrc"),
        format!("{KDE_CONFIG}VariantList=intl,\n"),
    )
    .unwrap();
    let session = fixture.load(false).unwrap();
    let error = prefixes(&session, "ru us э").unwrap_err();
    assert!(
        error.contains("dead key in the target layout is not supported"),
        "{error}"
    );
}

fn score_rows(output: &str) -> Vec<Vec<&str>> {
    output
        .lines()
        .skip(3)
        .map(|line| line.split_whitespace().collect())
        .collect()
}

#[test]
fn traces_open_prefixes_and_scores_the_exact_word_on_the_first_space() {
    let session = Fixture::new(DICTIONARY_CONFIG).load(false).unwrap();
    let output = session.trace("us ru bkb", None).unwrap();
    assert!(output.starts_with("us → ru | threshold: 2.50\n"));
    assert!(!output.contains("Learned"));
    assert_eq!(
        score_rows(&output),
        [
            vec!["b", "и", "0.00", "0.00", "0.00", "no"],
            vec!["bk", "ил", "0.00", "0.00", "0.00", "no"],
            vec!["bkb", "или", "0.00", "0.00", "0.00", "no"],
            vec!["bkb␣", "или␣", "-inf", "0.00", "+inf", "yes"],
        ]
    );
}

#[test]
fn uses_target_threshold_and_compares_unrounded_scores_at_each_prefix() {
    let fixture = Fixture::new(DICTIONARY_CONFIG);
    fs::write(fixture.dir.path().join("us.txt"), "bkbfoo 8\nz 8\n").unwrap();
    let session = fixture.load(false).unwrap();
    let default_reverse = session.trace("ru us или", None).unwrap();
    assert!(default_reverse.starts_with("ru → us | threshold: 1.50\n"));
    for (threshold, expected) in [(1.0, "no"), (0.999, "yes")] {
        let output = session.trace("us ru bkb", Some(threshold)).unwrap();
        let rows = score_rows(&output);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], ["b", "и", "-1.00", "0.00", "1.00", expected]);
    }
    assert!(
        session
            .trace("ru us или", Some(0.999))
            .unwrap()
            .starts_with("ru → us | threshold: 0.999\n")
    );
}

#[test]
fn marks_unavailable_comparison_and_prints_learned_adjustment_when_enabled() {
    let fixture = Fixture::new(DICTIONARY_CONFIG);
    fs::write(
        fixture.config_file.learned_rules_path(),
        "[ru]\n\"или\" = 3\n",
    )
    .unwrap();
    let session = fixture.load(true).unwrap();
    let output = session.trace("us ru /bkb 0", None).unwrap();
    assert!(output.lines().nth(2).unwrap().contains("Learned"));
    let rows = score_rows(&output);
    assert_eq!(rows[0], ["/", ".", "n/a", "n/a", "n/a", "n/a", "n/a"]);
    assert_eq!(rows[1], ["/b", ".и", "0.00", "0.00", "+3", "3.00", "yes"]);

    fs::write(fixture.dir.path().join("ru.txt"), "другое 1\n").unwrap();
    let with_learning = fixture
        .load(true)
        .unwrap()
        .trace("us ru bkb", None)
        .unwrap();
    let without_learning = fixture
        .load(false)
        .unwrap()
        .trace("us ru bkb", None)
        .unwrap();
    assert_eq!(
        score_rows(&with_learning)[3],
        ["bkb␣", "или␣", "-inf", "-18.00", "+3", "+inf", "yes"]
    );
    assert_eq!(
        score_rows(&without_learning)[3],
        ["bkb␣", "или␣", "-inf", "-inf", "0.00", "no"]
    );
}
