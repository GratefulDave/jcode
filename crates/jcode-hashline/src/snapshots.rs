//! Per-session snapshot store binding 4-hex tags to full-file text.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::format::compute_file_hash;
use crate::types::{
    Snapshot, DEFAULT_MAX_PATHS, DEFAULT_MAX_VERSIONS_PER_PATH,
};

#[derive(Debug)]
pub struct SnapshotStore {
    max_paths: usize,
    max_versions_per_path: usize,
    order: VecDeque<String>,
    versions: HashMap<String, Vec<Snapshot>>,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_PATHS, DEFAULT_MAX_VERSIONS_PER_PATH)
    }

    pub fn with_limits(max_paths: usize, max_versions_per_path: usize) -> Self {
        Self {
            max_paths,
            max_versions_per_path,
            order: VecDeque::new(),
            versions: HashMap::new(),
        }
    }

    fn touch(&mut self, path: &str) {
        if let Some(idx) = self.order.iter().position(|p| p == path) {
            self.order.remove(idx);
        }
        self.order.push_front(path.to_string());
        while self.order.len() > self.max_paths {
            if let Some(evicted) = self.order.pop_back() {
                self.versions.remove(&evicted);
            }
        }
    }

    pub fn head(&self, path: &str) -> Option<&Snapshot> {
        self.versions.get(path).and_then(|h| h.first())
    }

    pub fn by_hash(&self, path: &str, hash: &str) -> Option<&Snapshot> {
        self.versions
            .get(path)
            .and_then(|h| h.iter().find(|v| v.hash == hash))
    }

    pub fn by_hash_mut(&mut self, path: &str, hash: &str) -> Option<&mut Snapshot> {
        self.versions
            .get_mut(path)
            .and_then(|h| h.iter_mut().find(|v| v.hash == hash))
    }

    pub fn by_content(&self, path: &str, full_text: &str) -> Option<&Snapshot> {
        self.versions
            .get(path)
            .and_then(|h| h.iter().find(|v| v.text == full_text))
    }

    pub fn by_content_mut(&mut self, path: &str, full_text: &str) -> Option<&mut Snapshot> {
        self.versions
            .get_mut(path)
            .and_then(|h| h.iter_mut().find(|v| v.text == full_text))
    }

    pub fn find_by_hash(&self, hash: &str) -> Vec<Snapshot> {
        self.versions
            .values()
            .flatten()
            .filter(|v| v.hash == hash)
            .cloned()
            .collect()
    }

    pub fn record(
        &mut self,
        path: &str,
        full_text: &str,
        seen_lines: Option<impl IntoIterator<Item = usize>>,
    ) -> String {
        let hash = compute_file_hash(full_text);
        self.touch(path);
        let history = self.versions.entry(path.to_string()).or_default();
        if let Some(pos) = history
            .iter()
            .position(|v| v.hash == hash && v.text == full_text)
        {
            merge_seen_lines(&mut history[pos], seen_lines);
            if pos != 0 {
                let snap = history.remove(pos);
                history.insert(0, snap);
            }
            return hash;
        }
        let mut snapshot = Snapshot {
            path: path.to_string(),
            text: full_text.to_string(),
            hash: hash.clone(),
            seen_lines: None,
        };
        merge_seen_lines(&mut snapshot, seen_lines);
        history.insert(0, snapshot);
        history.truncate(self.max_versions_per_path);
        hash
    }

    pub fn record_seen_lines(&mut self, path: &str, hash: &str, lines: impl IntoIterator<Item = usize>) {
        if let Some(version) = self.by_hash_mut(path, hash) {
            merge_seen_lines(version, Some(lines));
        }
    }

    pub fn invalidate(&mut self, path: &str) {
        self.versions.remove(path);
        if let Some(idx) = self.order.iter().position(|p| p == path) {
            self.order.remove(idx);
        }
    }

    pub fn relocate(&mut self, from: &str, to: &str) {
        let Some(source) = self.versions.remove(from) else {
            return;
        };
        if let Some(idx) = self.order.iter().position(|p| p == from) {
            self.order.remove(idx);
        }
        let relocated: Vec<Snapshot> = source
            .into_iter()
            .map(|mut v| {
                v.path = to.to_string();
                v
            })
            .collect();
        if let Some(dest) = self.versions.get_mut(to) {
            let mut seen = BTreeSet::new();
            let mut merged = Vec::new();
            for version in relocated.into_iter().chain(dest.drain(..)) {
                if seen.insert(version.hash.clone()) {
                    merged.push(version);
                }
            }
            merged.truncate(self.max_versions_per_path);
            *dest = merged;
        } else {
            self.versions.insert(to.to_string(), relocated);
        }
        self.touch(to);
    }

    pub fn clear(&mut self) {
        self.versions.clear();
        self.order.clear();
    }
}

fn merge_seen_lines(
    snapshot: &mut Snapshot,
    lines: Option<impl IntoIterator<Item = usize>>,
) {
    let Some(lines) = lines else {
        return;
    };
    let seen = snapshot.seen_lines.get_or_insert_with(BTreeSet::new);
    seen.extend(lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/tmp/__hashline-snapshots__.ts";
    const COLLIDE_A: &str = "line one 263\nline two 4471\n";
    const COLLIDE_B: &str = "line one 410\nline two 6970\n";

    #[test]
    fn fuses_identical_content() {
        let mut store = SnapshotStore::new();
        let a = store.record(PATH, "hello\n", Some([1]));
        let b = store.record(PATH, "hello\n", Some([2]));
        assert_eq!(a, b);
        assert_eq!(
            store.by_content(PATH, "hello\n").unwrap().seen_lines,
            Some(BTreeSet::from([1, 2]))
        );
    }

    #[test]
    fn collision_pair_stays_distinct() {
        let mut store = SnapshotStore::new();
        let tag_a = store.record(PATH, COLLIDE_A, Some([1]));
        let tag_b = store.record(PATH, COLLIDE_B, Some([2]));
        assert_eq!(tag_a, tag_b);
        assert_eq!(
            store.by_content(PATH, COLLIDE_A).unwrap().seen_lines,
            Some(BTreeSet::from([1]))
        );
        assert_eq!(
            store.by_content(PATH, COLLIDE_B).unwrap().seen_lines,
            Some(BTreeSet::from([2]))
        );
        assert_eq!(store.by_hash(PATH, &tag_a).unwrap().text, COLLIDE_B);
    }

    #[test]
    fn bounds_versions_per_path() {
        let mut store = SnapshotStore::with_limits(8, 2);
        store.record(PATH, "v1\n", None::<[usize; 0]>);
        store.record(PATH, "v2\n", None::<[usize; 0]>);
        store.record(PATH, "v3\n", None::<[usize; 0]>);
        assert!(store.by_content(PATH, "v1\n").is_none());
        assert!(store.by_content(PATH, "v2\n").is_some());
        assert!(store.by_content(PATH, "v3\n").is_some());
    }
}
