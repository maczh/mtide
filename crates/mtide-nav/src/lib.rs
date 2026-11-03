//! Navigation, bookmarks, local history, and symbol-outline state.
//!
//! The crate stores metadata and snapshots only; rendering and editor effects
//! remain owned by the binary so navigation state can be tested without a TTY.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NavError {
    #[error("navigation I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("navigation data is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecentFile {
    pub path: PathBuf,
    pub available: bool,
    pub last_opened: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub edited_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub path: PathBuf,
    pub line: usize,
    pub name: Option<String>,
    pub mnemonic: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub children: Vec<Symbol>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalSnapshot {
    pub id: String,
    pub path: PathBuf,
    pub created_at: u64,
    pub text: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NavigationState {
    pub recent_files: Vec<RecentFile>,
    pub positions: Vec<Position>,
    pub bookmarks: Vec<Bookmark>,
    pub snapshots: BTreeMap<PathBuf, Vec<LocalSnapshot>>,
}

impl NavigationState {
    pub fn open_file(&mut self, path: impl Into<PathBuf>, max: usize) {
        let path = path.into();
        let now = unix_seconds();
        self.recent_files.retain(|item| item.path != path);
        self.recent_files.insert(
            0,
            RecentFile {
                available: path.exists(),
                path,
                last_opened: now,
            },
        );
        self.recent_files.truncate(max.max(1));
    }

    pub fn refresh_availability(&mut self) {
        for item in &mut self.recent_files {
            item.available = item.path.exists();
        }
    }

    pub fn filter_recent(&self, query: &str, root: Option<&Path>) -> Vec<&RecentFile> {
        let query = query.to_ascii_lowercase();
        self.recent_files
            .iter()
            .filter(|item| {
                let matches_query = query.is_empty()
                    || item
                        .path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&query);
                let matches_root = root.map(|root| item.path.starts_with(root)).unwrap_or(true);
                matches_query && matches_root
            })
            .collect()
    }

    pub fn record_position(
        &mut self,
        path: impl Into<PathBuf>,
        line: usize,
        column: usize,
        max: usize,
    ) {
        let position = Position {
            path: path.into(),
            line,
            column,
            edited_at: unix_seconds(),
        };
        self.positions.retain(|item| item.path != position.path);
        self.positions.insert(0, position);
        self.positions.truncate(max.max(1));
    }

    pub fn set_bookmark(&mut self, bookmark: Bookmark) {
        self.bookmarks
            .retain(|item| item.path != bookmark.path || item.line != bookmark.line);
        self.bookmarks.push(bookmark);
        self.bookmarks
            .sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    }

    pub fn mnemonic(&self, number: u8) -> Option<&Bookmark> {
        self.bookmarks
            .iter()
            .find(|item| item.mnemonic == Some(number))
    }

    pub fn add_snapshot(&mut self, snapshot: LocalSnapshot, max_per_file: usize) {
        let entries = self.snapshots.entry(snapshot.path.clone()).or_default();
        entries.push(snapshot);
        entries.sort_by_key(|item| item.created_at);
        if entries.len() > max_per_file.max(1) {
            let excess = entries.len() - max_per_file.max(1);
            entries.drain(..excess);
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), NavError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, NavError> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
}

pub fn filter_symbols(symbols: &[Symbol], query: &str) -> Vec<Symbol> {
    let query = query.to_lowercase();
    symbols
        .iter()
        .filter_map(|symbol| {
            let mut value = symbol.clone();
            value.children = filter_symbols(&symbol.children, &query);
            (query.is_empty()
                || symbol.name.to_lowercase().contains(&query)
                || !value.children.is_empty())
            .then_some(value)
        })
        .collect()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fr_nv01_recent_files_are_bounded_and_deleted_files_remain_visible() {
        let mut state = NavigationState::default();
        state.open_file("/definitely/missing.rs", 2);
        state.open_file("/tmp/one.rs", 2);
        state.open_file("/tmp/two.rs", 2);
        state.refresh_availability();
        assert_eq!(state.recent_files.len(), 2);
        assert!(!state
            .recent_files
            .iter()
            .any(|item| item.path.ends_with("missing.rs")));
        assert!(state.recent_files.iter().all(|item| !item.available));
    }

    #[test]
    fn fr_nv02_positions_and_nv04_mnemonics_are_independent() {
        let mut state = NavigationState::default();
        state.record_position("a.rs", 10, 3, 50);
        state.record_position("b.rs", 2, 4, 50);
        state.set_bookmark(Bookmark {
            path: "a.rs".into(),
            line: 10,
            name: Some("entry".into()),
            mnemonic: Some(1),
        });
        assert_eq!(state.positions[0].path, PathBuf::from("b.rs"));
        assert_eq!(state.mnemonic(1).map(|item| item.line), Some(10));
    }

    #[test]
    fn fr_nv03_local_history_is_bounded_and_nv11_outline_filters_children() {
        let mut state = NavigationState::default();
        for index in 0..3 {
            state.add_snapshot(
                LocalSnapshot {
                    id: index.to_string(),
                    path: "main.rs".into(),
                    created_at: index,
                    text: index.to_string(),
                },
                2,
            );
        }
        assert_eq!(state.snapshots[&PathBuf::from("main.rs")].len(), 2);
        let symbols = vec![Symbol {
            name: "App".into(),
            kind: "struct".into(),
            line: 1,
            children: vec![Symbol {
                name: "draw".into(),
                kind: "method".into(),
                line: 2,
                children: vec![],
            }],
        }];
        assert_eq!(filter_symbols(&symbols, "draw")[0].children[0].name, "draw");
    }
}
