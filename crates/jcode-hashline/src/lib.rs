//! Line-anchored hashline patches for jcode.
//!
//! Tags are 4-hex xxHash32 fingerprints of whole-file text. The model points
//! at `[path#TAG]` plus original line numbers instead of restating `old_string`.
//! A stale tag is rejected before the file is written.

mod apply;
mod block;
mod clipboard;
mod format;
mod normalize;
mod parse;
mod patcher;
mod recovery;
mod session;
mod snapshots;
mod types;

pub use apply::{apply_edits, ApplyError};
pub use block::{resolve_block, resolve_block_edits};
pub use format::{
    compute_file_hash, format_hashline_header, format_numbered_line, format_numbered_lines,
    parse_hashline_header, split_addressable_file_lines,
};
pub use normalize::{
    detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom, LineEnding,
};
pub use parse::{parse_input, parse_lid, parse_patch, parse_patch_streaming, ParseError};
pub use patcher::{apply_patch_to_disk, display_path, resolve_existing, PatchError, SectionResult};
pub use session::{clear_session, with_session};
pub use snapshots::SnapshotStore;
pub use types::{
    ApplyResult, BlockResolution, Clipboard, Edit, FileOp, Patch, PatchSection, Snapshot,
    SNAPSHOT_MAX_BYTES,
};

/// Record a read/write snapshot and return the tag, or `None` when the file is over cap.
pub fn record_snapshot(
    session_id: &str,
    path: &str,
    text: &str,
    seen_lines: Option<impl IntoIterator<Item = usize>>,
) -> Option<String> {
    if text.len() > SNAPSHOT_MAX_BYTES {
        return None;
    }
    let normalized = normalize_to_lf(strip_bom(text).1);
    Some(with_session(session_id, |store, _| {
        store.record(path, &normalized, seen_lines)
    }))
}

/// Strip copied `[path#TAG]` headers and `N:` prefixes from write content.
pub fn strip_write_content(content: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.first().is_some_and(|line| parse_hashline_header(line).is_some()) {
        lines.remove(0);
    }
    let mut all_prefixed = true;
    let mut saw = false;
    for line in &lines {
        if line.is_empty() {
            continue;
        }
        saw = true;
        if !looks_like_numbered_line(line) {
            all_prefixed = false;
            break;
        }
    }
    if saw && all_prefixed {
        lines
            .into_iter()
            .map(|line| {
                if line.is_empty() {
                    line.to_string()
                } else {
                    strip_numbered_prefix(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        lines.join("\n")
    }
}

fn looks_like_numbered_line(line: &str) -> bool {
    let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && line.as_bytes().get(digits) == Some(&b':')
}

fn strip_numbered_prefix(line: &str) -> String {
    let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && line.as_bytes().get(digits) == Some(&b':') {
        line[digits + 1..].to_string()
    } else {
        line.to_string()
    }
}

pub const HASHLINE_TOOL_PROMPT: &str = r#"Line-anchored patch language. Section `[PATH#TAG]`; TAG is the 4-hex snapshot from the latest read/write/edit. New files use write.

PUT N.=M: replace original inclusive lines N-M with +body rows.
PUT N*: replace the syntactic block beginning on line N.
PUT <N: insert before line N (PUT <1: = file head).
PUT >N: insert after line N. PUT >$: file tail.
CUT N.=M / CUT N*: delete and capture. REM deletes the file. MV DEST moves it.
Body rows are final content and start with +. Markdown bullet: +- item. Do not paste N:text read lines as body.

Numbers and TAG are from the latest read. Each edit remints TAG. Stale tag: stop and re-read.
"#;
