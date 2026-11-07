use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::{history::Word, layout::Layout};

const DATABASE_PATH: &str = "true-switcher/statistics.sqlite3";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS switches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    prev TEXT NOT NULL,
    a TEXT NOT NULL,
    b TEXT NOT NULL,
    mode INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS typing_words (
    word TEXT PRIMARY KEY,
    count INTEGER NOT NULL
)";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Switch(SwitchRecord),
    TypingWord(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SwitchRecord {
    mode: SwitchMode,
    timestamp: i64,
    prev: String,
    a: String,
    b: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum SwitchMode {
    Manual = 0,
    Auto = 1,
}

#[derive(Clone)]
pub struct Statistics {
    sender: Sender<Record>,
}

impl Statistics {
    pub fn start() -> Self {
        Self::start_with_writer(store)
    }

    fn start_with_writer(
        writer: impl FnMut(&Record) -> Result<(), Box<dyn std::error::Error>> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        // Dropping the handle detaches the worker; shutdown does not wait for queued records.
        if let Err(error) = thread::Builder::new()
            .name("statistics".into())
            .spawn(move || write_records(receiver, writer))
        {
            log::error!("true-switcher: cannot start statistics thread: {error}");
        }
        Self { sender }
    }

    #[cfg(test)]
    pub fn record_switch(
        &self,
        previous: Option<&Word>,
        word: Option<&Word>,
        target: &Layout,
        time: SystemTime,
    ) {
        self.record_switch_with_mode(previous, word, target, time, SwitchMode::Manual);
    }

    pub fn record_switch_with_mode(
        &self,
        previous: Option<&Word>,
        word: Option<&Word>,
        target: &Layout,
        time: SystemTime,
        mode: SwitchMode,
    ) {
        let Some(mut record) = make_record(previous, word, target, time) else {
            log::warn!("true-switcher: skipping statistics: cannot convert word to text");
            return;
        };
        record.mode = mode;
        self.send(Record::Switch(record));
    }

    pub fn record_word(&self, word: &Word) {
        let Some(text) = word.text() else {
            log::warn!(
                "true-switcher: skipping typing statistics: cannot convert word to text (missing layout, mismatched strokes or unsupported character)"
            );
            return;
        };
        self.send(Record::TypingWord(text.to_lowercase()));
    }

    fn send(&self, record: Record) {
        if let Err(error) = self.sender.send(record) {
            log::error!("true-switcher: statistics queue: {error}");
        }
    }
}

fn write_records(
    receiver: Receiver<Record>,
    mut writer: impl FnMut(&Record) -> Result<(), Box<dyn std::error::Error>>,
) {
    for record in receiver {
        if let Err(error) = writer(&record) {
            log::error!("true-switcher: statistics database: {error}");
        }
    }
}

fn make_record(
    previous: Option<&Word>,
    word: Option<&Word>,
    target: &Layout,
    time: SystemTime,
) -> Option<SwitchRecord> {
    Some(SwitchRecord {
        mode: SwitchMode::Manual,
        timestamp: time
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .try_into()
            .ok()?,
        prev: previous
            .map(Word::text)
            .unwrap_or_else(|| Some(String::new()))?,
        a: word
            .map(Word::text)
            .unwrap_or_else(|| Some(String::new()))?,
        b: word
            .map(|word| target.text(&word.strokes))
            .unwrap_or_else(|| Some(String::new()))?,
    })
}

fn database_path(
    state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    state
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/state")))
        .map(|path| path.join(DATABASE_PATH))
        .ok_or_else(|| io::Error::other("XDG_STATE_HOME and HOME are unset"))
}

fn store(record: &Record) -> Result<(), Box<dyn std::error::Error>> {
    let path = database_path(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))?;
    fs::create_dir_all(path.parent().ok_or("database directory is missing")?)?;
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    insert(&connection, record)?;
    Ok(())
}

fn insert(connection: &Connection, record: &Record) -> rusqlite::Result<()> {
    connection.execute_batch(SCHEMA)?;
    let has_mode = connection
        .prepare("PRAGMA table_info(switches)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "mode");
    if !has_mode {
        connection
            .execute_batch("ALTER TABLE switches ADD COLUMN mode INTEGER NOT NULL DEFAULT 0")?;
    }
    match record {
        Record::Switch(record) => {
            connection.execute(
                "INSERT INTO switches (timestamp, prev, a, b, mode) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.timestamp,
                    record.prev,
                    record.a,
                    record.b,
                    record.mode as i64
                ],
            )?;
        }
        Record::TypingWord(word) => {
            connection.execute(
                "INSERT INTO typing_words (word, count) VALUES (?1, 1)
                 ON CONFLICT(word) DO UPDATE SET count = count + 1",
                params![word],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Stroke;
    use evdev::KeyCode;
    use std::sync::Arc;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn migrates_old_switches_as_manual_and_stores_auto_mode() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE switches (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp INTEGER NOT NULL, prev TEXT NOT NULL, a TEXT NOT NULL, b TEXT NOT NULL); INSERT INTO switches (timestamp, prev, a, b) VALUES (1, '', 'old', 'old');").unwrap();
        let mut record = make_record(None, None, &layout("us"), UNIX_EPOCH).unwrap();
        record.mode = SwitchMode::Auto;
        insert(&connection, &Record::Switch(record)).unwrap();
        let modes: Vec<i64> = connection
            .prepare("SELECT mode FROM switches ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(modes, [0, 1]);
        let record = make_record(None, None, &layout("us"), UNIX_EPOCH).unwrap();
        insert(&connection, &Record::Switch(record)).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT mode FROM switches WHERE id = 3", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    fn layout(name: &str) -> Arc<Layout> {
        Arc::new(Layout {
            name: name.into(),
            ..Layout::default()
        })
    }

    fn word(name: &str) -> Word {
        let strokes: Vec<_> = [KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_B]
            .into_iter()
            .map(|key| Stroke { key, shift: false })
            .collect();
        Word {
            layouts: vec![Some(layout(name)); strokes.len()],
            strokes,
            trailing_spaces: 2,
        }
    }

    fn observed_history() -> (crate::history::History, Receiver<Record>) {
        let (sent, received) = mpsc::channel();
        let statistics = Statistics::start_with_writer(move |record| {
            sent.send(record.clone())?;
            Ok(())
        });
        let mut history = crate::history::History::default().with_statistics(statistics);
        history.accept_layout(Some(layout("us")), false);
        (history, received)
    }

    fn press(history: &mut crate::history::History, key: KeyCode) {
        history.process(std::path::Path::new("keyboard"), key, 1);
    }

    fn finish_history(history: crate::history::History, received: Receiver<Record>) -> Vec<Record> {
        drop(history);
        let mut records = Vec::new();
        loop {
            match received.recv_timeout(TEST_TIMEOUT) {
                Ok(record) => records.push(record),
                Err(mpsc::RecvTimeoutError::Disconnected) => return records,
                Err(error) => panic!("statistics worker did not finish: {error}"),
            }
        }
    }

    #[test]
    fn history_counts_final_words_once_only_when_they_leave() {
        for separator in [
            None,
            Some(KeyCode::KEY_TAB),
            Some(KeyCode::KEY_ENTER),
            Some(KeyCode::KEY_KPENTER),
        ] {
            let (mut history, received) = observed_history();
            let keys = [
                KeyCode::KEY_G,
                KeyCode::KEY_H,
                KeyCode::KEY_B,
                KeyCode::KEY_D,
                KeyCode::KEY_T,
                KeyCode::KEY_N,
            ];
            for key in keys {
                press(&mut history, key);
            }
            history.set_latest_layout(Some(layout("ru")));
            history.set_latest_layout(Some(layout("us")));
            history.set_latest_layout(Some(layout("ru")));
            press(&mut history, KeyCode::KEY_SPACE);
            press(&mut history, KeyCode::KEY_SPACE);
            for key in keys {
                press(&mut history, key);
            }
            assert!(matches!(
                received.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            if let Some(separator) = separator {
                press(&mut history, separator);
            } else {
                history.reset();
            }
            history.reset();
            assert_eq!(
                finish_history(history, received),
                [
                    Record::TypingWord("привет".into()),
                    Record::TypingWord("привет".into())
                ]
            );
        }
    }

    #[test]
    fn history_ignores_erased_words_and_counts_partially_erased_word() {
        let (mut history, received) = observed_history();
        for key in [
            KeyCode::KEY_A,
            KeyCode::KEY_SPACE,
            KeyCode::KEY_B,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_C,
            KeyCode::KEY_B,
            KeyCode::KEY_BACKSPACE,
        ] {
            press(&mut history, key);
        }
        history.reset();
        assert_eq!(
            finish_history(history, received),
            [Record::TypingWord("c".into())]
        );
    }

    #[test]
    fn typing_words_are_lowercase_keep_punctuation_and_skip_unknown_layouts() {
        let (mut history, received) = observed_history();
        press(&mut history, KeyCode::KEY_LEFTSHIFT);
        press(&mut history, KeyCode::KEY_G);
        history.process(std::path::Path::new("keyboard"), KeyCode::KEY_LEFTSHIFT, 0);
        history.set_latest_layout(Some(layout("ru")));
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_COMMA);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        history.set_latest_layout(None);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_C);
        history.reset();
        assert_eq!(
            finish_history(history, received),
            [
                Record::TypingWord("п".into()),
                Record::TypingWord("фб".into()),
                Record::TypingWord("с".into())
            ]
        );
    }

    #[test]
    fn sqlite_adds_typing_counters_to_existing_database_and_preserves_switches() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE switches (
                id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp INTEGER NOT NULL,
                prev TEXT NOT NULL, a TEXT NOT NULL, b TEXT NOT NULL
            );
            INSERT INTO switches (timestamp, prev, a, b) VALUES (123, '', 'a', 'ф');",
            )
            .unwrap();
        for text in ["привет", "привет", "привет,", "'\"; DROP TABLE switches;"] {
            insert(&connection, &Record::TypingWord(text.into())).unwrap();
        }
        let mut statement = connection
            .prepare("SELECT word, count FROM typing_words ORDER BY word")
            .unwrap();
        let counts: Vec<(String, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            counts,
            [
                ("'\"; DROP TABLE switches;".into(), 1),
                ("привет".into(), 2),
                ("привет,".into(), 1),
            ]
        );
        let switches: i64 = connection
            .query_row(
                "SELECT count(*) FROM switches WHERE a = 'a' AND b = 'ф'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(switches, 1);
    }

    #[test]
    fn records_original_previous_word_and_both_versions_of_corrected_word() {
        let previous = word("ru");
        let original = word("us");
        let time = UNIX_EPOCH + Duration::from_secs(123);
        let record = make_record(Some(&previous), Some(&original), &layout("ru"), time).unwrap();
        assert_eq!(
            record,
            SwitchRecord {
                mode: SwitchMode::Manual,
                timestamp: 123,
                prev: "при".into(),
                a: "ghb".into(),
                b: "при".into()
            }
        );
    }

    #[test]
    fn absent_words_are_empty_but_unknown_layout_skips_the_record() {
        let record = make_record(None, None, &layout("us"), UNIX_EPOCH).unwrap();
        assert_eq!(
            (&record.prev, &record.a, &record.b),
            (&String::new(), &String::new(), &String::new())
        );
        let mut unknown = word("us");
        unknown.layouts[0] = None;
        assert!(make_record(None, Some(&unknown), &layout("ru"), UNIX_EPOCH).is_none());
        assert!(make_record(Some(&unknown), None, &layout("ru"), UNIX_EPOCH).is_none());
        let invalid = layout("\0");
        assert!(make_record(None, Some(&word("us")), &invalid, UNIX_EPOCH).is_none());
    }

    #[test]
    fn state_path_uses_xdg_and_falls_back_for_empty_or_missing_value() {
        assert_eq!(
            database_path(Some("/state".into()), Some("/home/user".into())).unwrap(),
            PathBuf::from("/state/true-switcher/statistics.sqlite3")
        );
        for state in [None, Some("".into())] {
            assert_eq!(
                database_path(state, Some("/home/user".into())).unwrap(),
                PathBuf::from("/home/user/.local/state/true-switcher/statistics.sqlite3")
            );
        }
        assert!(database_path(None, None).is_err());
    }

    #[test]
    fn sqlite_preserves_unicode_quotes_timestamps_and_autoincrements_ids() {
        let connection = Connection::open_in_memory().unwrap();
        let record = SwitchRecord {
            mode: SwitchMode::Manual,
            timestamp: 123,
            prev: "привет".into(),
            a: "'\"; DROP TABLE switches;".into(),
            b: "слово".into(),
        };
        insert(&connection, &Record::Switch(record.clone())).unwrap();
        insert(&connection, &Record::Switch(record.clone())).unwrap();
        let stored = connection
            .query_row(
                "SELECT timestamp, prev, a, b FROM switches WHERE id = 2",
                [],
                |row| {
                    Ok(SwitchRecord {
                        mode: SwitchMode::Manual,
                        timestamp: row.get(0)?,
                        prev: row.get(1)?,
                        a: row.get(2)?,
                        b: row.get(3)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(stored, record);
        connection.execute("DELETE FROM switches", []).unwrap();
        insert(&connection, &Record::Switch(record)).unwrap();
        let id: i64 = connection
            .query_row("SELECT id FROM switches", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, 3);
    }

    #[test]
    fn database_write_error_is_returned_for_logging() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("PRAGMA query_only = ON").unwrap();
        let record = make_record(None, None, &layout("us"), UNIX_EPOCH).unwrap();
        assert!(insert(&connection, &Record::Switch(record)).is_err());
    }

    #[test]
    fn queues_switch_snapshot_despite_replay_cancellation_or_error() {
        for is_cancelled in [true, false] {
            let (sent, received) = mpsc::channel();
            let statistics = Statistics::start_with_writer(move |record| {
                sent.send(record.clone()).unwrap();
                Ok(())
            });
            let previous = word("ru");
            let original = word("us");
            let time = UNIX_EPOCH + Duration::from_secs(123);
            let result = crate::switch_and_replay(
                || Ok(()),
                || {},
                || statistics.record_switch(Some(&previous), Some(&original), &layout("ru"), time),
                || Err(io::Error::other("replay failed")),
                &std::sync::atomic::AtomicBool::new(is_cancelled),
            );
            assert_eq!(result.is_ok(), is_cancelled);
            assert_eq!(
                received.recv_timeout(TEST_TIMEOUT).unwrap(),
                Record::Switch(
                    make_record(Some(&previous), Some(&original), &layout("ru"), time).unwrap()
                )
            );
        }
    }

    #[test]
    fn replay_and_shutdown_do_not_wait_for_database_write() {
        let (started, writing) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (finished, completed) = mpsc::channel();
        let statistics = Statistics::start_with_writer(move |_| {
            started.send(()).unwrap();
            blocked.recv_timeout(TEST_TIMEOUT).unwrap();
            finished.send(()).unwrap();
            Ok(())
        });
        crate::switch_and_replay(
            || Ok(()),
            || {},
            || {
                statistics.record_switch(None, None, &layout("us"), UNIX_EPOCH);
                writing.recv_timeout(TEST_TIMEOUT).unwrap();
            },
            || {
                assert!(matches!(
                    completed.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
                Ok(())
            },
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        drop(statistics);
        release.send(()).unwrap();
        completed.recv_timeout(TEST_TIMEOUT).unwrap();
    }

    #[test]
    fn database_error_does_not_stop_replay_or_later_writes() {
        let (sent, received) = mpsc::channel();
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("PRAGMA query_only = ON").unwrap();
        let statistics = Statistics::start_with_writer(move |record| {
            let result = insert(&connection, record);
            sent.send(result.is_ok()).unwrap();
            connection.execute_batch("PRAGMA query_only = OFF")?;
            result.map_err(Into::into)
        });
        for is_expected_success in [false, true] {
            let mut is_replayed = false;
            crate::switch_and_replay(
                || Ok(()),
                || {},
                || {
                    statistics.record_switch(None, None, &layout("us"), UNIX_EPOCH);
                    assert_eq!(
                        received.recv_timeout(TEST_TIMEOUT).unwrap(),
                        is_expected_success
                    );
                },
                || {
                    is_replayed = true;
                    Ok(())
                },
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
            assert!(is_replayed);
        }
    }

    #[test]
    fn disconnected_statistics_queue_does_not_stop_replay() {
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let statistics = Statistics { sender };
        let mut is_replayed = false;
        crate::switch_and_replay(
            || Ok(()),
            || {},
            || statistics.record_switch(None, None, &layout("us"), UNIX_EPOCH),
            || {
                is_replayed = true;
                Ok(())
            },
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(is_replayed);
    }
}
