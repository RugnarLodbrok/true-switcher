use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use evdev::raw_stream::RawDevice;
use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent};

pub(super) const INPUT_DIRECTORY: &str = "/dev/input";
const DEVICE_PREFIX: &[u8] = b"event";

pub(super) type Identity = (u64, u64);

pub(super) struct DirectoryWatcher(Inotify);

impl DirectoryWatcher {
    pub(super) fn new(directory: &Path) -> io::Result<Self> {
        let watcher = Inotify::init(InitFlags::IN_CLOEXEC)?;
        watcher.add_watch(
            directory,
            AddWatchFlags::IN_CREATE
                | AddWatchFlags::IN_DELETE
                | AddWatchFlags::IN_MOVED_FROM
                | AddWatchFlags::IN_MOVED_TO
                | AddWatchFlags::IN_ATTRIB
                | AddWatchFlags::IN_DELETE_SELF
                | AddWatchFlags::IN_MOVE_SELF,
        )?;
        Ok(Self(watcher))
    }

    pub(super) fn read_events(&self) -> io::Result<Vec<InotifyEvent>> {
        loop {
            match self.0.read_events() {
                Ok(events) => return Ok(events),
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

pub(super) fn is_watch_lost(mask: AddWatchFlags) -> bool {
    mask.intersects(
        AddWatchFlags::IN_DELETE_SELF | AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_IGNORED,
    )
}

pub(super) fn is_device_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(DEVICE_PREFIX)
}

pub(super) fn device_paths(
    directory: &Path,
) -> io::Result<impl Iterator<Item = io::Result<PathBuf>>> {
    Ok(fs::read_dir(directory)?.filter_map(|entry| match entry {
        Ok(entry) if is_device_name(&entry.file_name()) => Some(Ok(entry.path())),
        Ok(_) => None,
        Err(error) => Some(Err(error)),
    }))
}

pub(super) fn open_device(path: &Path) -> io::Result<(Identity, RawDevice)> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let device = RawDevice::try_from(file)?;
    Ok(((metadata.rdev(), metadata.ino()), device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "true-switcher-device-watch-{}-{}",
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
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn discovery_and_notifications_use_the_same_device_name_filter() {
        let directory = TestDirectory::new();
        let watcher = DirectoryWatcher::new(&directory.0).unwrap();
        let names = [
            OsStr::new("event0"),
            OsStr::new("event"),
            OsStr::from_bytes(b"event\xff"),
            OsStr::new("mouse0"),
            OsStr::new("not-event0"),
        ];
        for name in names {
            File::create(directory.0.join(name)).unwrap();
        }
        let expected: std::collections::BTreeSet<_> = names[..3]
            .iter()
            .map(|name| directory.0.join(name))
            .collect();
        let discovered = device_paths(&directory.0)
            .unwrap()
            .collect::<io::Result<std::collections::BTreeSet<_>>>()
            .unwrap();
        assert_eq!(discovered, expected);
        let notified = watcher
            .read_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.mask.contains(AddWatchFlags::IN_CREATE))
            .filter_map(|event| event.name)
            .filter(|name| is_device_name(name))
            .map(|name| directory.0.join(name))
            .collect();
        assert_eq!(expected, notified);
    }

    #[test]
    fn moving_directory_loses_watch_but_device_removal_and_overflow_do_not() {
        let directory = TestDirectory::new();
        let watched_path = directory.0.join("input");
        fs::create_dir(&watched_path).unwrap();
        let watcher = DirectoryWatcher::new(&watched_path).unwrap();
        fs::rename(&watched_path, directory.0.join("moved-input")).unwrap();
        assert!(watcher.read_events().unwrap().iter().any(|event| {
            event.mask.contains(AddWatchFlags::IN_MOVE_SELF) && is_watch_lost(event.mask)
        }));
        assert!(is_watch_lost(AddWatchFlags::IN_DELETE_SELF));
        assert!(is_watch_lost(AddWatchFlags::IN_IGNORED));
        assert!(!is_watch_lost(AddWatchFlags::IN_DELETE));
        assert!(!is_watch_lost(AddWatchFlags::IN_MOVED_FROM));
        assert!(!is_watch_lost(AddWatchFlags::IN_Q_OVERFLOW));
    }
}
