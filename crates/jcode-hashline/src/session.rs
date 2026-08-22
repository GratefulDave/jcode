//! Process-wide session snapshot and clipboard maps.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::snapshots::SnapshotStore;
use crate::types::Clipboard;

struct SessionState {
    snapshots: SnapshotStore,
    clipboard: Clipboard,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<Mutex<SessionState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<SessionState>>>> {
    &SESSIONS
}

fn state(session_id: &str) -> Arc<Mutex<SessionState>> {
    let mut map = sessions().lock().expect("hashline session map");
    map.entry(session_id.to_string())
        .or_insert_with(|| {
            Arc::new(Mutex::new(SessionState {
                snapshots: SnapshotStore::new(),
                clipboard: Clipboard::default(),
            }))
        })
        .clone()
}

pub fn with_session<T>(session_id: &str, f: impl FnOnce(&mut SnapshotStore, &mut Clipboard) -> T) -> T {
    let slot = state(session_id);
    let mut guard = slot.lock().expect("hashline session");
    let SessionState {
        snapshots,
        clipboard,
    } = &mut *guard;
    f(snapshots, clipboard)
}

pub fn clear_session(session_id: &str) {
    if let Ok(mut map) = sessions().lock() {
        map.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_session_drops_snapshots() {
        let id = "clear-session-test";
        let tag = crate::record_snapshot(id, "/tmp/hashline-clear.txt", "hello\n", None::<[usize; 0]>)
            .expect("tag");
        assert!(!tag.is_empty());
        with_session(id, |store, _| {
            assert!(store.head("/tmp/hashline-clear.txt").is_some());
        });
        clear_session(id);
        with_session(id, |store, _| {
            assert!(store.head("/tmp/hashline-clear.txt").is_none());
        });
    }
}
