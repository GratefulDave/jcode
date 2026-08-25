//! Expand `N*` block ops using brace, indent, or markdown structure.

use crate::types::{
    Anchor, BlockMode, BlockResolution, BlockSpan, Cursor, Edit, ParsedRange, PasteTarget,
};

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("{0}")]
    Message(String),
}

pub fn has_block_edit(edits: &[Edit]) -> bool {
    edits.iter().any(|e| matches!(e, Edit::Block { .. }))
}

/// Resolve the outermost multi-line construct that begins on `line`.
pub fn resolve_block(path: &str, text: &str, line: usize) -> Option<BlockSpan> {
    let lines: Vec<&str> = text.split('\n').collect();
    if line == 0 || line > lines.len() {
        return None;
    }
    if looks_like_markdown(path) {
        if let Some(span) = markdown_section(&lines, line) {
            return Some(span);
        }
    }
    if let Some(span) = brace_block(&lines, line) {
        return Some(span);
    }
    indent_block(&lines, line)
}

fn looks_like_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".markdown")
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&level) && trimmed.as_bytes().get(level) == Some(&b' ') {
        Some(level)
    } else {
        None
    }
}

fn markdown_section(lines: &[&str], line: usize) -> Option<BlockSpan> {
    let level = heading_level(lines[line - 1])?;
    let mut end = line;
    for (idx, candidate) in lines.iter().enumerate().skip(line) {
        if let Some(next) = heading_level(candidate) {
            if next <= level {
                break;
            }
        }
        end = idx + 1;
    }
    if end == line {
        return None;
    }
    Some(BlockSpan { start: line, end })
}

fn opener_on_line(line: &str) -> Option<char> {
    let mut in_str = None;
    let mut escaped = false;
    let mut last = None;
    for c in line.chars() {
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' && q != '\'' {
                escaped = true;
            } else if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => in_str = Some(c),
            '{' | '(' | '[' => last = Some(c),
            _ => {}
        }
    }
    last
}

fn matching(open: char) -> char {
    match open {
        '{' => '}',
        '(' => ')',
        '[' => ']',
        _ => open,
    }
}

fn brace_block(lines: &[&str], line: usize) -> Option<BlockSpan> {
    let open = opener_on_line(lines[line - 1])?;
    let close = matching(open);
    let mut depth = 0i32;
    let mut started = false;
    for (idx, candidate) in lines.iter().enumerate().skip(line - 1) {
        let mut in_str = None;
        let mut escaped = false;
        for c in candidate.chars() {
            if let Some(q) = in_str {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q != '\'' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
                continue;
            }
            match c {
                '"' | '\'' | '`' => in_str = Some(c),
                c if c == open => {
                    depth += 1;
                    started = true;
                }
                c if c == close && started => {
                    depth -= 1;
                    if depth == 0 {
                        let end = idx + 1;
                        if end == line {
                            return None;
                        }
                        return Some(BlockSpan { start: line, end });
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn indent_width(line: &str) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    Some(
        line.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum(),
    )
}

fn indent_block(lines: &[&str], line: usize) -> Option<BlockSpan> {
    let opener = lines[line - 1];
    if !opener.trim_end().ends_with(':') && !opener.trim_end().ends_with('{') {
        return None;
    }
    let base = indent_width(opener).unwrap_or(0);
    let mut end = line;
    for (idx, candidate) in lines.iter().enumerate().skip(line) {
        if candidate.trim().is_empty() {
            end = idx + 1;
            continue;
        }
        let width = indent_width(candidate)?;
        if width <= base {
            break;
        }
        end = idx + 1;
    }
    if end == line {
        return None;
    }
    Some(BlockSpan { start: line, end })
}

pub fn resolve_block_edits(
    edits: &[Edit],
    text: &str,
    path: &str,
    on_unresolved_throw: bool,
    resolutions: &mut Vec<BlockResolution>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Edit>, BlockError> {
    if !has_block_edit(edits) {
        return Ok(edits.to_vec());
    }
    let mut out = Vec::new();
    let mut index = 0usize;
    for edit in edits {
        let Edit::Block {
            anchor,
            payloads,
            mode,
            register,
            line_num,
            ..
        } = edit
        else {
            out.push(edit.clone());
            continue;
        };
        let Some(span) = resolve_block(path, text, anchor.line) else {
            if *mode == BlockMode::InsertAfter || *mode == BlockMode::PasteAfter {
                warnings.push(format!(
                    "PUT >{}*: could not resolve block; lowered to PUT >{}",
                    anchor.line, anchor.line
                ));
                match mode {
                    BlockMode::InsertAfter => {
                        for text in payloads {
                            out.push(Edit::Insert {
                                cursor: Cursor::AfterAnchor { anchor: *anchor },
                                text: text.clone(),
                                line_num: *line_num,
                                index,
                                replacement: false,
                                block_start: None,
                            });
                            index += 1;
                        }
                    }
                    BlockMode::PasteAfter => {
                        out.push(Edit::Paste {
                            at: PasteTarget::Gap {
                                cursor: Cursor::AfterAnchor { anchor: *anchor },
                            },
                            register: register.clone(),
                            line_num: *line_num,
                            index,
                            block_start: None,
                        });
                        index += 1;
                    }
                    _ => {}
                }
                continue;
            }
            if on_unresolved_throw {
                return Err(BlockError::Message(format!(
                    "line {line_num}: could not resolve block opening at {}",
                    anchor.line
                )));
            }
            continue;
        };
        if span.end == span.start {
            return Err(BlockError::Message(format!(
                "line {line_num}: `PUT {}*:` resolved to a single line; use `PUT {n}.={n}:`",
                anchor.line,
                n = anchor.line
            )));
        }
        resolutions.push(BlockResolution {
            line: anchor.line,
            start: span.start,
            end: span.end,
        });
        match mode {
            BlockMode::Replace => {
                if let Some(name) = register {
                    out.push(Edit::Paste {
                        at: PasteTarget::Span {
                            range: ParsedRange {
                                start: Anchor { line: span.start },
                                end: Anchor { line: span.end },
                            },
                        },
                        register: Some(name.clone()),
                        line_num: *line_num,
                        index,
                        block_start: None,
                    });
                    index += 1;
                } else {
                    for text in payloads {
                        out.push(Edit::Insert {
                            cursor: Cursor::BeforeAnchor {
                                anchor: Anchor { line: span.start },
                            },
                            text: text.clone(),
                            line_num: *line_num,
                            index,
                            replacement: true,
                            block_start: None,
                        });
                        index += 1;
                    }
                    for line in span.start..=span.end {
                        out.push(Edit::Delete {
                            anchor: Anchor { line },
                            line_num: *line_num,
                            index,
                        });
                        index += 1;
                    }
                }
            }
            BlockMode::Cut => {
                out.push(Edit::Cut {
                    range: ParsedRange {
                        start: Anchor { line: span.start },
                        end: Anchor { line: span.end },
                    },
                    register: register.clone(),
                    line_num: *line_num,
                    index,
                });
                index += 1;
                for line in span.start..=span.end {
                    out.push(Edit::Delete {
                        anchor: Anchor { line },
                        line_num: *line_num,
                        index,
                    });
                    index += 1;
                }
            }
            BlockMode::InsertAfter => {
                for text in payloads {
                    out.push(Edit::Insert {
                        cursor: Cursor::AfterAnchor {
                            anchor: Anchor { line: span.end },
                        },
                        text: text.clone(),
                        line_num: *line_num,
                        index,
                        replacement: false,
                        block_start: Some(span.start),
                    });
                    index += 1;
                }
            }
            BlockMode::PasteAfter => {
                out.push(Edit::Paste {
                    at: PasteTarget::Gap {
                        cursor: Cursor::AfterAnchor {
                            anchor: Anchor { line: span.end },
                        },
                    },
                    register: register.clone(),
                    line_num: *line_num,
                    index,
                    block_start: Some(span.start),
                });
                index += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_fn_block() {
        let text = "fn greet(name: &str) {\n    println!(\"{name}\");\n}\ncall();\n";
        assert_eq!(
            resolve_block("greet.rs", text, 1),
            Some(BlockSpan { start: 1, end: 3 })
        );
    }

    #[test]
    fn python_indent_block() {
        let text = "def greet(name):\n    msg = name\n    print(msg)\ngreet('x')\n";
        assert_eq!(
            resolve_block("greet.py", text, 1),
            Some(BlockSpan { start: 1, end: 3 })
        );
    }

    #[test]
    fn markdown_heading() {
        let text = "## A\nbody\n### nested\nmore\n## B\n";
        assert_eq!(
            resolve_block("doc.md", text, 1),
            Some(BlockSpan { start: 1, end: 4 })
        );
    }
}
