//! Apply parsed edits to a text body.

use crate::clipboard::resolve_clipboard_edits;
use crate::types::{ApplyResult, Clipboard, Cursor, Edit};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0}")]
    Message(String),
}

fn trailing_phantom_line(file_lines: &[String]) -> usize {
    if file_lines.len() > 1 && file_lines.last().is_some_and(|l| l.is_empty()) {
        file_lines.len()
    } else {
        0
    }
}

fn drop_trailing_phantom_deletes(edits: Vec<Edit>, file_lines: &[String]) -> Vec<Edit> {
    let phantom = trailing_phantom_line(file_lines);
    if phantom == 0 {
        return edits;
    }
    edits
        .into_iter()
        .filter(|edit| match edit {
            Edit::Delete { anchor, .. } => anchor.line != phantom,
            _ => true,
        })
        .collect()
}

fn validate_line_bounds(edits: &[Edit], file_lines: &[String]) -> Result<(), ApplyError> {
    for edit in edits {
        for anchor in edit.anchors() {
            if anchor.line < 1 || anchor.line > file_lines.len() {
                return Err(ApplyError::Message(format!(
                    "Line {} does not exist (file has {} lines)",
                    anchor.line,
                    file_lines.len()
                )));
            }
        }
    }
    Ok(())
}

fn bucket_line(edit: &Edit) -> Option<usize> {
    match edit {
        Edit::Insert { cursor, .. } => match cursor {
            Cursor::BeforeAnchor { anchor } | Cursor::AfterAnchor { anchor } => Some(anchor.line),
            Cursor::Bof | Cursor::Eof => None,
        },
        Edit::Delete { anchor, .. } => Some(anchor.line),
        _ => None,
    }
}

fn materialize(original_lines: &[String], edits: &[Edit]) -> ApplyResult {
    let mut file_lines = original_lines.to_vec();
    let mut first_changed = None;
    let mut track = |line: usize| {
        if first_changed.is_none_or(|cur| line < cur) {
            first_changed = Some(line);
        }
    };

    let mut bof = Vec::new();
    let mut eof = Vec::new();
    let mut anchored = Vec::new();
    for (idx, edit) in edits.iter().enumerate() {
        match edit {
            Edit::Insert {
                cursor: Cursor::Bof,
                text,
                ..
            } => bof.push(text.clone()),
            Edit::Insert {
                cursor: Cursor::Eof,
                text,
                ..
            } => eof.push(text.clone()),
            _ => anchored.push((idx, edit)),
        }
    }

    let mut by_line: std::collections::BTreeMap<usize, Vec<(usize, &Edit)>> =
        std::collections::BTreeMap::new();
    for (idx, edit) in anchored {
        if let Some(line) = bucket_line(edit) {
            by_line.entry(line).or_default().push((idx, edit));
        }
    }

    for (line, mut bucket) in by_line.into_iter().rev() {
        bucket.sort_by_key(|(idx, _)| *idx);
        let idx = line.saturating_sub(1);
        let current = file_lines.get(idx).cloned().unwrap_or_default();
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut replacement = Vec::new();
        let mut delete_line = false;
        for (_, edit) in bucket {
            match edit {
                Edit::Insert {
                    replacement: true,
                    text,
                    ..
                } => replacement.push(text.clone()),
                Edit::Insert {
                    cursor: Cursor::AfterAnchor { .. },
                    text,
                    ..
                } => after.push(text.clone()),
                Edit::Insert { text, .. } => before.push(text.clone()),
                Edit::Delete { .. } => delete_line = true,
                _ => {}
            }
        }
        if before.is_empty() && replacement.is_empty() && after.is_empty() && !delete_line {
            continue;
        }
        let mut next = before;
        next.extend(replacement);
        if !delete_line {
            next.push(current);
        }
        next.extend(after);
        if idx < file_lines.len() {
            file_lines.splice(idx..=idx, next);
        } else {
            file_lines.extend(next);
        }
        track(line);
    }

    if !bof.is_empty() {
        let mut next = bof;
        next.append(&mut file_lines);
        file_lines = next;
        track(1);
    }
    if !eof.is_empty() {
        let changed = file_lines.len() + 1;
        file_lines.extend(eof);
        track(changed);
    }

    ApplyResult {
        text: file_lines.join("\n"),
        first_changed_line: first_changed,
        warnings: Vec::new(),
        block_resolutions: Vec::new(),
    }
}

pub fn apply_edits(
    text: &str,
    edits: &[Edit],
    clipboard: Option<&mut Clipboard>,
) -> Result<ApplyResult, ApplyError> {
    if edits.is_empty() {
        return Ok(ApplyResult {
            text: text.to_string(),
            ..ApplyResult::default()
        });
    }
    let file_lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut scratch = Clipboard::default();
    let clip = match clipboard {
        Some(c) => c,
        None => &mut scratch,
    };
    let mut warnings = Vec::new();
    let concrete = resolve_clipboard_edits(edits, &file_lines, clip, false, &mut warnings)
        .map_err(|e| ApplyError::Message(e.to_string()))?;
    for edit in &concrete {
        if matches!(edit, Edit::Block { .. }) {
            return Err(ApplyError::Message(
                "unresolved block edit reached the applier".into(),
            ));
        }
        if matches!(edit, Edit::Cut { .. } | Edit::Paste { .. }) {
            return Err(ApplyError::Message(
                "unresolved clipboard edit reached the applier".into(),
            ));
        }
    }
    let target = drop_trailing_phantom_deletes(concrete, &file_lines);
    validate_line_bounds(&target, &file_lines)?;
    let mut result = materialize(&file_lines, &target);
    result.warnings = warnings;
    Ok(result)
}


