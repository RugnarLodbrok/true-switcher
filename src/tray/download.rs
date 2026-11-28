use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use tempfile::NamedTempFile;
use ureq::Agent;
use ureq::http::{StatusCode, Uri, uri::Scheme};
use ureq::tls::{RootCerts, TlsConfig};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue};

use crate::config::ConfigurationFile;
use crate::dictionary::DICTIONARY_COUNT;
use crate::layout::Layout;

pub(super) const SOURCES_FILE: &str = "dict-sources.toml";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const READ_BUFFER_SIZE: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Deserialize)]
pub(super) struct Source {
    pub(super) name: String,
    directory: String,
    link: String,
    url_template: String,
    layout_keys: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Sources {
    #[serde(default)]
    source: Vec<Source>,
}

#[derive(PartialEq, Eq)]
enum FileState {
    Missing,
    Present(SystemTime),
    Unreadable(io::ErrorKind),
}

pub(super) struct SourceFile {
    pub(super) path: PathBuf,
    state: FileState,
    pub(super) result: Result<Vec<Arc<Source>>, String>,
}

impl SourceFile {
    pub(super) fn new(path: PathBuf) -> Self {
        let state = Self::state(&path);
        let result = Self::read(&path, &state);
        Self {
            path,
            state,
            result,
        }
    }

    fn state(path: &Path) -> FileState {
        match fs::metadata(path).and_then(|metadata| metadata.modified()) {
            Ok(modified) => FileState::Present(modified),
            Err(error) if error.kind() == io::ErrorKind::NotFound => FileState::Missing,
            Err(error) => FileState::Unreadable(error.kind()),
        }
    }

    fn read(path: &Path, state: &FileState) -> Result<Vec<Arc<Source>>, String> {
        if *state == FileState::Missing {
            return Err(format!("error: {} is missing", path.display()));
        }
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("error: {}: {error}", path.display()))?;
        let sources: Sources = toml::from_str(&contents)
            .map_err(|error| format!("error: {}: {error}", path.display()))?;
        Ok(sources.source.into_iter().map(Arc::new).collect())
    }

    pub(super) fn refresh(&mut self) -> bool {
        let state = Self::state(&self.path);
        if state == self.state {
            return false;
        }
        self.result = Self::read(&self.path, &state);
        self.state = state;
        true
    }

    pub(super) fn is_present(&self) -> bool {
        self.state != FileState::Missing
    }
}

pub(super) fn distinct_layouts(layouts: &[Layout]) -> Vec<&Layout> {
    let mut seen = HashSet::new();
    layouts
        .iter()
        .filter(|layout| seen.insert(layout.name()))
        .collect()
}

pub(super) fn check_count(count: usize) -> io::Result<()> {
    match count {
        DICTIONARY_COUNT => Ok(()),
        ..DICTIONARY_COUNT => Err(io::Error::other(
            "Dictionary download requires two distinct layouts",
        )),
        _ => Err(io::Error::other(
            "Dictionary download supports at most two distinct layouts",
        )),
    }
}

enum Progress<'a> {
    Downloading {
        target: &'a str,
        received: u64,
        total: Option<u64>,
        is_finished: bool,
    },
    Checking(&'a str),
}

fn progress_message(progress: Progress<'_>) -> String {
    match progress {
        Progress::Downloading {
            target,
            received,
            total: Some(total),
            ..
        } if total > 0 => {
            let percent = received.saturating_mul(100) / total;
            format!("Downloading {target}: {percent}%")
        }
        Progress::Downloading {
            target, received, ..
        } => {
            format!("Downloading {target}: {received} bytes")
        }
        Progress::Checking(target) => format!("Checking dictionary for {target}"),
    }
}

struct Notification<'a> {
    connection: &'a Connection,
    id: u32,
    last_update: Option<Instant>,
}

impl Notification<'_> {
    fn show(&mut self, summary: &str, message: &str) {
        match super::notify_replacing(self.connection, self.id, summary, message) {
            Ok(id) => self.id = id,
            Err(error) => log::warn!("true-switcher: cannot send download notification: {error}"),
        }
        self.last_update = Some(Instant::now());
    }

    fn progress(&mut self, progress: Progress<'_>) {
        if matches!(
            progress,
            Progress::Checking(_)
                | Progress::Downloading { received: 0, .. }
                | Progress::Downloading {
                    is_finished: true,
                    ..
                }
        ) || self
            .last_update
            .is_none_or(|last| last.elapsed() >= PROGRESS_INTERVAL)
        {
            self.show("Dictionary download", &progress_message(progress));
        }
    }
}

pub(super) type ActiveLayouts = dyn Fn() -> io::Result<Vec<Layout>> + Send + Sync;

pub(super) fn run(
    connection: &Connection,
    source: &Source,
    config: &ConfigurationFile,
    active_layouts: &ActiveLayouts,
    reload: impl FnOnce() -> io::Result<()>,
) {
    let mut notification = Notification {
        connection,
        id: 0,
        last_update: None,
    };
    notification.show("Dictionary download", "Preparing dictionaries");
    match connect(source, config, active_layouts, |event| {
        notification.progress(event)
    }) {
        Ok(()) => {
            notification.show("Dictionaries saved", "Reloading True Switcher");
            if let Err(error) = reload() {
                let message =
                    format!("Dictionaries were saved. Cannot restart True Switcher: {error}");
                log::warn!("true-switcher: {message}");
                notification.show("Reload failed", &message);
            }
        }
        Err(error) => {
            log::warn!("true-switcher: dictionary download: {error}");
            notification.show("Dictionary download failed", &error.to_string());
        }
    }
}

struct Download {
    target: String,
    key: String,
    url: Uri,
    path: PathBuf,
}

impl Download {
    fn plan(source: &Source, layout: &Layout, sources_path: &Path) -> io::Result<Self> {
        let target = layout.name();
        let key = if layout.variant().is_empty() {
            None
        } else {
            source
                .layout_keys
                .get(&format!("{target}({})", layout.variant()))
        }
        .or_else(|| source.layout_keys.get(target))
        .ok_or_else(|| {
            io::Error::other(format!(
                "No dictionary mapping for {target} (variant {:?}) in {}",
                layout.variant(),
                sources_path.display()
            ))
        })?;
        if !source.url_template.contains("{key}") {
            return Err(io::Error::other(format!(
                "Missing {{key}} in url_template in {}",
                sources_path.display()
            )));
        }
        let url = http_url(&source.url_template.replace("{key}", key))?;
        let name = url
            .path()
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| io::Error::other("Missing dictionary filename in URL"))?;
        let path = PathBuf::from("dicts")
            .join(path_component(&source.directory)?)
            .join(path_component(target)?)
            .join(path_component(name)?);
        Ok(Self {
            target: target.into(),
            key: key.clone(),
            url,
            path,
        })
    }

    fn receive(
        &self,
        client: &Agent,
        sources_path: &Path,
        output: &mut File,
        mut progress: impl FnMut(Progress<'_>),
    ) -> io::Result<()> {
        let mut response = client
            .get(self.url.clone())
            .call()
            .map_err(io::Error::other)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(io::Error::other(format!(
                "Dictionary not found for {} (key {:?}) in {}",
                self.target,
                self.key,
                sources_path.display()
            )));
        }
        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "HTTP {} downloading {}",
                response.status(),
                self.url
            )));
        }
        let total = response.body().content_length();
        let mut reader = response.body_mut().as_reader();
        let mut received = 0;
        let mut buffer = vec![0; READ_BUFFER_SIZE];
        loop {
            progress(Progress::Downloading {
                target: &self.target,
                received,
                total,
                is_finished: false,
            });
            let size = reader.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            output.write_all(&buffer[..size])?;
            received += size as u64;
        }
        progress(Progress::Downloading {
            target: &self.target,
            received,
            total,
            is_finished: true,
        });
        if total.is_some_and(|total| total != received) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete dictionary download",
            ));
        }
        Ok(())
    }

    fn install(
        &self,
        directory: &Path,
        client: &Agent,
        sources_path: &Path,
        skip_pattern: &str,
        mut progress: impl FnMut(Progress<'_>),
    ) -> io::Result<()> {
        let path = directory.join(&self.path);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                progress(Progress::Checking(&self.target));
                return crate::dictionary::validate_file(&path, skip_pattern);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = path.parent().expect("dictionary path includes a directory");
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        self.receive(client, sources_path, temporary.as_file_mut(), &mut progress)?;
        progress(Progress::Checking(&self.target));
        crate::dictionary::validate_file(temporary.path(), skip_pattern).map_err(|error| {
            io::Error::other(format!("Dictionary for {}: {error}", self.target))
        })?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                crate::dictionary::validate_file(&path, skip_pattern)
            }
            Err(error) => Err(error.error),
        }
    }
}

fn connect(
    source: &Source,
    config: &ConfigurationFile,
    active_layouts: &ActiveLayouts,
    mut progress: impl FnMut(Progress<'_>),
) -> io::Result<()> {
    let layouts = active_layouts()?;
    let selected = distinct_layouts(&layouts);
    check_count(selected.len())?;
    let sources_path = config.directory().join(SOURCES_FILE);
    let downloads = selected
        .into_iter()
        .map(|layout| Download::plan(source, layout, &sources_path))
        .collect::<io::Result<Vec<_>>>()?;
    let skip_pattern = config.dictionary_skip_pattern()?;
    let client = Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    for download in &downloads {
        download.install(
            config.directory(),
            &client,
            &sources_path,
            &skip_pattern,
            &mut progress,
        )?;
    }
    if active_layouts()? != layouts {
        return Err(io::Error::other(
            "KDE layouts changed during dictionary download",
        ));
    }
    config.connect_dictionaries(
        downloads
            .iter()
            .map(|download| (download.target.as_str(), download.path.as_path())),
        &skip_pattern,
    )
}

fn http_url(text: &str) -> io::Result<Uri> {
    let url: Uri = text.parse().map_err(io::Error::other)?;
    let is_http = url
        .scheme()
        .is_some_and(|scheme| scheme == &Scheme::HTTP || scheme == &Scheme::HTTPS);
    if !is_http || url.host().is_none_or(str::is_empty) {
        return Err(io::Error::other(
            "Dictionary source requires an HTTP or HTTPS URL",
        ));
    }
    Ok(url)
}

fn path_component(value: &str) -> io::Result<&str> {
    let mut parts = Path::new(value).components();
    if matches!(parts.next(), Some(Component::Normal(part)) if part == value)
        && parts.next().is_none()
    {
        return Ok(value);
    }
    Err(io::Error::other(format!(
        "Invalid dictionary path component {value:?}"
    )))
}

pub(super) enum OpenTarget {
    Link(Arc<Source>),
    File(PathBuf),
}

impl OpenTarget {
    pub(super) fn open(&self, connection: &Connection) -> io::Result<()> {
        let proxy =
            Proxy::new(connection, PORTAL, PORTAL_PATH, OPEN_URI).map_err(io::Error::other)?;
        let mut options: HashMap<&str, OwnedValue> = HashMap::new();
        let _: OwnedObjectPath = match self {
            Self::Link(source) => {
                http_url(&source.link)?;
                proxy.call("OpenURI", &("", source.link.as_str(), options))
            }
            Self::File(path) => {
                let file = File::open(path)?;
                options.insert("writable", true.into());
                proxy.call("OpenFile", &("", Fd::from(&file), options))
            }
        }
        .map_err(io::Error::other)?;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct DownloadSlot(Mutex<bool>);

impl DownloadSlot {
    pub(super) fn reserve(self: &Arc<Self>) -> Option<DownloadReservation> {
        let mut is_running = self.0.lock().unwrap();
        if *is_running {
            return None;
        }
        *is_running = true;
        Some(DownloadReservation(self.clone()))
    }
}

pub(super) struct DownloadReservation(Arc<DownloadSlot>);

impl Drop for DownloadReservation {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = false;
    }
}

#[cfg(test)]
mod tests;
