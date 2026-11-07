use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::{layout::Layout, switcher::SwitchMode, word::Word};

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

enum PendingRecord {
    Switch {
        previous: Option<Word>,
        word: Option<Word>,
        target: Layout,
        time: SystemTime,
        mode: SwitchMode,
    },
    TypingWord(Word),
}

#[derive(Clone)]
pub struct Statistics {
    sender: Sender<PendingRecord>,
}

impl Statistics {
    pub fn start() -> Self {
        Self::start_with_writer(database_writer(open_database))
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
        self.send(PendingRecord::Switch {
            previous: previous.cloned(),
            word: word.cloned(),
            target: target.clone(),
            time,
            mode,
        });
    }

    pub fn record_word(&self, word: Word) {
        self.send(PendingRecord::TypingWord(word));
    }

    fn send(&self, record: PendingRecord) {
        if let Err(error) = self.sender.send(record) {
            log::error!("true-switcher: statistics queue: {error}");
        }
    }
}

fn write_records(
    receiver: Receiver<PendingRecord>,
    mut writer: impl FnMut(&Record) -> Result<(), Box<dyn std::error::Error>>,
) {
    for pending in receiver {
        let Some(record) = convert_record(pending) else {
            continue;
        };
        if let Err(error) = writer(&record) {
            log::error!("true-switcher: statistics database: {error}");
        }
    }
}

fn convert_record(pending: PendingRecord) -> Option<Record> {
    let record = match pending {
        PendingRecord::TypingWord(word) => word
            .text()
            .map(|text| Record::TypingWord(text.to_lowercase())),
        PendingRecord::Switch {
            previous,
            word,
            target,
            time,
            mode,
        } => make_record(previous.as_ref(), word.as_ref(), &target, time).map(|mut record| {
            record.mode = mode;
            Record::Switch(record)
        }),
    };
    if record.is_none() {
        log::warn!("true-switcher: skipping statistics: cannot convert word to text or timestamp");
    }
    record
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
            .map(|word| target.text(word.strokes()))
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

fn database_writer(
    mut open: impl FnMut() -> Result<Connection, Box<dyn std::error::Error>>,
) -> impl FnMut(&Record) -> Result<(), Box<dyn std::error::Error>> {
    let mut connection = None;
    move |record| {
        if connection.is_none() {
            let opened = open()?;
            prepare_schema(&opened)?;
            connection = Some(opened);
        }
        insert(connection.as_ref().unwrap(), record)?;
        Ok(())
    }
}

fn open_database() -> Result<Connection, Box<dyn std::error::Error>> {
    let path = database_path(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))?;
    fs::create_dir_all(path.parent().ok_or("database directory is missing")?)?;
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    Ok(connection)
}

fn prepare_schema(connection: &Connection) -> rusqlite::Result<()> {
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
    Ok(())
}

fn insert(connection: &Connection, record: &Record) -> rusqlite::Result<()> {
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
    use crate::word::Stroke;
    use evdev::KeyCode;
    use std::sync::Arc;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn queues_owned_snapshots_before_conversion_even_for_unknown_layouts() {
        let (sender, receiver) = mpsc::channel();
        let statistics = Statistics { sender };
        let mut original = word("us");
        original.entries[0].layout = None;
        statistics.record_word(original.clone());
        statistics.record_switch_with_mode(
            None,
            Some(&original),
            &layout("ru"),
            UNIX_EPOCH,
            SwitchMode::Auto,
        );
        original.entries.clear();
        let typing = receiver.recv().unwrap();
        let PendingRecord::TypingWord(snapshot) = typing else {
            panic!("expected typing snapshot")
        };
        assert_eq!(snapshot.entries.len(), 3);
        assert!(snapshot.entries[0].layout.is_none());
        assert!(convert_record(PendingRecord::TypingWord(snapshot)).is_none());
        let switch = receiver.recv().unwrap();
        let PendingRecord::Switch {
            word: Some(snapshot),
            time,
            mode,
            ..
        } = &switch
        else {
            panic!("expected switch snapshot")
        };
        assert_eq!(snapshot.entries.len(), 3);
        assert_eq!(*time, UNIX_EPOCH);
        assert_eq!(*mode, SwitchMode::Auto);
        assert!(convert_record(switch).is_none());
    }

    #[test]
    fn worker_skips_unconvertible_snapshots_and_preserves_event_time_and_mode() {
        let (sent, received) = mpsc::channel();
        let statistics = Statistics::start_with_writer(move |record| {
            assert_eq!(std::thread::current().name(), Some("statistics"));
            sent.send(record.clone())?;
            Ok(())
        });
        let mut unknown = word("us");
        unknown.entries[0].layout = None;
        statistics.record_word(unknown);
        statistics.record_switch_with_mode(
            None,
            Some(&word("us")),
            &layout("ru"),
            UNIX_EPOCH + Duration::from_secs(123),
            SwitchMode::Auto,
        );
        drop(statistics);
        let Record::Switch(record) = received.recv_timeout(TEST_TIMEOUT).unwrap() else {
            panic!("expected switch record")
        };
        assert_eq!(record.timestamp, 123);
        assert_eq!(record.mode, SwitchMode::Auto);
        assert_eq!(record.a, "ghb");
        assert_eq!(record.b, "при");
        assert!(matches!(
            received.recv_timeout(TEST_TIMEOUT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn database_writer_retries_initialization_and_reuses_connection() {
        for is_open_error in [true, false] {
            let attempts = std::rc::Rc::new(std::cell::Cell::new(0));
            let open_attempts = attempts.clone();
            let mut writer = database_writer(move || {
                let attempt = open_attempts.get() + 1;
                open_attempts.set(attempt);
                if attempt == 1 && is_open_error {
                    return Err(io::Error::other("open failed").into());
                }
                let connection = Connection::open_in_memory()?;
                if attempt == 1 {
                    connection.execute_batch("PRAGMA query_only = ON")?;
                }
                Ok(connection)
            });
            let record = Record::TypingWord("example".into());
            assert!(writer(&record).is_err());
            writer(&record).unwrap();
            writer(&record).unwrap();
            assert_eq!(attempts.get(), 2);
        }
    }

    #[test]
    fn migrates_old_switches_as_manual_and_stores_auto_mode() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE switches (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp INTEGER NOT NULL, prev TEXT NOT NULL, a TEXT NOT NULL, b TEXT NOT NULL); INSERT INTO switches (timestamp, prev, a, b) VALUES (1, '', 'old', 'old');").unwrap();
        prepare_schema(&connection).unwrap();
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
        let entries = [KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_B]
            .into_iter()
            .map(|key| crate::word::WordStroke {
                stroke: Stroke { key, shift: false },
                layout: Some(layout(name)),
            })
            .collect();
        Word {
            entries,
            trailing_spaces: 2,
        }
    }

    fn observed_history() -> (crate::input::Input, Receiver<Record>) {
        let (sent, received) = mpsc::channel();
        let statistics = Statistics::start_with_writer(move |record| {
            sent.send(record.clone())?;
            Ok(())
        });
        let history = crate::input::tests::statistics_input(statistics);
        (history, received)
    }

    fn press(history: &mut crate::input::Input, key: KeyCode) {
        crate::input::tests::statistics_event(history, key, 1);
    }

    fn finish_history(history: crate::input::Input, received: Receiver<Record>) -> Vec<Record> {
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
                history.reset_history();
            }
            history.reset_history();
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
        history.reset_history();
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
        crate::input::tests::statistics_event(&mut history, KeyCode::KEY_LEFTSHIFT, 0);
        history.set_latest_layout(Some(layout("ru")));
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_A);
        press(&mut history, KeyCode::KEY_COMMA);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_B);
        history.set_latest_layout(None);
        press(&mut history, KeyCode::KEY_SPACE);
        press(&mut history, KeyCode::KEY_C);
        history.reset_history();
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
        prepare_schema(&connection).unwrap();
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
        unknown.entries[0].layout = None;
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
        prepare_schema(&connection).unwrap();
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
            let result = crate::switcher::switch_and_replay(
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
        crate::switcher::switch_and_replay(
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
        prepare_schema(&connection).unwrap();
        connection.execute_batch("PRAGMA query_only = ON").unwrap();
        let statistics = Statistics::start_with_writer(move |record| {
            let result = insert(&connection, record);
            sent.send(result.is_ok()).unwrap();
            connection.execute_batch("PRAGMA query_only = OFF")?;
            result.map_err(Into::into)
        });
        for is_expected_success in [false, true] {
            let mut is_replayed = false;
            crate::switcher::switch_and_replay(
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
        crate::switcher::switch_and_replay(
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
