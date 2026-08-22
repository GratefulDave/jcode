//! Clipboard registers for CUT / register PUT.

use crate::types::{Clipboard, Cursor, Edit, PasteTarget};

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("{0}")]
    Message(String),
}

pub fn has_clipboard_edit(edits: &[Edit]) -> bool {
    edits
        .iter()
        .any(|e| matches!(e, Edit::Cut { .. } | Edit::Paste { .. }))
}

fn describe_cut(range: &crate::types::ParsedRange, register: Option<&str>) -> String {
    match register {
        Some(name) => format!("CUT {}=.={} @{name}", range.start.line, range.end.line),
        None => format!("CUT {}=.={}", range.start.line, range.end.line),
    }
}

fn read_register(
    register: Option<&str>,
    target: &str,
    clipboard: &mut Clipboard,
    line_num: usize,
    on_empty_drop: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<Vec<String>>, ClipboardError> {
    if let Some(name) = register {
        if let Some(lines) = clipboard.named.get(name) {
            return Ok(Some(lines.clone()));
        }
        if on_empty_drop {
            return Ok(None);
        }
        if target == "span" {
            return Err(ClipboardError::Message(format!(
                "line {line_num}: named register @{name} is empty; cannot paste over a span"
            )));
        }
        warnings.push(format!(
            "line {line_num}: named register @{name} is empty; pasting nothing"
        ));
        return Ok(Some(Vec::new()));
    }
    if clipboard.pending_anon_cuts.len() > 1 {
        if on_empty_drop {
            return Ok(None);
        }
        return Err(ClipboardError::Message(format!(
            "line {line_num}: ambiguous unlabeled paste; {} pending anonymous CUTs",
            clipboard.pending_anon_cuts.len()
        )));
    }
    match &clipboard.lines {
        Some(lines) => {
            clipboard.pending_anon_cuts.clear();
            Ok(Some(lines.clone()))
        }
        None if on_empty_drop => Ok(None),
        None => Err(ClipboardError::Message(format!(
            "line {line_num}: unlabeled paste with empty anonymous register"
        ))),
    }
}

fn write_register(
    range: &crate::types::ParsedRange,
    register: Option<&str>,
    line_num: usize,
    file_lines: &[String],
    clipboard: &mut Clipboard,
) -> Result<(), ClipboardError> {
    if range.start.line < 1 || range.end.line > file_lines.len() {
        return Err(ClipboardError::Message(format!(
            "line {line_num}: `{}` is out of range (file has {} lines).",
            describe_cut(range, register),
            file_lines.len()
        )));
    }
    let captured = file_lines[range.start.line - 1..range.end.line].to_vec();
    if let Some(name) = register {
        clipboard.named.insert(name.to_string(), captured);
    } else {
        clipboard.lines = Some(captured);
        clipboard
            .pending_anon_cuts
            .push(describe_cut(range, None));
    }
    Ok(())
}

pub fn resolve_clipboard_edits(
    edits: &[Edit],
    file_lines: &[String],
    clipboard: &mut Clipboard,
    on_empty_drop: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<Edit>, ClipboardError> {
    if !has_clipboard_edit(edits) {
        return Ok(edits.to_vec());
    }
    let mut resolved = Vec::new();
    let mut synth = 0usize;
    for edit in edits {
        match edit {
            Edit::Cut {
                range,
                register,
                line_num,
                ..
            } => {
                write_register(range, register.as_deref(), *line_num, file_lines, clipboard)?;
            }
            Edit::Paste {
                at,
                register,
                line_num,
                block_start,
                ..
            } => {
                let kind = match at {
                    PasteTarget::Gap { .. } => "gap",
                    PasteTarget::Span { .. } => "span",
                };
                let Some(lines) = read_register(
                    register.as_deref(),
                    kind,
                    clipboard,
                    *line_num,
                    on_empty_drop,
                    warnings,
                )?
                else {
                    continue;
                };
                match at {
                    PasteTarget::Gap { cursor } => {
                        for text in lines {
                            resolved.push(Edit::Insert {
                                cursor: cursor.clone(),
                                text,
                                line_num: *line_num,
                                index: synth,
                                replacement: false,
                                block_start: *block_start,
                            });
                            synth += 1;
                        }
                    }
                    PasteTarget::Span { range } => {
                        if range.start.line < 1 || range.end.line > file_lines.len() {
                            return Err(ClipboardError::Message(format!(
                                "line {line_num}: span paste is out of range (file has {} lines).",
                                file_lines.len()
                            )));
                        }
                        let cursor = Cursor::BeforeAnchor {
                            anchor: range.start,
                        };
                        for text in lines {
                            resolved.push(Edit::Insert {
                                cursor: cursor.clone(),
                                text,
                                line_num: *line_num,
                                index: synth,
                                replacement: true,
                                block_start: None,
                            });
                            synth += 1;
                        }
                        for line in range.start.line..=range.end.line {
                            resolved.push(Edit::Delete {
                                anchor: crate::types::Anchor { line },
                                line_num: *line_num,
                                index: synth,
                            });
                            synth += 1;
                        }
                    }
                }
            }
            other => resolved.push(other.clone()),
        }
    }
    Ok(resolved)
}

pub fn start_clipboard_batch(source: Option<&Clipboard>) -> Clipboard {
    match source {
        Some(src) if !src.named.is_empty() => Clipboard {
            named: src.named.clone(),
            ..Clipboard::default()
        },
        _ => Clipboard::default(),
    }
}

pub fn fork_clipboard(source: Option<&Clipboard>) -> Clipboard {
    source.cloned().unwrap_or_default()
}

pub fn commit_clipboard(fork: &Clipboard, target: &mut Clipboard) {
    for (k, v) in &fork.named {
        target.named.insert(k.clone(), v.clone());
    }
}

pub fn validate_clipboard_sequence(
    edits: &[Edit],
    clipboard: &Clipboard,
) -> Result<(), ClipboardError> {
    let mut fork = fork_clipboard(Some(clipboard));
    let mut warnings = Vec::new();
    for edit in edits {
        match edit {
            Edit::Cut {
                range, register, ..
            } => {
                if let Some(name) = register {
                    fork.named.insert(name.clone(), Vec::new());
                } else {
                    fork.lines = Some(Vec::new());
                    fork.pending_anon_cuts.push(describe_cut(range, None));
                }
            }
            Edit::Paste {
                at,
                register,
                line_num,
                ..
            } => {
                let kind = match at {
                    PasteTarget::Gap { .. } => "gap",
                    PasteTarget::Span { .. } => "span",
                };
                read_register(
                    register.as_deref(),
                    kind,
                    &mut fork,
                    *line_num,
                    false,
                    &mut warnings,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}
