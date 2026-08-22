//! Recover stale tags when every anchored line remaps with a uniform offset.

use std::collections::HashMap;

use similar::{ChangeTag, TextDiff};

use crate::apply::apply_edits;
use crate::snapshots::SnapshotStore;
use crate::types::{Anchor, ApplyResult, Clipboard, Cursor, Edit, ParsedRange, PasteTarget};

#[derive(Debug, Clone)]
pub struct RecoveryResult {
    pub apply: ApplyResult,
    pub warning: String,
}

fn build_line_map(previous: &str, current: &str) -> HashMap<usize, usize> {
    let old: Vec<&str> = previous.split('\n').collect();
    let new: Vec<&str> = current.split('\n').collect();
    let diff = TextDiff::from_slices(&old, &new);
    let mut map = HashMap::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Equal => {
                    map.insert(old_line, new_line);
                    old_line += 1;
                    new_line += 1;
                }
                ChangeTag::Delete => {
                    old_line += 1;
                }
                ChangeTag::Insert => {
                    new_line += 1;
                }
            }
        }
    }
    map
}

fn remap_cursor(cursor: &Cursor, map: &HashMap<usize, usize>) -> Option<Cursor> {
    match cursor {
        Cursor::Bof | Cursor::Eof => Some(cursor.clone()),
        Cursor::BeforeAnchor { anchor } => Some(Cursor::BeforeAnchor {
            anchor: Anchor {
                line: *map.get(&anchor.line)?,
            },
        }),
        Cursor::AfterAnchor { anchor } => Some(Cursor::AfterAnchor {
            anchor: Anchor {
                line: *map.get(&anchor.line)?,
            },
        }),
    }
}

fn remap_range(range: &ParsedRange, map: &HashMap<usize, usize>) -> Option<ParsedRange> {
    Some(ParsedRange {
        start: Anchor {
            line: *map.get(&range.start.line)?,
        },
        end: Anchor {
            line: *map.get(&range.end.line)?,
        },
    })
}

fn remap_edit(edit: &Edit, map: &HashMap<usize, usize>) -> Option<Edit> {
    match edit {
        Edit::Insert {
            cursor,
            text,
            line_num,
            index,
            replacement,
            block_start,
        } => Some(Edit::Insert {
            cursor: remap_cursor(cursor, map)?,
            text: text.clone(),
            line_num: *line_num,
            index: *index,
            replacement: *replacement,
            block_start: block_start.and_then(|line| map.get(&line).copied()),
        }),
        Edit::Delete {
            anchor,
            line_num,
            index,
        } => Some(Edit::Delete {
            anchor: Anchor {
                line: *map.get(&anchor.line)?,
            },
            line_num: *line_num,
            index: *index,
        }),
        Edit::Cut {
            range,
            register,
            line_num,
            index,
        } => Some(Edit::Cut {
            range: remap_range(range, map)?,
            register: register.clone(),
            line_num: *line_num,
            index: *index,
        }),
        Edit::Paste {
            at,
            register,
            line_num,
            index,
            block_start,
        } => {
            let at = match at {
                PasteTarget::Gap { cursor } => PasteTarget::Gap {
                    cursor: remap_cursor(cursor, map)?,
                },
                PasteTarget::Span { range } => PasteTarget::Span {
                    range: remap_range(range, map)?,
                },
            };
            Some(Edit::Paste {
                at,
                register: register.clone(),
                line_num: *line_num,
                index: *index,
                block_start: block_start.and_then(|line| map.get(&line).copied()),
            })
        }
        Edit::Block { .. } => None,
    }
}

fn collect_anchor_lines(edits: &[Edit]) -> Vec<usize> {
    let mut lines: Vec<usize> = edits
        .iter()
        .flat_map(|edit| edit.anchors())
        .map(|anchor| anchor.line)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn uniform_offsets(anchors: &[usize], map: &HashMap<usize, usize>) -> Option<i64> {
    let mut offset = None;
    for line in anchors {
        let mapped = *map.get(line)?;
        let delta = mapped as i64 - *line as i64;
        match offset {
            None => offset = Some(delta),
            Some(existing) if existing != delta => return None,
            Some(_) => {}
        }
    }
    offset.or(Some(0))
}

fn validate_neighbors(
    previous: &str,
    map: &HashMap<usize, usize>,
    anchors: &[usize],
) -> bool {
    let old: Vec<&str> = previous.split('\n').collect();
    let anchor_set: std::collections::HashSet<usize> = anchors.iter().copied().collect();
    for &line in anchors {
        let mapped = match map.get(&line) {
            Some(v) => *v,
            None => return false,
        };
        let mut left = line.saturating_sub(1);
        while left >= 1 && anchor_set.contains(&left) {
            left -= 1;
        }
        let mut right = line + 1;
        while anchor_set.contains(&right) {
            right += 1;
        }
        if left >= 1 {
            if let Some(&left_mapped) = map.get(&left) {
                if (mapped as i64 - left_mapped as i64) != (line as i64 - left as i64) {
                    let old_line = old.get(line - 1).copied().unwrap_or("");
                    let left_line = old.get(left - 1).copied().unwrap_or("");
                    if old_line == left_line {
                        return false;
                    }
                }
            }
        }
        if right <= old.len() {
            if let Some(&right_mapped) = map.get(&right) {
                if (right_mapped as i64 - mapped as i64) != (right as i64 - line as i64) {
                    let old_line = old.get(line - 1).copied().unwrap_or("");
                    let right_line = old.get(right - 1).copied().unwrap_or("");
                    if old_line == right_line {
                        return false;
                    }
                }
            }
        }
    }
    true
}

pub fn try_recover(
    store: &SnapshotStore,
    path: &str,
    current_text: &str,
    file_hash: &str,
    edits: &[Edit],
    clipboard: &mut Clipboard,
) -> Option<RecoveryResult> {
    let snapshot = store.by_hash(path, file_hash)?;
    let previous = snapshot.text.clone();
    if previous == current_text {
        return None;
    }
    let map = build_line_map(&previous, current_text);
    let anchors = collect_anchor_lines(edits);
    if uniform_offsets(&anchors, &map).is_none() {
        return None;
    }
    if !validate_neighbors(&previous, &map, &anchors) {
        return None;
    }
    let remapped: Vec<Edit> = edits
        .iter()
        .map(|edit| remap_edit(edit, &map))
        .collect::<Option<_>>()?;
    let apply = apply_edits(current_text, &remapped, Some(clipboard)).ok()?;
    if apply.text == current_text {
        return None;
    }
    let warning = if store.head(path).is_some_and(|head| head.text == previous) {
        format!("stale tag {file_hash}: file drifted outside this session; remapped anchors")
    } else {
        format!("stale tag {file_hash}: remapped anchors from a prior in-session edit")
    };
    Some(RecoveryResult { apply, warning })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_patch;
    use crate::snapshots::SnapshotStore;

    fn store_with(key: &str, text: &str) -> (SnapshotStore, String) {
        let mut store = SnapshotStore::new();
        let tag = store.record(key, text, None::<[usize; 0]>);
        (store, tag)
    }

    #[test]
    fn recovers_uniform_offset_drift() {
        let prev = "alpha\nbeta\ngamma\n";
        let (store, tag) = store_with("/recovers.rs", prev);
        // One line inserted at the head outside this session: every old line
        // remaps with the same +1 offset.
        let current = format!("header\n{prev}");
        let edits = parse_patch("PUT 2.=2:\n+BETA").unwrap().edits;
        let mut clip = Clipboard::default();
        let recovered = try_recover(&store, "/recovers.rs", &current, &tag, &edits, &mut clip)
            .expect("uniform drift should recover");
        assert_eq!(recovered.apply.text, "header\nalpha\nBETA\ngamma\n");
    }

    #[test]
    fn rejects_interior_line_drift_inside_a_cut_range() {
        let prev = "a\nb\nc\nd\ne\n";
        let (store, tag) = store_with("/interior.rs", prev);
        // Head insert (+1 for every endpoint) but interior line `c` was
        // replaced, so it has no mapping. Endpoint-only anchors would see a
        // uniform offset and wrongly recover; every covered line must anchor.
        let current = "x\na\nb\nQ\nd\ne\n";
        let edits = parse_patch("CUT 2.=4").unwrap().edits;
        let mut clip = Clipboard::default();
        assert!(
            try_recover(&store, "/interior.rs", &current, &tag, &edits, &mut clip).is_none(),
            "interior drift inside CUT must not recover"
        );
    }
}
