use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;
use crate::config::{RuntimeSetting, RuntimeSettings};

const BASE_CONFIG: &str = "switch_key = 'KEY_PAUSE'\nauto_switch = true\n";
const RECORD: &str = "word 1\n";

fn layouts() -> io::Result<Vec<Layout>> {
    Ok(vec![
        Layout::new("us", "dvorak", "", ""),
        Layout::new("us", "rus", "", ""),
        Layout::new("ru", "", "", ""),
    ])
}

fn source(directory: &str, url_template: String) -> Source {
    Source {
        name: directory.into(),
        directory: directory.into(),
        link: "https://example.test".into(),
        url_template,
        layout_keys: [("us", "en"), ("us(rus)", "russian"), ("ru", "ru")]
            .into_iter()
            .map(|(layout, key)| (layout.into(), key.into()))
            .collect(),
    }
}

fn configured(contents: &str) -> (tempfile::TempDir, ConfigurationFile, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, contents).unwrap();
    let (_, file) = crate::config::load_from(&path).unwrap();
    (directory, file, path)
}

fn server() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://{}/{{key}}/words.txt",
        listener.local_addr().unwrap()
    );
    (listener, url)
}

fn request(listener: &TcpListener, expected: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("Expected HTTP request for {expected}: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, format!("GET {expected} HTTP/1.1\r\n"));
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
    }
    stream
}

fn respond(mut stream: TcpStream, status: &str, body: &str, has_length: bool) {
    write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\n").unwrap();
    if has_length {
        write!(stream, "Content-Length: {}\r\n", body.len()).unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
}

fn write_dictionary(directory: &Path, relative_path: &str, content: &str) {
    let path = directory.join(relative_path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn downloads_first_variants_and_merges_configuration_changes_while_in_flight() {
    let initial = format!(
        "{BASE_CONFIG}\n[[dict]]\ntarget = 'us'\nfile = 'old.txt'\nthreshold = 3.0\n\n[[dict]]\ntarget = 'de'\nfile = 'untouched.txt'\nthreshold = 4.0\n"
    );
    let (directory, config, path) = configured(&initial);
    let config = Arc::new(config);
    let (listener, url) = server();
    let changing_config = config.clone();
    let disk_path = path.clone();
    let worker = thread::spawn(move || {
        let request_en = request(&listener, "/en/words.txt");
        fs::write(
            &disk_path,
            initial.replace("threshold = 3.0", "threshold = 4.5"),
        )
        .unwrap();
        let mut settings = RuntimeSettings::default();
        settings.toggle(RuntimeSetting::PatternSwitch);
        changing_config
            .save_setting(RuntimeSetting::PatternSwitch, settings)
            .unwrap();
        respond(request_en, "200 OK", RECORD, true);
        respond(request(&listener, "/ru/words.txt"), "200 OK", RECORD, false);
    });

    let mut updates = Vec::new();
    connect(&source("first", url), &config, &layouts, |event| {
        updates.push(progress_message(event))
    })
    .unwrap();
    worker.join().unwrap();
    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("auto_switch = false"));
    assert!(contents.contains("pattern_switch = true"));
    assert!(contents.contains("threshold = 4.5"));
    assert!(contents.contains("threshold = 15.0"));
    assert!(contents.contains("file = 'untouched.txt'"));
    assert!(contents.contains("dicts/first/us/words.txt"));
    assert!(contents.contains("dicts/first/ru/words.txt"));
    assert_eq!(
        fs::read_to_string(directory.path().join("dicts/first/us/words.txt")).unwrap(),
        RECORD
    );
    assert!(
        updates
            .iter()
            .any(|update| update == "Downloading us: 100%")
    );
    assert!(
        updates
            .iter()
            .any(|update| update == "Downloading ru: 7 bytes")
    );
    assert!(
        updates
            .iter()
            .any(|update| update == "Checking dictionary for ru")
    );

    fs::remove_file(&path).unwrap();
    config.save_log_level(log::LevelFilter::Debug).unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("dicts/first/ru/words.txt")
    );
}

#[test]
fn rejects_layout_count_and_missing_first_variant_mapping_before_http() {
    let (_directory, config, path) = configured(BASE_CONFIG);
    let (listener, url) = server();
    let mut source = source("first", url);
    for (names, error) in [
        (vec!["us"], "requires two distinct layouts"),
        (
            vec!["us", "ru", "de"],
            "supports at most two distinct layouts",
        ),
    ] {
        let active_layouts = move || {
            Ok(names
                .iter()
                .map(|name| Layout::new(name, "", "", ""))
                .collect())
        };
        assert!(
            connect(&source, &config, &active_layouts, |_| {})
                .unwrap_err()
                .to_string()
                .contains(error)
        );
    }
    source.layout_keys.remove("us");
    let error = connect(&source, &config, &layouts, |_| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("No dictionary mapping for us"));
    assert!(error.contains("dvorak"));
    assert!(error.contains(&config.directory().join(SOURCES_FILE).display().to_string()));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read_to_string(path).unwrap(), BASE_CONFIG);
}

#[test]
fn maps_an_exact_first_variant_before_falling_back_to_its_base() {
    let source = source("first", "https://example.test/{key}_full.txt".into());
    let sources_path = Path::new(SOURCES_FILE);
    let exact = Download::plan(&source, &Layout::new("us", "rus", "", ""), sources_path).unwrap();
    let fallback =
        Download::plan(&source, &Layout::new("us", "dvorak", "", ""), sources_path).unwrap();
    assert_eq!(exact.url.path(), "/russian_full.txt");
    assert_eq!(fallback.url.path(), "/en_full.txt");
    assert_eq!(fallback.path, Path::new("dicts/first/us/en_full.txt"));
}

#[test]
fn requires_absolute_http_urls_and_keeps_query_out_of_the_filename() {
    for url in ["words.txt", "//example.test/words.txt", "file:///words.txt"] {
        assert!(http_url(url).is_err(), "{url}");
    }

    let layout = Layout::new("us", "", "", "");
    let source = source("first", "https://example.test/{key}.txt?download=1".into());
    let download = Download::plan(&source, &layout, Path::new(SOURCES_FILE)).unwrap();
    assert_eq!(download.url.query(), Some("download=1"));
    assert_eq!(download.path, Path::new("dicts/first/us/en.txt"));
}

#[test]
fn reports_http_and_format_errors_without_altering_the_config_or_caching_invalid_downloads() {
    let (directory, config, path) = configured(BASE_CONFIG);
    for (status, body, message) in [
        (
            "404 Not Found",
            "",
            "Dictionary not found for us (key \"en\")",
        ),
        ("502 Bad Gateway", "", "HTTP 502"),
        ("200 OK", "bad input", "Invalid dictionary record"),
    ] {
        let (listener, url) = server();
        let worker =
            thread::spawn(move || respond(request(&listener, "/en/words.txt"), status, body, true));
        let error = connect(&source("first", url), &config, &layouts, |_| {})
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{error}");
        if status.starts_with("404") {
            assert!(error.contains(&config.directory().join(SOURCES_FILE).display().to_string()));
        }
        worker.join().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), BASE_CONFIG);
        assert!(!directory.path().join("dicts/first/us/words.txt").exists());
    }
}

#[test]
fn reuses_existing_and_concurrently_created_files_with_source_specific_paths() {
    const REUSED: &str = "existing 1\n";
    const CREATED: &str = "concurrent 1\n";
    let (directory, config, _) = configured(BASE_CONFIG);
    write_dictionary(directory.path(), "dicts/old/us/words.txt", REUSED);
    write_dictionary(directory.path(), "dicts/new/us/words.txt", REUSED);
    let (listener, url) = server();
    let root = directory.path().to_path_buf();
    let worker = thread::spawn(move || {
        let pending_request = request(&listener, "/ru/words.txt");
        write_dictionary(&root, "dicts/new/ru/words.txt", CREATED);
        respond(pending_request, "200 OK", RECORD, true);
    });
    connect(&source("new", url.clone()), &config, &layouts, |_| {}).unwrap();
    worker.join().unwrap();
    let read = |path| fs::read_to_string(directory.path().join(path)).unwrap();
    assert_eq!(read("dicts/old/us/words.txt"), REUSED);
    assert_eq!(read("dicts/new/us/words.txt"), REUSED);
    assert_eq!(read("dicts/new/ru/words.txt"), CREATED);
    write_dictionary(directory.path(), "dicts/new/ru/words.txt", "invalid");
    let previous = read("config.toml");
    assert!(connect(&source("new", url), &config, &layouts, |_| {}).is_err());
    assert_eq!(read("dicts/new/ru/words.txt"), "invalid");
    assert_eq!(read("config.toml"), previous);
}

#[test]
fn changed_layout_order_aborts_configuration_commit() {
    let (directory, config, path) = configured(BASE_CONFIG);
    for target in ["us", "ru"] {
        write_dictionary(
            directory.path(),
            &format!("dicts/cached/{target}/words.txt"),
            RECORD,
        );
    }
    let counter = std::sync::atomic::AtomicUsize::new(0);
    let active_layouts = move || {
        let mut layouts = layouts()?;
        if counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
            layouts.reverse();
        }
        Ok(layouts)
    };
    let source = source("cached", "https://example.test/{key}/words.txt".into());
    assert!(
        connect(&source, &config, &active_layouts, |_| {})
            .unwrap_err()
            .to_string()
            .contains("KDE layouts changed")
    );
    assert_eq!(fs::read_to_string(path).unwrap(), BASE_CONFIG);
}

#[test]
fn download_slot_rejects_duplicates_and_reopens_after_completion() {
    let slot = Arc::new(DownloadSlot::default());
    let active = slot.reserve().unwrap();
    assert!(slot.reserve().is_none());
    drop(active);
    assert!(slot.reserve().is_some());
}
