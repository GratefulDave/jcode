//! Line-oriented hashline tokenizer and parser.

use crate::format::{parse_hashline_header, HL_RANGE_SEP};
use crate::types::{
    Anchor, BlockMode, Cursor, Edit, FileOp, ParsedRange, PasteTarget, Patch, PatchSection,
    MAX_RANGE_LINES,
};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{0}")]
    Message(String),
}

type Result<T, E = ParseError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
enum Target {
    Replace {
        range: ParsedRange,
        register: Option<String>,
    },
    Block {
        anchor: Anchor,
        register: Option<String>,
    },
    InsertBefore {
        anchor: Anchor,
        register: Option<String>,
    },
    InsertAfter {
        anchor: Anchor,
        register: Option<String>,
    },
    InsertAfterBlock {
        anchor: Anchor,
        register: Option<String>,
    },
    Cut {
        range: ParsedRange,
        register: Option<String>,
    },
    CutBlock {
        anchor: Anchor,
        register: Option<String>,
    },
    Bof {
        register: Option<String>,
    },
    Eof {
        register: Option<String>,
    },
    Rem,
    Move {
        dest: String,
    },
}

struct Pending {
    target: Target,
    line_num: usize,
    payloads: Vec<PayloadRow>,
    had_colon: bool,
    deferred_blanks: Vec<PayloadRow>,
}

#[derive(Clone)]
struct PayloadRow {
    text: String,
    line_num: usize,
    bare: bool,
    minus: bool,
}

const EMPTY_PUT_AUTO_CUT_WARNING: &str =
    "empty `PUT` body as deletion; use `CUT` to delete lines";
const BARE_BODY_AUTO_PIPED_WARNING: &str = "Auto-prefixed bare body row";
const EMPTY_INSERT: &str = "`PUT` insert promises body rows";
const COLONLESS_SPAN_PUT: &str =
    "colonless span `PUT` needs a register (`PUT N.=M @name`) or a `:` body";
const CUT_TAKES_NO_BODY: &str = "`CUT` takes no body rows";
const REM_TAKES_NO_BODY: &str = "`REM` deletes the whole file and cannot be combined with line ops";
const COLON_ON_REGISTER_PUT: &str = "register `PUT` takes no `:` body";
const MINUS_ROW_REJECTED: &str =
    "body rows starting with `-` are not hashline; use `+` for final content";

pub fn parse_lid(raw: &str, line_num: usize) -> Result<Anchor> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Message(format!(
            "line {line_num}: invalid line number {raw:?}"
        )));
    }
    if trimmed.len() > 1 && trimmed.starts_with('0') {
        return Err(ParseError::Message(format!(
            "line {line_num}: invalid line number {raw:?}"
        )));
    }
    let line: u64 = trimmed.parse().map_err(|_| {
        ParseError::Message(format!("line {line_num}: invalid line number {raw:?}"))
    })?;
    if line == 0 || line > (i64::MAX as u64) {
        return Err(ParseError::Message(format!(
            "line {line_num}: invalid line number {raw:?}"
        )));
    }
    Ok(Anchor { line: line as usize })
}

fn validate_range(range: &ParsedRange, line_num: usize) -> Result<()> {
    if range.start.line == 0 || range.end.line == 0 {
        return Err(ParseError::Message(format!(
            "line {line_num}: line numbers start at 1"
        )));
    }
    if range.end.line < range.start.line {
        return Err(ParseError::Message(format!(
            "line {line_num}: inverted range {}{}{}",
            range.start.line, HL_RANGE_SEP, range.end.line
        )));
    }
    let span = range.end.line - range.start.line + 1;
    if span > MAX_RANGE_LINES {
        return Err(ParseError::Message(format!(
            "line {line_num}: replace range spans {span} lines; the maximum is {MAX_RANGE_LINES}"
        )));
    }
    Ok(())
}

fn is_sep_char(c: char) -> bool {
    matches!(c, '-' | '.' | '=' | ' ' | '\t' | '…')
}

fn scan_line_number(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let n: usize = s[..i].parse().ok()?;
    if n == 0 {
        return None;
    }
    Some((n, i))
}

fn skip_ws(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

fn scan_range(rest: &str, allow_single: bool) -> Option<(ParsedRange, bool, &str)> {
    let rest = skip_ws(rest);
    let (start, used) = scan_line_number(rest)?;
    let after = &rest[used..];
    let mut sep_len = 0;
    let mut saw_sep = false;
    for c in after.chars() {
        if is_sep_char(c) {
            sep_len += c.len_utf8();
            saw_sep = true;
        } else {
            break;
        }
    }
    if saw_sep {
        let after_sep = skip_ws(&after[sep_len..]);
        if let Some((end, used2)) = scan_line_number(after_sep) {
            return Some((
                ParsedRange {
                    start: Anchor { line: start },
                    end: Anchor { line: end },
                },
                true,
                skip_ws(&after_sep[used2..]),
            ));
        }
        if allow_single {
            let leftover = skip_ws(&after[sep_len..]);
            if leftover.is_empty() || leftover.starts_with(':') || leftover.starts_with('@') {
                return Some((
                    ParsedRange {
                        start: Anchor { line: start },
                        end: Anchor { line: start },
                    },
                    true,
                    leftover,
                ));
            }
        }
        return None;
    }
    if allow_single {
        return Some((
            ParsedRange {
                start: Anchor { line: start },
                end: Anchor { line: start },
            },
            false,
            skip_ws(after),
        ));
    }
    None
}

fn scan_register(rest: &str) -> Option<(String, &str)> {
    let rest = skip_ws(rest);
    let stripped = rest.strip_prefix('@')?;
    let end = stripped
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .unwrap_or(stripped.len());
    if end == 0 || end > 64 {
        return None;
    }
    Some((stripped[..end].to_string(), skip_ws(&stripped[end..])))
}

fn finish_target(rest: &str) -> (Option<String>, bool, &str) {
    let rest = skip_ws(rest);
    if let Some((name, after)) = scan_register(rest) {
        let after = skip_ws(after);
        if let Some(after_colon) = after.strip_prefix(':') {
            return (Some(name), true, skip_ws(after_colon));
        }
        return (Some(name), false, after);
    }
    if let Some(after) = rest.strip_prefix(':') {
        return (None, true, skip_ws(after));
    }
    (None, false, rest)
}

fn unquote_path(path: &str) -> String {
    let path = path.trim();
    if path.len() >= 2 {
        let bytes = path.as_bytes();
        if (bytes[0] == b'"' || bytes[0] == b'\'') && bytes[0] == bytes[bytes.len() - 1] {
            return path[1..path.len() - 1].to_string();
        }
    }
    path.to_string()
}

fn parse_put_target(locator: &str) -> Result<(Target, bool)> {
    let locator = skip_ws(locator);
    if locator.is_empty() {
        return Err(ParseError::Message("empty PUT locator".into()));
    }
    if locator.starts_with('<') || locator.starts_with('>') {
        let is_after = locator.starts_with('>');
        let after_sigil = skip_ws(&locator[1..]);
        if is_after && after_sigil.starts_with('$') {
            let (register, had_colon, leftover) = finish_target(&after_sigil[1..]);
            if !leftover.is_empty() {
                return Err(ParseError::Message(format!(
                    "trailing junk after PUT locator: {leftover:?}"
                )));
            }
            return Ok((Target::Eof { register }, had_colon));
        }
        let (n, used) = scan_line_number(after_sigil).ok_or_else(|| {
            ParseError::Message(format!("invalid PUT locator {locator:?}"))
        })?;
        let mut rest = &after_sigil[used..];
        let mut block = false;
        if rest.starts_with('*') {
            block = true;
            rest = &rest[1..];
        }
        let (register, had_colon, leftover) = finish_target(rest);
        if !leftover.is_empty() {
            return Err(ParseError::Message(format!(
                "trailing junk after PUT locator: {leftover:?}"
            )));
        }
        let target = if is_after {
            if block {
                Target::InsertAfterBlock {
                    anchor: Anchor { line: n },
                    register,
                }
            } else {
                Target::InsertAfter {
                    anchor: Anchor { line: n },
                    register,
                }
            }
        } else if n == 1 {
            Target::Bof { register }
        } else {
            Target::InsertBefore {
                anchor: Anchor { line: n },
                register,
            }
        };
        return Ok((target, had_colon));
    }
    let (range, had_sep, rest) = scan_range(locator, true)
        .ok_or_else(|| ParseError::Message(format!("invalid PUT locator {locator:?}")))?;
    if rest.starts_with('*') {
        if had_sep {
            return Err(ParseError::Message(
                "block locators are single lines (`N*`), never ranges".into(),
            ));
        }
        let (register, had_colon, leftover) = finish_target(&rest[1..]);
        if !leftover.is_empty() {
            return Err(ParseError::Message(format!(
                "trailing junk after PUT locator: {leftover:?}"
            )));
        }
        return Ok((
            Target::Block {
                anchor: range.start,
                register,
            },
            had_colon,
        ));
    }
    let (register, had_colon, leftover) = finish_target(rest);
    if !leftover.is_empty() {
        return Err(ParseError::Message(format!(
            "trailing junk after PUT locator: {leftover:?}"
        )));
    }
    Ok((Target::Replace { range, register }, had_colon))
}

fn parse_cut_target(locator: &str) -> Result<(Target, bool)> {
    let (range, had_sep, rest) = scan_range(locator, true)
        .ok_or_else(|| ParseError::Message(format!("invalid CUT locator {locator:?}")))?;
    if rest.starts_with('*') {
        if had_sep {
            return Err(ParseError::Message(
                "block locators are single lines (`N*`), never ranges".into(),
            ));
        }
        let (register, had_colon, leftover) = finish_target(&rest[1..]);
        if !leftover.is_empty() {
            return Err(ParseError::Message(format!(
                "trailing junk after CUT locator: {leftover:?}"
            )));
        }
        return Ok((
            Target::CutBlock {
                anchor: range.start,
                register,
            },
            had_colon,
        ));
    }
    let (register, had_colon, leftover) = finish_target(rest);
    if !leftover.is_empty() {
        return Err(ParseError::Message(format!(
            "trailing junk after CUT locator: {leftover:?}"
        )));
    }
    Ok((Target::Cut { range, register }, had_colon))
}

fn is_hunk_header_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("PUT ") || t.starts_with("CUT ") || t == "REM" || t.starts_with("MV ")
}

fn strip_one_leading_prefix(line: &str) -> String {
    let re = regex_lite_prefix(line);
    re.unwrap_or_else(|| line.to_string())
}

fn regex_lite_prefix(line: &str) -> Option<String> {
    let mut s = line.trim_start();
    for marker in [">>>", ">>"] {
        if let Some(rest) = s.strip_prefix(marker) {
            s = rest.trim_start();
        }
    }
    if let Some(rest) = s.strip_prefix(['+', '*', '-']) {
        if rest.starts_with(char::is_whitespace) {
            s = rest.trim_start();
        }
    }
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || s.as_bytes().first() == Some(&b'0') {
        return None;
    }
    let after = &s[digits..];
    if after.starts_with(':') || after.starts_with('|') {
        return Some(after[1..].to_string());
    }
    None
}

struct Executor {
    edits: Vec<Edit>,
    warnings: Vec<String>,
    edit_index: usize,
    pending: Option<Pending>,
    file_op: Option<FileOp>,
}

impl Executor {
    fn new() -> Self {
        Self {
            edits: Vec::new(),
            warnings: Vec::new(),
            edit_index: 0,
            pending: None,
            file_op: None,
        }
    }

    fn warn(&mut self, message: &str) {
        if !self.warnings.iter().any(|w| w == message) {
            self.warnings.push(message.to_string());
        }
    }

    fn push_insert(
        &mut self,
        cursor: Cursor,
        text: String,
        line_num: usize,
        replacement: bool,
    ) {
        self.edits.push(Edit::Insert {
            cursor,
            text,
            line_num,
            index: self.edit_index,
            replacement,
            block_start: None,
        });
        self.edit_index += 1;
    }

    fn push_delete(&mut self, line: usize, line_num: usize) {
        self.edits.push(Edit::Delete {
            anchor: Anchor { line },
            line_num,
            index: self.edit_index,
        });
        self.edit_index += 1;
    }

    fn push_delete_range(&mut self, range: &ParsedRange, line_num: usize) {
        for line in range.start.line..=range.end.line {
            self.push_delete(line, line_num);
        }
    }

    fn push_cut(&mut self, range: ParsedRange, line_num: usize, register: Option<String>) {
        self.edits.push(Edit::Cut {
            range: range.clone(),
            register,
            line_num,
            index: self.edit_index,
        });
        self.edit_index += 1;
        self.push_delete_range(&range, line_num);
    }

    fn push_paste(&mut self, at: PasteTarget, register: Option<String>, line_num: usize) {
        self.edits.push(Edit::Paste {
            at,
            register,
            line_num,
            index: self.edit_index,
            block_start: None,
        });
        self.edit_index += 1;
    }

    fn push_block(
        &mut self,
        anchor: Anchor,
        payloads: Vec<String>,
        line_num: usize,
        mode: BlockMode,
        register: Option<String>,
    ) {
        self.edits.push(Edit::Block {
            anchor,
            payloads,
            mode,
            register,
            line_num,
            index: self.edit_index,
        });
        self.edit_index += 1;
    }

    fn set_file_op(&mut self, op: FileOp, line_num: usize) -> Result<()> {
        if self.file_op.is_some() {
            return Err(ParseError::Message(format!(
                "line {line_num}: only one file-level op (`REM` or `MV`) per section"
            )));
        }
        if matches!(op, FileOp::Rem) && !self.edits.is_empty() {
            return Err(ParseError::Message(format!(
                "line {line_num}: {REM_TAKES_NO_BODY}"
            )));
        }
        self.file_op = Some(op);
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        resolve_minus_rows(&mut pending.payloads)?;
        strip_bare_prefixes_if_uniform(&mut pending.payloads);
        let payloads = pending.payloads;
        let line_num = pending.line_num;
        let had_colon = pending.had_colon;
        match pending.target {
            Target::Rem | Target::Move { .. } => Ok(()),
            Target::Cut { range, register } => {
                validate_range(&range, line_num)?;
                self.push_cut(range, line_num, register);
                Ok(())
            }
            Target::CutBlock { anchor, register } => {
                self.push_block(anchor, vec![], line_num, BlockMode::Cut, register);
                Ok(())
            }
            Target::Replace { range, register } => {
                validate_range(&range, line_num)?;
                if register.is_some() {
                    self.push_paste(
                        PasteTarget::Span { range },
                        register,
                        line_num,
                    );
                    return Ok(());
                }
                if payloads.is_empty() {
                    if !had_colon {
                        return Err(ParseError::Message(format!(
                            "line {line_num}: {COLONLESS_SPAN_PUT}"
                        )));
                    }
                    self.push_delete_range(&range, line_num);
                    self.warn(EMPTY_PUT_AUTO_CUT_WARNING);
                    return Ok(());
                }
                let cursor = Cursor::BeforeAnchor {
                    anchor: range.start,
                };
                for row in &payloads {
                    self.push_insert(cursor.clone(), row.text.clone(), line_num, true);
                }
                self.push_delete_range(&range, line_num);
                Ok(())
            }
            Target::Block { anchor, register } => {
                if register.is_some() {
                    self.push_block(anchor, vec![], line_num, BlockMode::Replace, register);
                    return Ok(());
                }
                if payloads.is_empty() {
                    if !had_colon {
                        return Err(ParseError::Message(format!(
                            "line {line_num}: {COLONLESS_SPAN_PUT}"
                        )));
                    }
                    self.push_block(anchor, vec![], line_num, BlockMode::Replace, None);
                    self.warn(EMPTY_PUT_AUTO_CUT_WARNING);
                    return Ok(());
                }
                self.push_block(
                    anchor,
                    payloads.into_iter().map(|r| r.text).collect(),
                    line_num,
                    BlockMode::Replace,
                    None,
                );
                Ok(())
            }
            Target::InsertAfterBlock { anchor, register } => {
                if register.is_some() || (!had_colon && payloads.is_empty()) {
                    self.push_block(
                        anchor,
                        vec![],
                        line_num,
                        BlockMode::PasteAfter,
                        register,
                    );
                    return Ok(());
                }
                if payloads.is_empty() {
                    return Err(ParseError::Message(format!(
                        "line {line_num}: {EMPTY_INSERT}"
                    )));
                }
                self.push_block(
                    anchor,
                    payloads.into_iter().map(|r| r.text).collect(),
                    line_num,
                    BlockMode::InsertAfter,
                    None,
                );
                Ok(())
            }
            Target::InsertBefore { anchor, register } => self.flush_gap(
                Cursor::BeforeAnchor { anchor },
                register,
                had_colon,
                payloads,
                line_num,
            ),
            Target::InsertAfter { anchor, register } => self.flush_gap(
                Cursor::AfterAnchor { anchor },
                register,
                had_colon,
                payloads,
                line_num,
            ),
            Target::Bof { register } => {
                self.flush_gap(Cursor::Bof, register, had_colon, payloads, line_num)
            }
            Target::Eof { register } => {
                self.flush_gap(Cursor::Eof, register, had_colon, payloads, line_num)
            }
        }
    }

    fn flush_gap(
        &mut self,
        cursor: Cursor,
        register: Option<String>,
        had_colon: bool,
        payloads: Vec<PayloadRow>,
        line_num: usize,
    ) -> Result<()> {
        if register.is_some() || (!had_colon && payloads.is_empty()) {
            self.push_paste(PasteTarget::Gap { cursor }, register, line_num);
            return Ok(());
        }
        if payloads.is_empty() {
            return Err(ParseError::Message(format!(
                "line {line_num}: {EMPTY_INSERT}"
            )));
        }
        for row in payloads {
            self.push_insert(cursor.clone(), row.text, line_num, false);
        }
        Ok(())
    }

    fn feed_op(&mut self, target: Target, had_colon: bool, line_num: usize) -> Result<()> {
        if had_colon && matches!(&target, Target::Cut { .. } | Target::CutBlock { .. }) {
            self.warn("`CUT` colon is ignored");
        }
        let has_register = match &target {
            Target::Replace { register, .. }
            | Target::Block { register, .. }
            | Target::InsertBefore { register, .. }
            | Target::InsertAfter { register, .. }
            | Target::InsertAfterBlock { register, .. }
            | Target::Cut { register, .. }
            | Target::CutBlock { register, .. }
            | Target::Bof { register }
            | Target::Eof { register } => register.is_some(),
            Target::Rem | Target::Move { .. } => false,
        };
        if had_colon && has_register && !matches!(target, Target::Rem | Target::Move { .. }) {
            return Err(ParseError::Message(format!(
                "line {line_num}: {COLON_ON_REGISTER_PUT}"
            )));
        }
        if matches!(target, Target::Rem) {
            self.flush_pending()?;
            return self.set_file_op(FileOp::Rem, line_num);
        }
        if let Target::Move { dest } = &target {
            self.flush_pending()?;
            return self.set_file_op(FileOp::Move { dest: dest.clone() }, line_num);
        }
        self.flush_pending()?;
        self.pending = Some(Pending {
            target,
            line_num,
            payloads: Vec::new(),
            had_colon,
            deferred_blanks: Vec::new(),
        });
        Ok(())
    }

    fn bodyless_message(target: &Target, had_colon: bool) -> Option<&'static str> {
        match target {
            Target::Cut { .. } | Target::CutBlock { .. } => Some(CUT_TAKES_NO_BODY),
            Target::Rem | Target::Move { .. } => Some(REM_TAKES_NO_BODY),
            Target::Replace { register: Some(_), .. }
            | Target::Block { register: Some(_), .. }
            | Target::InsertBefore { register: Some(_), .. }
            | Target::InsertAfter { register: Some(_), .. }
            | Target::InsertAfterBlock { register: Some(_), .. }
            | Target::Bof { register: Some(_) }
            | Target::Eof { register: Some(_) } => Some(COLON_ON_REGISTER_PUT),
            _ if !had_colon => Some("this op takes no body rows"),
            _ => None,
        }
    }

    fn handle_literal(&mut self, text: String, line_num: usize) -> Result<()> {
        let Some(pending) = self.pending.as_mut() else {
            if self.file_op.is_some() {
                return Err(ParseError::Message(format!(
                    "line {line_num}: file-level op takes no body rows"
                )));
            }
            return Err(ParseError::Message(format!(
                "line {line_num}: payload line has no preceding hunk header. Got {:?}.",
                format!("+{text}")
            )));
        };
        if let Some(msg) = Self::bodyless_message(&pending.target, pending.had_colon) {
            return Err(ParseError::Message(format!("line {line_num}: {msg}")));
        }
        if !pending.deferred_blanks.is_empty() {
            pending.payloads.append(&mut pending.deferred_blanks);
        }
        if is_hunk_header_text(&text) {
            self.warn(&format!(
                "line {line_num}: body row looks like an op header and will be inserted as text"
            ));
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.payloads.push(PayloadRow {
                text,
                line_num,
                bare: false,
                minus: false,
            });
        }
        Ok(())
    }

    fn handle_raw(&mut self, text: String, line_num: usize) -> Result<()> {
        if self.pending.is_some() {
            let (had_colon, target_msg, has_payloads, has_deferred) = {
                let pending = self.pending.as_ref().expect("pending");
                (
                    pending.had_colon,
                    Self::bodyless_message(&pending.target, pending.had_colon).map(str::to_string),
                    !pending.payloads.is_empty(),
                    !pending.deferred_blanks.is_empty(),
                )
            };
            if text.trim().is_empty() {
                if target_msg.is_none() && has_payloads {
                    if let Some(pending) = self.pending.as_mut() {
                        pending.deferred_blanks.push(PayloadRow {
                            text,
                            line_num,
                            bare: true,
                            minus: false,
                        });
                    }
                }
                return Ok(());
            }
            if let Some(msg) = target_msg {
                return Err(ParseError::Message(format!("line {line_num}: {msg}")));
            }
            let _ = had_colon;
            if has_deferred {
                self.warn(BARE_BODY_AUTO_PIPED_WARNING);
            }
            let minus = text.trim_start().starts_with('-');
            if !minus {
                self.warn(BARE_BODY_AUTO_PIPED_WARNING);
            }
            if let Some(pending) = self.pending.as_mut() {
                if !pending.deferred_blanks.is_empty() {
                    pending.payloads.append(&mut pending.deferred_blanks);
                }
                pending.payloads.push(PayloadRow {
                    text,
                    line_num,
                    bare: true,
                    minus,
                });
            }
            return Ok(());
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        Err(ParseError::Message(format!(
            "line {line_num}: payload line has no preceding hunk header. Use `PUT N.=M:`, `CUT N.=M`, or `PUT <N:`/`PUT >N:` above the body. Got {text:?}."
        )))
    }

    fn feed_line(&mut self, line: &str, line_num: usize) -> Result<()> {
        if line == "*** Begin Patch" || line == "*** End Patch" {
            if line == "*** End Patch" {
                self.flush_pending()?;
            }
            return Ok(());
        }
        if line == "*** Abort" {
            self.flush_pending()?;
            return Ok(());
        }
        if parse_hashline_header(line).is_some() {
            self.flush_pending()?;
            return Ok(());
        }
        if line.is_empty() {
            return self.handle_raw(line.to_string(), line_num);
        }
        if let Some(rest) = line.strip_prefix('+') {
            return self.handle_literal(rest.to_string(), line_num);
        }
        if let Some(locator) = line.strip_prefix("PUT ") {
            let (target, had_colon) = parse_put_target(locator)?;
            return self.feed_op(target, had_colon, line_num);
        }
        if let Some(locator) = line.strip_prefix("CUT ") {
            let (target, had_colon) = parse_cut_target(locator)?;
            return self.feed_op(target, had_colon, line_num);
        }
        if line.trim() == "REM" {
            return self.feed_op(Target::Rem, false, line_num);
        }
        if let Some(dest) = line.strip_prefix("MV ") {
            return self.feed_op(
                Target::Move {
                    dest: unquote_path(dest),
                },
                false,
                line_num,
            );
        }
        self.handle_raw(line.to_string(), line_num)
    }

    fn finish(mut self, streaming: bool) -> Result<ParsedBody> {
        if streaming {
            if let Some(pending) = &self.pending {
                let complete = matches!(
                    pending.target,
                    Target::Cut { .. } | Target::CutBlock { .. }
                ) || match &pending.target {
                    Target::Replace { register: Some(_), .. }
                    | Target::Block { register: Some(_), .. }
                    | Target::InsertBefore { register: Some(_), .. }
                    | Target::InsertAfter { register: Some(_), .. }
                    | Target::InsertAfterBlock { register: Some(_), .. }
                    | Target::Bof { register: Some(_) }
                    | Target::Eof { register: Some(_) } => true,
                    Target::InsertBefore { .. }
                    | Target::InsertAfter { .. }
                    | Target::InsertAfterBlock { .. }
                    | Target::Bof { .. }
                    | Target::Eof { .. }
                        if !pending.had_colon =>
                    {
                        true
                    }
                    _ => !pending.payloads.is_empty(),
                };
                if complete {
                    self.flush_pending()?;
                } else {
                    self.pending = None;
                }
            }
        } else {
            self.flush_pending()?;
        }
        if matches!(self.file_op, Some(FileOp::Rem)) && !self.edits.is_empty() {
            return Err(ParseError::Message(REM_TAKES_NO_BODY.into()));
        }
        Ok(ParsedBody {
            edits: self.edits,
            file_op: self.file_op,
            warnings: self.warnings,
        })
    }
}

fn resolve_minus_rows(payloads: &mut Vec<PayloadRow>) -> Result<()> {
    let first_minus = payloads.iter().find(|r| r.minus).cloned();
    let Some(first) = first_minus else {
        return Ok(());
    };
    let all_bullet = payloads
        .iter()
        .filter(|r| r.minus)
        .all(|r| r.text.trim_start().starts_with("- "));
    let has_explicit = payloads.iter().any(|r| !r.bare);
    let has_explicit_bullet = payloads
        .iter()
        .any(|r| !r.bare && r.text.trim_start().starts_with("- "));
    if all_bullet && (!has_explicit || has_explicit_bullet) {
        return Ok(());
    }
    if has_explicit && !all_bullet {
        payloads.retain(|r| !r.minus);
        return Ok(());
    }
    Err(ParseError::Message(format!(
        "line {}: {MINUS_ROW_REJECTED}",
        first.line_num
    )))
}

fn strip_bare_prefixes_if_uniform(payloads: &mut [PayloadRow]) {
    let mut saw_bare = false;
    let mut all_literal = true;
    for row in payloads.iter() {
        if !row.bare || row.text.trim().is_empty() {
            continue;
        }
        saw_bare = true;
        let stripped = strip_one_leading_prefix(&row.text);
        if stripped == row.text {
            return;
        }
        let t = stripped.trim();
        let looks_literal = (t.starts_with('"') && t.ends_with('"'))
            || (t.starts_with('\'') && t.ends_with('\''))
            || t.parse::<f64>().is_ok()
            || t.trim_end_matches(',').parse::<f64>().is_ok();
        all_literal &= looks_literal;
    }
    if !saw_bare || all_literal {
        return;
    }
    for row in payloads.iter_mut() {
        if row.bare && !row.text.trim().is_empty() {
            row.text = strip_one_leading_prefix(&row.text);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedBody {
    pub edits: Vec<Edit>,
    pub file_op: Option<FileOp>,
    pub warnings: Vec<String>,
}

pub fn parse_patch(diff: &str) -> Result<ParsedBody> {
    parse_patch_inner(diff, false)
}

pub fn parse_patch_streaming(diff: &str) -> Result<ParsedBody> {
    parse_patch_inner(diff, true)
}

fn parse_patch_inner(diff: &str, streaming: bool) -> Result<ParsedBody> {
    let mut exec = Executor::new();
    for (i, line) in split_input_lines(diff).into_iter().enumerate() {
        exec.feed_line(&line, i + 1)?;
    }
    exec.finish(streaming)
}

fn split_input_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.trim_end_matches('\r').to_string()).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

pub fn parse_input(input: &str) -> Result<Patch> {
    let lines = split_input_lines(input.trim_start_matches(['\n', '\r']));
    let mut sections = Vec::new();
    let mut current: Option<PatchSection> = None;
    let mut body = Vec::new();

    let flush = |current: &mut Option<PatchSection>, body: &mut Vec<String>, sections: &mut Vec<PatchSection>| {
        if let Some(mut section) = current.take() {
            section.body = body.join("\n");
            if !section.body.is_empty() || section.file_hash.is_some() {
                sections.push(section);
            }
            body.clear();
        }
    };

    for line in &lines {
        if line == "*** Begin Patch" || line == "*** End Patch" || line == "*** Abort" {
            if line == "*** Abort" {
                flush(&mut current, &mut body, &mut sections);
                break;
            }
            continue;
        }
        if let Some((path, hash)) = parse_hashline_header(line) {
            flush(&mut current, &mut body, &mut sections);
            current = Some(PatchSection {
                path,
                file_hash: hash,
                body: String::new(),
            });
            continue;
        }
        if current.is_none() {
            // Bare ops without a header are a single anonymous section; the
            // patcher still requires a tag before apply.
            current = Some(PatchSection {
                path: String::new(),
                file_hash: None,
                body: String::new(),
            });
        }
        body.push(line.clone());
    }
    flush(&mut current, &mut body, &mut sections);
    Ok(Patch { sections })
}

pub fn collect_anchor_lines(edits: &[Edit]) -> Vec<usize> {
    let mut lines = Vec::new();
    for edit in edits {
        for anchor in edit.anchors() {
            if !lines.contains(&anchor.line) {
                lines.push(anchor.line);
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply_edits;

    fn apply_patch(text: &str, diff: &str) -> String {
        apply_edits(text, &parse_patch(diff).unwrap().edits, None).unwrap().text
    }

    #[test]
    fn replaces_concrete_range() {
        assert_eq!(
            apply_patch("a\nb\nc", "PUT 2.=2:\n+before\n+after"),
            "a\nbefore\nafter\nc"
        );
    }

    #[test]
    fn deletes_and_inserts() {
        assert_eq!(apply_patch("a\nb\nc", "CUT 2.=2"), "a\nc");
        assert_eq!(apply_patch("a\nb\nc\nd", "CUT 2.=3"), "a\nd");
        assert_eq!(
            apply_patch("a\nb\nc", "PUT <2:\n+before\nPUT >2:\n+after"),
            "a\nbefore\nb\nafter\nc"
        );
        assert_eq!(apply_patch("a\nb", "PUT <1:\n+HEAD"), "HEAD\na\nb");
        assert_eq!(apply_patch("a\nb", "PUT >$:\n+TAIL"), "a\nb\nTAIL");
    }

    #[test]
    fn lenient_separators() {
        for sep in ["-", ".", "=", "..", "…", " "] {
            assert_eq!(apply_patch("a\nb\nc\nd", &format!("CUT 2{sep}3")), "a\nd");
            assert_eq!(
                apply_patch("a\nb\nc\nd", &format!("PUT 2{sep}3:\n+middle")),
                "a\nmiddle\nd"
            );
        }
    }

    #[test]
    fn empty_replace_is_delete() {
        let parsed = parse_patch("PUT 2-3:").unwrap();
        assert_eq!(
            apply_edits("a\nb\nc\nd", &parsed.edits, None).unwrap().text,
            "a\nd"
        );
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w.contains("empty `PUT` body as deletion"))
        );
        assert!(parse_patch("PUT <1:").is_err());
    }

    #[test]
    fn rejects_body_under_cut() {
        let err = parse_patch("CUT 2\n+replacement").unwrap_err().to_string();
        assert!(err.contains("takes no body rows"), "{err}");
    }

    #[test]
    fn auto_pipes_bare_rows() {
        assert_eq!(apply_patch("a\nb\nc", "PUT 2-2:\nraw"), "a\nraw\nc");
        let parsed = parse_patch("PUT 2-2:\n3:replaced").unwrap();
        assert_eq!(
            apply_edits("a\nb\nc", &parsed.edits, None).unwrap().text,
            "a\nreplaced\nc"
        );
    }

    #[test]
    fn phantom_sentinel() {
        let edits = parse_patch("CUT 3").unwrap().edits;
        assert_eq!(
            apply_edits("a\nb\n", &edits, None).unwrap().text,
            "a\nb\n"
        );
        let edits = parse_patch("CUT 2-3").unwrap().edits;
        assert_eq!(apply_edits("a\nb\n", &edits, None).unwrap().text, "a\n");
        let edits = parse_patch("PUT 2-3:\n+B").unwrap().edits;
        assert_eq!(apply_edits("a\nb\n", &edits, None).unwrap().text, "a\nB\n");
        let edits = parse_patch("CUT 2").unwrap().edits;
        assert_eq!(apply_edits("a\nb", &edits, None).unwrap().text, "a");
    }

    #[test]
    fn range_limit() {
        let err = parse_patch("PUT 1-100001:\n+x").unwrap_err().to_string();
        assert!(err.contains("100001"), "{err}");
    }

    #[test]
    fn streaming_empty_replace_not_flushed() {
        let result = parse_patch_streaming("PUT 5-5:\n").unwrap();
        assert!(result.edits.is_empty());
    }
}
