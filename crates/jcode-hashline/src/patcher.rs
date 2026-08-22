//! Filesystem-backed patch orchestrator.

use std::path::{Path, PathBuf};

use crate::apply::apply_edits;
use crate::block::{has_block_edit, resolve_block_edits};
use crate::clipboard::{
    commit_clipboard, start_clipboard_batch, validate_clipboard_sequence,
};
use crate::format::{compute_file_hash, format_hashline_header};
use crate::normalize::{detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom, LineEnding};
use crate::parse::{collect_anchor_lines, parse_patch};
use crate::recovery::try_recover;
use crate::snapshots::SnapshotStore;
use crate::types::{
    ApplyResult, Clipboard, Edit, FileOp, Patch, PatchSection, SEEN_LINE_REVEAL_CAP,
    SEEN_LINE_REVEAL_MAX_COLUMNS,
};

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("{0}")]
    Message(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct SectionResult {
    pub path: String,
    pub op: &'static str,
    pub before: String,
    pub after: String,
    pub file_hash: String,
    pub header: String,
    pub first_changed_line: Option<usize>,
    pub warnings: Vec<String>,
    pub block_resolutions: Vec<crate::types::BlockResolution>,
    pub move_dest: Option<String>,
}

struct Prepared {
    section: PatchSection,
    canonical: PathBuf,
    exists: bool,
    _raw: String,
    bom: String,
    line_ending: LineEnding,
    normalized: String,
    apply: ApplyResult,
    parse_warnings: Vec<String>,
    file_op: Option<FileOp>,
}

pub fn apply_patch_to_disk(
    patch: &Patch,
    store: &mut SnapshotStore,
    clipboard: &mut Clipboard,
    cwd: &Path,
    enforce_seen_lines: bool,
) -> Result<Vec<SectionResult>, PatchError> {
    let mut batch = start_clipboard_batch(Some(clipboard));
    let mut prepared = Vec::new();
    for section in &patch.sections {
        prepared.push(prepare(
            section,
            store,
            &mut batch,
            cwd,
            enforce_seen_lines,
        )?);
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &prepared {
        let key = entry.canonical.clone();
        if !seen.insert(key) {
            return Err(PatchError::Message(format!(
                "duplicate section for {}",
                entry.section.path
            )));
        }
        if prepared.len() > 1 && entry.apply.text == entry.normalized && entry.file_op.is_none()
        {
            return Err(PatchError::Message(format!(
                "Edits to {} resulted in no changes being made.",
                entry.section.path
            )));
        }
    }
    let mut results = Vec::new();
    for entry in prepared {
        results.push(commit(entry, store, cwd)?);
    }
    commit_clipboard(&batch, clipboard);
    Ok(results)
}

fn prepare(
    section: &PatchSection,
    store: &mut SnapshotStore,
    clipboard: &mut Clipboard,
    cwd: &Path,
    enforce_seen_lines: bool,
) -> Result<Prepared, PatchError> {
    if section.path.is_empty() || section.file_hash.is_none() {
        return Err(PatchError::Message(
            "each hashline section needs `[PATH#TAG]` from the latest read/write/edit".into(),
        ));
    }
    let parsed = parse_patch(&section.body).map_err(|e| PatchError::Message(e.to_string()))?;
    let mut path = resolve_existing(cwd, &section.path);
    let exists = path.exists();
    if !exists {
        return Err(PatchError::Message(format!(
            "File not found: {}. Use the write tool to create new files.",
            section.path
        )));
    }
    if let Ok(canonical) = path.canonicalize() {
        path = canonical;
    }
    let raw = std::fs::read_to_string(&path)?;
    let (bom, text) = strip_bom(&raw);
    let bom = bom.to_string();
    let line_ending = detect_line_ending(text);
    let normalized = normalize_to_lf(text);
    if let FileOp::Move { dest } = &parsed.file_op.clone().unwrap_or(FileOp::Rem) {
        if dest.is_empty() {
            return Err(PatchError::Message("MV destination is empty".into()));
        }
        if resolve_existing(cwd, dest) == path {
            return Err(PatchError::Message(format!(
                "MV destination is the same as {}.",
                section.path
            )));
        }
    }
    let apply = apply_with_recovery(
        section,
        &path,
        &normalized,
        &parsed.edits,
        parsed.file_op.as_ref(),
        store,
        clipboard,
        enforce_seen_lines,
    )?;
    Ok(Prepared {
        section: section.clone(),
        canonical: path,
        exists,
        _raw: raw,
        bom: bom.to_string(),
        line_ending,
        normalized,
        apply,
        parse_warnings: parsed.warnings,
        file_op: parsed.file_op,
    })
}

fn apply_with_recovery(
    section: &PatchSection,
    canonical: &Path,
    normalized: &str,
    edits: &[Edit],
    file_op: Option<&FileOp>,
    store: &mut SnapshotStore,
    clipboard: &mut Clipboard,
    enforce_seen_lines: bool,
) -> Result<ApplyResult, PatchError> {
    let key = canonical.to_string_lossy().into_owned();
    let expected = section.file_hash.clone().unwrap_or_default();
    let live_matches = compute_file_hash(normalized) == expected;
    let mut resolve_warnings = Vec::new();
    let mut block_resolutions = Vec::new();
    let resolved = if has_block_edit(edits) {
        let base = if live_matches {
            normalized.to_string()
        } else {
            store
                .by_hash(&key, &expected)
                .map(|s| s.text.clone())
                .ok_or_else(|| mismatch(section, store, &key, normalized, &expected, false))?
        };
        resolve_block_edits(
            edits,
            &base,
            &section.path,
            true,
            &mut block_resolutions,
            &mut resolve_warnings,
        )
        .map_err(|e| PatchError::Message(e.to_string()))?
    } else {
        edits.to_vec()
    };
    validate_clipboard_sequence(&resolved, clipboard)
        .map_err(|e| PatchError::Message(e.to_string()))?;
    if matches!(file_op, Some(FileOp::Rem)) {
        let mut result = ApplyResult {
            text: normalized.to_string(),
            ..ApplyResult::default()
        };
        result.warnings.extend(resolve_warnings);
        return Ok(result);
    }
    if live_matches {
        if enforce_seen_lines {
            assert_seen_lines(section, store, &key, normalized, &expected, &resolved)?;
        }
        let mut result = apply_edits(normalized, &resolved, Some(clipboard))
            .map_err(|e| PatchError::Message(e.to_string()))?;
        result.warnings.splice(0..0, resolve_warnings);
        result.block_resolutions = block_resolutions;
        return Ok(result);
    }
    if !resolved.iter().any(Edit::is_anchor_scoped) {
        let mut result = apply_edits(normalized, &resolved, Some(clipboard))
            .map_err(|e| PatchError::Message(e.to_string()))?;
        result.warnings.insert(
            0,
            "stale tag on a head/tail-only insert; applied onto live content".into(),
        );
        result.warnings.splice(1..1, resolve_warnings);
        return Ok(result);
    }
    if let Some(recovered) = try_recover(store, &key, normalized, &expected, &resolved, clipboard)
    {
        let mut result = recovered.apply;
        result.warnings.insert(0, recovered.warning);
        result.warnings.splice(1..1, resolve_warnings);
        return Ok(result);
    }
    let recognized = store.by_hash(&key, &expected).is_some();
    Err(mismatch(
        section,
        store,
        &key,
        normalized,
        &expected,
        recognized,
    ))
}

fn assert_seen_lines(
    section: &PatchSection,
    store: &mut SnapshotStore,
    key: &str,
    normalized: &str,
    expected: &str,
    edits: &[Edit],
) -> Result<(), PatchError> {
    let Some(snapshot) = store.by_content(key, normalized).cloned() else {
        return Ok(());
    };
    let Some(seen) = snapshot.seen_lines.clone() else {
        return Ok(());
    };
    if seen.is_empty() {
        return Ok(());
    }
    let unseen: Vec<usize> = collect_anchor_lines(edits)
        .into_iter()
        .filter(|line| !seen.contains(line))
        .collect();
    if unseen.is_empty() {
        return Ok(());
    }
    let source: Vec<&str> = snapshot.text.split('\n').collect();
    let mut revealed = Vec::new();
    let mut column_truncated = false;
    for line in unseen.iter().take(SEEN_LINE_REVEAL_CAP) {
        if *line < 1 || *line > source.len() {
            continue;
        }
        let text = source[*line - 1];
        if text.len() > SEEN_LINE_REVEAL_MAX_COLUMNS {
            revealed.push(format!(
                "{}:{}…",
                line,
                &text[..SEEN_LINE_REVEAL_MAX_COLUMNS]
            ));
            column_truncated = true;
        } else {
            revealed.push(format!("{line}:{text}"));
        }
    }
    let truncated = unseen.len() > revealed.len() || column_truncated;
    if !truncated {
        if let Some(live) = store.by_content_mut(key, normalized) {
            let seen = live.seen_lines.get_or_insert_with(Default::default);
            for line in &unseen {
                seen.insert(*line);
            }
        }
    }
    let _ = expected;
    Err(PatchError::Message(format!(
        "edit on {} touches lines not shown in the tagged read ({}): {}\nRe-read those lines, then retry.{}",
        section.path,
        expected,
        unseen
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        if revealed.is_empty() {
            String::new()
        } else {
            format!("\n{}", revealed.join("\n"))
        }
    )))
}

fn mismatch(
    section: &PatchSection,
    store: &mut SnapshotStore,
    key: &str,
    normalized: &str,
    expected: &str,
    recognized: bool,
) -> PatchError {
    let actual = store.record(key, normalized, None::<[usize; 0]>);
    let why = if recognized {
        "file changed since that snapshot"
    } else {
        "hash is not from this session"
    };
    PatchError::Message(format!(
        "stale hashline tag for {}: expected {expected}, live file is {actual} ({why}). Re-read, then retry.",
        section.path
    ))
}

fn commit(
    prepared: Prepared,
    store: &mut SnapshotStore,
    cwd: &Path,
) -> Result<SectionResult, PatchError> {
    let key = prepared.canonical.to_string_lossy().into_owned();
    let mut warnings = prepared.parse_warnings;
    warnings.extend(prepared.apply.warnings.clone());
    if matches!(prepared.file_op, Some(FileOp::Rem)) {
        std::fs::remove_file(&prepared.canonical)?;
        store.invalidate(&key);
        let hash = compute_file_hash(&prepared.normalized);
        return Ok(SectionResult {
            path: prepared.section.path.clone(),
            op: "delete",
            before: prepared.normalized.clone(),
            after: prepared.normalized,
            file_hash: hash.clone(),
            header: format_hashline_header(&prepared.section.path, &hash),
            first_changed_line: None,
            warnings,
            block_resolutions: Vec::new(),
            move_dest: None,
        });
    }
    let after = prepared.apply.text;
    if after == prepared.normalized && !matches!(prepared.file_op, Some(FileOp::Move { .. })) {
        let hash = store.record(&key, &prepared.normalized, None::<[usize; 0]>);
        return Ok(SectionResult {
            path: prepared.section.path.clone(),
            op: "noop",
            before: prepared.normalized.clone(),
            after: prepared.normalized,
            file_hash: hash.clone(),
            header: format_hashline_header(&prepared.section.path, &hash),
            first_changed_line: None,
            warnings,
            block_resolutions: prepared.apply.block_resolutions,
            move_dest: None,
        });
    }
    let persisted = format!(
        "{}{}",
        prepared.bom,
        restore_line_endings(&after, prepared.line_ending)
    );
    if let Some(FileOp::Move { dest }) = &prepared.file_op {
        let dest_path = resolve_existing(cwd, dest);
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest_path, &persisted)?;
        std::fs::remove_file(&prepared.canonical)?;
        let dest_key = dest_path.to_string_lossy().into_owned();
        store.relocate(&key, &dest_key);
        let hash = store.record(&dest_key, &after, None::<[usize; 0]>);
        return Ok(SectionResult {
            path: dest.clone(),
            op: "update",
            before: prepared.normalized,
            after,
            file_hash: hash.clone(),
            header: format_hashline_header(dest, &hash),
            first_changed_line: prepared.apply.first_changed_line,
            warnings,
            block_resolutions: prepared.apply.block_resolutions,
            move_dest: Some(dest.clone()),
        });
    }
    std::fs::write(&prepared.canonical, &persisted)?;
    let recorded = normalize_to_lf(strip_bom(&persisted).1);
    if recorded != after {
        warnings.push(format!(
            "write of {} drifted from the in-memory result",
            prepared.section.path
        ));
    }
    let hash = store.record(&key, &recorded, None::<[usize; 0]>);
    Ok(SectionResult {
        path: prepared.section.path.clone(),
        op: if prepared.exists { "update" } else { "create" },
        before: prepared.normalized,
        after,
        file_hash: hash.clone(),
        header: format_hashline_header(&prepared.section.path, &hash),
        first_changed_line: prepared.apply.first_changed_line,
        warnings,
        block_resolutions: prepared.apply.block_resolutions,
        move_dest: None,
    })
}

pub fn resolve_existing(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

pub fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_input;

    #[test]
    fn applies_and_rejects_stale() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("greet.py");
        std::fs::write(&file, "def greet(name):\n    print(name)\n").unwrap();
        let mut store = SnapshotStore::new();
        let key = file.canonicalize().unwrap().to_string_lossy().into_owned();
        let tag = store.record(
            &key,
            "def greet(name):\n    print(name)\n",
            Some([1, 2]),
        );
        let input = format!(
            "[greet.py#{tag}]\nPUT 2.=2:\n+    print(f\"hi {{name}}\")"
        );
        let patch = parse_input(&input).unwrap();
        let mut clip = Clipboard::default();
        let results = apply_patch_to_disk(&patch, &mut store, &mut clip, dir.path(), true).unwrap();
        assert_eq!(results.len(), 1);
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("print(f\"hi {name}\")"));
        let stale = format!("[greet.py#{tag}]\nPUT 2.=2:\n+    pass");
        let patch = parse_input(&stale).unwrap();
        let err = apply_patch_to_disk(&patch, &mut store, &mut clip, dir.path(), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stale"), "{err}");
    }

    fn record_file(dir: &std::path::Path, name: &str, text: &str) -> (std::path::PathBuf, SnapshotStore, String) {
        let file = dir.join(name);
        std::fs::write(&file, text).unwrap();
        let mut store = SnapshotStore::new();
        let key = file.canonicalize().unwrap().to_string_lossy().into_owned();
        let tag = store.record(&key, text, None::<[usize; 0]>);
        (file, store, tag)
    }

    #[test]
    fn put_star_replaces_block() {
        let dir = tempfile::tempdir().unwrap();
        let text = "fn greet() {\n    println!(\"a\");\n}\nfn other() {}\n";
        let (file, mut store, tag) = record_file(dir.path(), "greet.rs", text);
        let input = format!("[greet.rs#{tag}]\nPUT 1*:\n+fn greet() {{\n+    println!(\"b\");\n+}}");
        let patch = parse_input(&input).unwrap();
        let mut clip = Clipboard::default();
        apply_patch_to_disk(&patch, &mut store, &mut clip, dir.path(), true).unwrap();
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("println!(\"b\")"), "{after}");
        assert!(after.contains("fn other()"), "{after}");
        assert!(!after.contains("println!(\"a\")"), "{after}");
    }

    #[test]
    fn rem_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let (file, mut store, tag) = record_file(dir.path(), "gone.txt", "bye\n");
        let input = format!("[gone.txt#{tag}]\nREM");
        let patch = parse_input(&input).unwrap();
        let mut clip = Clipboard::default();
        let results = apply_patch_to_disk(&patch, &mut store, &mut clip, dir.path(), true).unwrap();
        assert_eq!(results[0].op, "delete");
        assert!(!file.exists());
    }

    #[test]
    fn mv_renames_file() {
        let dir = tempfile::tempdir().unwrap();
        let (file, mut store, tag) = record_file(dir.path(), "src.txt", "keep\n");
        let dest = dir.path().join("dest.txt");
        let input = format!("[src.txt#{tag}]\nMV dest.txt");
        let patch = parse_input(&input).unwrap();
        let mut clip = Clipboard::default();
        let results = apply_patch_to_disk(&patch, &mut store, &mut clip, dir.path(), true).unwrap();
        assert_eq!(results[0].move_dest.as_deref(), Some("dest.txt"));
        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep\n");
    }

}
