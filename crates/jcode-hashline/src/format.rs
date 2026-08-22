//! Hashline format primitives: sigils, hashing, and numbered-line display.

use xxhash_rust::xxh32::xxh32;

pub const HL_FILE_PREFIX: char = '[';
pub const HL_FILE_SUFFIX: char = ']';
pub const HL_FILE_HASH_SEP: char = '#';
pub const HL_RANGE_SEP: &str = ".=";
pub const HL_LINE_BODY_SEP: char = ':';
pub const HL_FILE_HASH_LENGTH: usize = 4;

/// Trim trailing space/tab/CR from every line so CRLF and display-trim
/// do not invalidate a tag.
pub fn normalize_file_hash_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t', '\r']));
    }
    out
}

/// 4-hex fingerprint: low 16 bits of xxHash32(seed 0) over normalized text.
pub fn compute_file_hash(text: &str) -> String {
    let normalized = normalize_file_hash_text(text);
    let low16 = xxh32(normalized.as_bytes(), 0) & 0xffff;
    format!("{low16:04X}")
}

pub fn format_hashline_header(file_path: &str, file_hash: &str) -> String {
    format!("{HL_FILE_PREFIX}{file_path}{HL_FILE_HASH_SEP}{file_hash}{HL_FILE_SUFFIX}")
}

pub fn format_numbered_line(line_number: usize, line: &str) -> String {
    format!("{line_number}{HL_LINE_BODY_SEP}{line}")
}

/// Split LF text into addressable lines. A terminal newline is not content.
pub fn split_addressable_file_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if lines.len() > 1 && lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

pub fn format_numbered_lines(text: &str, start_line: usize) -> String {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| format_numbered_line(start_line + i, line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_hashline_header(line: &str) -> Option<(String, Option<String>)> {
    let trimmed = line.trim();
    if !trimmed.starts_with(HL_FILE_PREFIX) || !trimmed.ends_with(HL_FILE_SUFFIX) {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    if let Some(hash_at) = inner.rfind(HL_FILE_HASH_SEP) {
        let path = inner[..hash_at].trim();
        let tag = inner[hash_at + 1..].trim();
        if path.is_empty() {
            return None;
        }
        if tag.len() == HL_FILE_HASH_LENGTH
            && tag.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Some((path.to_string(), Some(tag.to_ascii_uppercase())));
        }
        if tag.is_empty() {
            return Some((path.to_string(), None));
        }
    }
    if inner.is_empty() {
        None
    } else {
        Some((inner.to_string(), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collide_pair_shares_1d84() {
        let a = "line one 263\nline two 4471\n";
        let b = "line one 410\nline two 6970\n";
        assert_eq!(compute_file_hash(a), compute_file_hash(b));
        assert_eq!(compute_file_hash(a), "1D84");
    }

    #[test]
    fn trailing_ws_does_not_change_hash() {
        assert_eq!(
            compute_file_hash("hello  \nworld\t"),
            compute_file_hash("hello\nworld")
        );
    }

    #[test]
    fn addressable_lines_drop_terminal_newline() {
        assert_eq!(split_addressable_file_lines("a\nb\n"), ["a", "b"]);
        assert_eq!(split_addressable_file_lines("a\nb\n\n"), ["a", "b", ""]);
        assert_eq!(format_numbered_lines("a\n\nb\n", 1), "1:a\n2:\n3:b\n4:");
    }
}
