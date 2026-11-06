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
    b TEXT NOT NULL
)";

#[derive(Debug, PartialEq, Eq)]
struct Record {
    timestamp: i64,
    prev: String,
    a: String,
    b: String,
}

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
            eprintln!("true-switcher: cannot start statistics thread: {error}");
        }
        Self { sender }
    }

    pub fn record_switch(
        &self,
        previous: Option<&Word>,
        word: Option<&Word>,
        target: &Layout,
        time: SystemTime,
    ) {
        let Some(record) = make_record(previous, word, target, time) else {
            eprintln!("true-switcher: skipping statistics: cannot convert word to text");
            return;
        };
        if let Err(error) = self.sender.send(record) {
            eprintln!("true-switcher: statistics queue: {error}");
        }
    }
}

fn write_records(
    receiver: Receiver<Record>,
    mut writer: impl FnMut(&Record) -> Result<(), Box<dyn std::error::Error>>,
) {
    for record in receiver {
        if let Err(error) = writer(&record) {
            eprintln!("true-switcher: statistics database: {error}");
        }
    }
}

fn make_record(
    previous: Option<&Word>,
    word: Option<&Word>,
    target: &Layout,
    time: SystemTime,
) -> Option<Record> {
    Some(Record {
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
    connection.execute(
        "INSERT INTO switches (timestamp, prev, a, b) VALUES (?1, ?2, ?3, ?4)",
        params![record.timestamp, record.prev, record.a, record.b],
    )?;
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

    #[test]
    fn records_original_previous_word_and_both_versions_of_corrected_word() {
        let previous = word("ru");
        let original = word("us");
        let time = UNIX_EPOCH + Duration::from_secs(123);
        let record = make_record(Some(&previous), Some(&original), &layout("ru"), time).unwrap();
        assert_eq!(
            record,
            Record {
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
        let record = Record {
            timestamp: 123,
            prev: "привет".into(),
            a: "'\"; DROP TABLE switches;".into(),
            b: "слово".into(),
        };
        insert(&connection, &record).unwrap();
        insert(&connection, &record).unwrap();
        let stored = connection
            .query_row(
                "SELECT timestamp, prev, a, b FROM switches WHERE id = 2",
                [],
                |row| {
                    Ok(Record {
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
        insert(&connection, &record).unwrap();
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
        assert!(insert(&connection, &record).is_err());
    }

    #[test]
    fn queues_switch_snapshot_despite_replay_cancellation_or_error() {
        for is_cancelled in [true, false] {
            let (sent, received) = mpsc::channel();
            let statistics = Statistics::start_with_writer(move |record| {
                sent.send(Record {
                    timestamp: record.timestamp,
                    prev: record.prev.clone(),
                    a: record.a.clone(),
                    b: record.b.clone(),
                })
                .unwrap();
                Ok(())
            });
            let previous = word("ru");
            let original = word("us");
            let time = UNIX_EPOCH + Duration::from_secs(123);
            let result = crate::switch_and_replay(
                || Ok(()),
                || statistics.record_switch(Some(&previous), Some(&original), &layout("ru"), time),
                || Err(io::Error::other("replay failed")),
                &std::sync::atomic::AtomicBool::new(is_cancelled),
            );
            assert_eq!(result.is_ok(), is_cancelled);
            assert_eq!(
                received.recv_timeout(TEST_TIMEOUT).unwrap(),
                make_record(Some(&previous), Some(&original), &layout("ru"), time).unwrap()
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
