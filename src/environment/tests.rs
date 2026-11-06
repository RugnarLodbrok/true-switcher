use super::*;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        loop {
            let path = env::temp_dir().join(format!(
                "true-switcher-qdbus-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }

    fn executable(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn desktop(&self) -> KdePlasma {
        KdePlasma {
            qdbus: find_qdbus(self.0.as_os_str()).unwrap(),
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn accepts_each_supported_name() {
    for name in QDBUS_NAMES {
        let directory = TestDirectory::new();
        let executable = directory.executable(name, "exit 0");
        assert_eq!(
            directory.desktop().qdbus,
            executable.canonicalize().unwrap()
        );
    }
}

#[test]
fn prefers_qt6_names_across_path_directories() {
    let first = TestDirectory::new();
    let second = TestDirectory::new();
    first.executable("qdbus", "exit 0");
    let qt6 = second.executable("qdbus6", "exit 0");
    let path = env::join_paths([&first.0, &second.0]).unwrap();
    assert_eq!(find_qdbus(&path).unwrap(), qt6.canonicalize().unwrap());
    let preferred = second.executable("qdbus-qt6", "exit 0");
    assert_eq!(
        find_qdbus(&path).unwrap(),
        preferred.canonicalize().unwrap()
    );
}

#[test]
fn skips_non_executable_files_and_directories() {
    let directory = TestDirectory::new();
    fs::write(directory.0.join("qdbus-qt6"), "not executable").unwrap();
    fs::create_dir(directory.0.join("qdbus6")).unwrap();
    let executable = directory.executable("qdbus", "exit 0");
    assert_eq!(
        directory.desktop().qdbus,
        executable.canonicalize().unwrap()
    );
}

#[test]
fn reports_missing_commands_and_searched_path() {
    let directory = TestDirectory::new();
    let error = find_qdbus(directory.0.as_os_str()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    for name in QDBUS_NAMES {
        assert!(message.contains(name));
    }
    assert!(message.contains(directory.0.to_str().unwrap()));
}

#[test]
fn invokes_selected_command_with_kde_arguments() {
    let directory = TestDirectory::new();
    directory.executable(
        "qdbus-qt6",
        "[ \"$#\" -eq 3 ] && [ \"$1\" = org.kde.keyboard ] && [ \"$2\" = /Layouts ] && [ \"$3\" = org.kde.KeyboardLayouts.switchToNextLayout ]",
    );
    directory.desktop().switch_to_next_layout().unwrap();
}

#[test]
fn preserves_symlink_name_when_invoking_command() {
    let directory = TestDirectory::new();
    let target = directory.executable("qt-tool", "[ \"${0##*/}\" = qdbus-qt6 ]");
    let link = directory.0.join("qdbus-qt6");
    std::os::unix::fs::symlink(target, &link).unwrap();
    let desktop = directory.desktop();
    assert_eq!(desktop.qdbus, link);
    desktop.switch_to_next_layout().unwrap();
}

fn assert_no_fallback(directory: &TestDirectory) {
    assert!(!directory.0.join("fallback-called").exists());
}

#[test]
fn reports_command_failure_without_trying_another_name() {
    for (body, expected) in [
        ("printf 'dbus denied' >&2; exit 7", "dbus denied"),
        ("printf 'stdout error'; exit 7", "stdout error"),
    ] {
        let directory = TestDirectory::new();
        let executable = directory.executable("qdbus-qt6", body);
        directory.executable("qdbus6", "echo called > \"${0%/*}/fallback-called\"");
        let error = directory.desktop().switch_to_next_layout().unwrap_err();
        let message = error.to_string();
        assert!(message.contains(executable.to_str().unwrap()));
        assert!(message.contains(expected));
        assert!(message.contains('7'));
        assert_no_fallback(&directory);
    }
}

#[test]
fn retains_selected_path_and_reports_spawn_failure_without_fallback() {
    let directory = TestDirectory::new();
    let executable = directory.executable("qdbus6", "exit 0");
    let desktop = directory.desktop();
    directory.executable("qdbus-qt6", "echo called > \"${0%/*}/fallback-called\"");
    fs::remove_file(&executable).unwrap();
    let error = desktop.switch_to_next_layout().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains(executable.to_str().unwrap()));
    assert_no_fallback(&directory);
}
