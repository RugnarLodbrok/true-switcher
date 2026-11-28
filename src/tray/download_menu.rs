use std::path::PathBuf;
use std::sync::Arc;

use super::download::{self, OpenTarget, Source, SourceFile};
use super::menu::{Properties, item_properties, separator_properties, submenu_properties};
use crate::layout::Layout;

pub(super) const DOWNLOAD_MENU_ID: i32 = 15;
const SOURCES_SEPARATOR_ID: i32 = 16;
const OPEN_FILE_ID: i32 = 17;
const ERROR_ID: i32 = 18;
const FIRST_SOURCE_ID: i32 = 19;
const ITEMS_PER_SOURCE: i32 = 3;
const OPEN_LINK_OFFSET: i32 = 1;
const START_DOWNLOAD_OFFSET: i32 = 2;

pub(super) enum Action {
    Open(OpenTarget),
    Download(Arc<Source>),
}

pub(super) struct DownloadMenu {
    sources: SourceFile,
    first_source_id: i32,
    layout_error: Option<String>,
    revision: u32,
}

impl DownloadMenu {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            sources: SourceFile::new(path),
            first_source_id: FIRST_SOURCE_ID,
            layout_error: None,
            revision: 1,
        }
    }

    pub(super) fn refresh(&mut self, layouts: std::io::Result<Vec<Layout>>) -> bool {
        let layout_error = layouts
            .and_then(|layouts| download::check_count(download::distinct_layouts(&layouts).len()))
            .err()
            .map(|error| format!("Error: {error}"));
        let old_count = self.sources.result.as_ref().map_or(0, Vec::len);
        let is_file_changed = self.sources.refresh();
        if is_file_changed {
            self.first_source_id += i32::try_from(old_count).unwrap() * ITEMS_PER_SOURCE;
        }
        let is_changed = is_file_changed || self.layout_error != layout_error;
        self.layout_error = layout_error;
        if is_changed {
            self.revision += 1;
        }
        is_changed
    }

    pub(super) fn revision(&self) -> u32 {
        self.revision
    }

    fn error(&self) -> Option<&str> {
        self.layout_error
            .as_deref()
            .or(self.sources.result.as_ref().err().map(String::as_str))
    }

    pub(super) fn children(&self, id: i32) -> Vec<i32> {
        if id == DOWNLOAD_MENU_ID {
            let mut ids = if self.error().is_some() {
                vec![ERROR_ID]
            } else {
                let count = self.sources.result.as_ref().map_or(0, Vec::len) as i32;
                (0..count)
                    .map(|index| self.first_source_id + index * ITEMS_PER_SOURCE)
                    .collect()
            };
            if self.sources.is_present() {
                ids.extend([SOURCES_SEPARATOR_ID, OPEN_FILE_ID]);
            }
            return ids;
        }
        if let Some((_, 0)) = self.source_item(id) {
            return vec![id + OPEN_LINK_OFFSET, id + START_DOWNLOAD_OFFSET];
        }
        Vec::new()
    }

    fn source_item(&self, id: i32) -> Option<(&Arc<Source>, i32)> {
        if self.error().is_some() || id < self.first_source_id {
            return None;
        }
        let offset = id - self.first_source_id;
        let source = self
            .sources
            .result
            .as_ref()
            .ok()?
            .get((offset / ITEMS_PER_SOURCE) as usize)?;
        Some((source, offset % ITEMS_PER_SOURCE))
    }

    pub(super) fn properties(&self, id: i32) -> Option<Properties> {
        match id {
            DOWNLOAD_MENU_ID => Some(submenu_properties("Download dictionaries")),
            SOURCES_SEPARATOR_ID if self.sources.is_present() => Some(separator_properties()),
            OPEN_FILE_ID if self.sources.is_present() => {
                Some(item_properties("Open dict-sources.toml", true))
            }
            ERROR_ID => self.error().map(|error| item_properties(error, false)),
            _ => self.source_item(id).map(|(source, item)| match item {
                OPEN_LINK_OFFSET => item_properties("Open in browser", true),
                START_DOWNLOAD_OFFSET => item_properties("Download", true),
                _ => submenu_properties(&source.name),
            }),
        }
    }

    pub(super) fn action(&self, id: i32) -> Option<Action> {
        if id == OPEN_FILE_ID && self.sources.is_present() {
            return Some(Action::Open(OpenTarget::File(self.sources.path.clone())));
        }
        match self.source_item(id)? {
            (source, OPEN_LINK_OFFSET) => Some(Action::Open(OpenTarget::Link(source.clone()))),
            (source, START_DOWNLOAD_OFFSET) => Some(Action::Download(source.clone())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn menu_checks_distinct_targets_and_allows_multiple_variants_of_one_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(download::SOURCES_FILE);
        fs::write(
            &path,
            include_str!("../../installer/dict-sources.example.toml"),
        )
        .unwrap();
        let mut menu = DownloadMenu::new(path);
        let us = Layout::new("us", "", "", "");
        let dvorak = Layout::new("us", "dvorak", "", "");
        let ru = Layout::new("ru", "", "", "");
        let de = Layout::new("de", "", "", "");
        for (layouts, error) in [
            (
                vec![us.clone(), dvorak.clone()],
                "requires two distinct layouts",
            ),
            (
                vec![us.clone(), ru.clone(), de],
                "supports at most two distinct layouts",
            ),
        ] {
            assert!(menu.refresh(Ok(layouts)));
            let children = menu.children(DOWNLOAD_MENU_ID);
            assert_eq!(children, [ERROR_ID, SOURCES_SEPARATOR_ID, OPEN_FILE_ID]);
            let properties = menu.properties(ERROR_ID).unwrap();
            assert!(
                <&str>::try_from(&properties["label"])
                    .unwrap()
                    .contains(error)
            );
            assert!(!bool::try_from(&properties["enabled"]).unwrap());
        }
        assert!(menu.refresh(Ok(vec![us, dvorak, ru])));
        assert_eq!(menu.children(DOWNLOAD_MENU_ID).len(), 4);
    }
}
