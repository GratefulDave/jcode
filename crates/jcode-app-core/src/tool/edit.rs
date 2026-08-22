use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, FileOp, FileTouch};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use std::path::Path;

const FILE_TOUCH_PREVIEW_MAX_LINES: usize = 6;
const FILE_TOUCH_PREVIEW_MAX_BYTES: usize = 240;

pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct EditInput {
    #[serde(default)]
    intent: Option<String>,
    /// Hashline patch. Preferred over old_string/new_string.
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file. Prefer hashline `input` with [PATH#TAG] from the latest read. REM deletes the file. MV DEST renames it. old_string/new_string still works."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "input": {
                    "type": "string",
                    "description": "Hashline patch: [PATH#TAG] then PUT/CUT/REM/MV. TAG is the 4-hex snapshot from read/write/edit. REM deletes the file. MV DEST renames it. New files use write. Body rows start with +; do not paste N:text read lines."
                },
                "file_path": {
                    "type": "string",
                    "description": "File path for old_string/new_string fallback."
                },
                "old_string": {
                    "type": "string",
                    "description": "Text to replace when not using hashline input."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text when not using hashline input."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all matches for old_string."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: EditInput = serde_json::from_value(input)?;
        if let Some(patch) = params
            .input
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return execute_hashline(patch, params.intent.as_deref(), &ctx);
        }
        let file_path = params.file_path.ok_or_else(|| {
            anyhow::anyhow!("edit needs hashline `input` or file_path + old_string + new_string")
        })?;
        let old_string = params.old_string.ok_or_else(|| {
            anyhow::anyhow!("edit needs hashline `input` or file_path + old_string + new_string")
        })?;
        let new_string = params.new_string.ok_or_else(|| {
            anyhow::anyhow!("edit needs hashline `input` or file_path + old_string + new_string")
        })?;

        if old_string == new_string {
            return Err(anyhow::anyhow!(
                "old_string and new_string must be different"
            ));
        }

        let path = ctx.resolve_path(Path::new(&file_path));

        if !path.exists() {
            return Err(anyhow::anyhow!("File not found: {}", file_path));
        }

        let content = tokio::fs::read_to_string(&path).await?;

        // Count occurrences
        let occurrences = content.matches(&old_string).count();

        if occurrences == 0 {
            // Try flexible matching
            return try_flexible_match(&content, &old_string, &file_path);
        }

        if occurrences > 1 && !params.replace_all {
            return Err(anyhow::anyhow!(
                "old_string found {} times in the file. Either:\n\
                 1. Provide more context to make it unique, or\n\
                 2. Set replace_all: true to replace all occurrences",
                occurrences
            ));
        }

        // Perform replacement
        let new_content = if params.replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };

        // Find line number where edit starts
        let start_line = find_line_number(&content, &old_string);

        // Write back
        tokio::fs::write(&path, &new_content).await?;

        // Generate a diff with line numbers
        let diff = generate_diff(&old_string, &new_string, start_line);

        // Publish file touch event for swarm coordination
        let end_line = start_line + new_string.lines().count().saturating_sub(1);
        let detail = build_file_touch_preview(&diff);
        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: path.to_path_buf(),
            op: FileOp::Edit,
            intent: params
                .intent
                .clone()
                .filter(|value| !value.trim().is_empty()),
            summary: Some(format!(
                "edited lines {}-{} ({} occurrence{})",
                start_line,
                end_line,
                occurrences,
                if occurrences == 1 { "" } else { "s" }
            )),
            detail,
        }));

        // Extract context around the edit to help with consecutive edits
        let end_line = start_line + new_string.lines().count().saturating_sub(1);
        let context = extract_context(&new_content, start_line, end_line, 3);

        let mut body = format!(
            "Edited {}: replaced {} occurrence(s)\n{}\n\nContext after edit (lines {}-{}):\n{}",
            file_path, occurrences, diff, context.0, context.1, context.2
        );
        super::config_edit_notice::append_config_edit_notice(
            &mut body,
            &path,
            &content,
            &new_content,
        );

        Ok(ToolOutput::new(body).with_title(file_path))
    }
}

fn execute_hashline(
    input: &str,
    intent: Option<&str>,
    ctx: &ToolContext,
) -> Result<ToolOutput> {
    let patch = jcode_hashline::parse_input(input).map_err(|err| anyhow::anyhow!("{err}"))?;
    if patch.sections.is_empty() {
        return Err(anyhow::anyhow!(
            "hashline input needs at least one [PATH#TAG] section"
        ));
    }
    let cwd = ctx
        .working_dir
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let config_watch = super::config_edit_notice::ConfigEditWatch::begin();
    let results = jcode_hashline::with_session(&ctx.session_id, |store, clipboard| {
        jcode_hashline::apply_patch_to_disk(&patch, store, clipboard, &cwd, true)
    })
    .map_err(|err| anyhow::anyhow!("{err}"))?;

    let mut body = String::new();
    let mut title = None;
    for result in &results {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&result.header);
        body.push('\n');
        if result.op == "delete" {
            body.push_str("deleted file\n");
        } else if result.op == "noop" {
            body.push_str("no changes\n");
        } else {
            if let Some(dest) = &result.move_dest {
                body.push_str(&format!("moved to {dest}\n"));
            }
            for resolution in &result.block_resolutions {
                body.push_str(&format!(
                    "PUT {}*: → resolved lines {}-{}\n",
                    resolution.line, resolution.start, resolution.end
                ));
            }
            if let Some(line) = result.first_changed_line {
                body.push_str(&format!("first changed line {line}\n"));
            }
            let preview = generate_changed_span_diff(&result.before, &result.after);
            if !preview.is_empty() {
                body.push_str(&preview);
                if !preview.ends_with('\n') {
                    body.push('\n');
                }
            }
        }
        if !result.warnings.is_empty() {
            body.push_str("Warnings:\n");
            for warning in &result.warnings {
                body.push_str(warning);
                body.push('\n');
            }
        }
        title.get_or_insert_with(|| result.path.clone());
        let dest = result
            .move_dest
            .as_deref()
            .unwrap_or(result.path.as_str());
        let dest_path = ctx.resolve_path(Path::new(dest));
        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: dest_path,
            op: FileOp::Edit,
            intent: intent
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty()),
            summary: Some(format!("{} {}", result.op, dest)),
            detail: None,
        }));
    }
    config_watch.finish(&mut body);
    let mut output = ToolOutput::new(body);
    if let Some(title) = title {
        output = output.with_title(title);
    }
    Ok(output)
}


/// Find the 1-based line number where a substring starts
fn find_line_number(content: &str, substring: &str) -> usize {
    if let Some(pos) = content.find(substring) {
        content[..pos].lines().count() + 1
    } else {
        1
    }
}

/// Generate a compact diff: "42- old" / "42+ new"
fn generate_diff(old: &str, new: &str, start_line: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    let mut old_line = start_line;
    let mut new_line = start_line;

    for change in diff.iter_all_changes() {
        let content = change.value().trim();
        let (prefix, line_num) = match change.tag() {
            ChangeTag::Delete => {
                let num = old_line;
                old_line += 1;
                if content.is_empty() {
                    continue;
                }
                ("-", num)
            }
            ChangeTag::Insert => {
                let num = new_line;
                new_line += 1;
                if content.is_empty() {
                    continue;
                }
                ("+", num)
            }
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
                continue;
            }
        };

        // Compact format: "42- content" (no spaces)
        output.push_str(&format!("{}{} {}\n", line_num, prefix, content));
    }

    if output.is_empty() {
        String::new()
    } else {
        output.trim_end().to_string()
    }
}

const HASHLINE_DIFF_MAX_LINES: usize = 30;

fn changed_span(before: &str, after: &str) -> (String, String, usize) {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut start = 0;
    while start < old.len() && start < new.len() && old[start] == new[start] {
        start += 1;
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > start && new_end > start && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    (
        old[start..old_end].join("\n"),
        new[start..new_end].join("\n"),
        start + 1,
    )
}

fn generate_changed_span_diff(before: &str, after: &str) -> String {
    let (old, new, start) = changed_span(before, after);
    let diff = generate_diff(&old, &new, start);
    let mut lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= HASHLINE_DIFF_MAX_LINES {
        return diff;
    }
    lines.truncate(HASHLINE_DIFF_MAX_LINES);
    let mut out = lines.join("\n");
    out.push_str("\n...");
    out
}


fn build_file_touch_preview(diff: &str) -> Option<String> {
    let trimmed = diff.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut lines = trimmed.lines();
    let mut preview = lines
        .by_ref()
        .take(FILE_TOUCH_PREVIEW_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut truncated = lines.next().is_some();

    if preview.len() > FILE_TOUCH_PREVIEW_MAX_BYTES {
        preview = crate::util::truncate_str(&preview, FILE_TOUCH_PREVIEW_MAX_BYTES)
            .trim_end()
            .to_string();
        truncated = true;
    }

    if truncated {
        preview.push_str("\n…");
    }

    Some(preview)
}

/// Extract lines around the edited region, returns (start_line, end_line, content)
fn extract_context(
    content: &str,
    edit_start: usize,
    edit_end: usize,
    padding: usize,
) -> (usize, usize, String) {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Calculate range with padding (1-indexed to 0-indexed)
    let start = edit_start.saturating_sub(padding + 1);
    let end = (edit_end + padding).min(total_lines);

    let context_lines: Vec<String> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>4}│ {}", start + i + 1, line))
        .collect();

    (start + 1, end, context_lines.join("\n"))
}

fn try_flexible_match(content: &str, old_string: &str, file_path: &str) -> Result<ToolOutput> {
    // Try trimmed matching
    let trimmed = old_string.trim();
    if content.contains(trimmed) && trimmed != old_string {
        return Err(anyhow::anyhow!(
            "old_string not found exactly, but found after trimming whitespace.\n\
             Try using the exact string from the file, including leading/trailing whitespace."
        ));
    }

    // Try line-by-line matching with normalized whitespace
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    for (i, window) in content_lines.windows(old_lines.len()).enumerate() {
        let matches = window
            .iter()
            .zip(old_lines.iter())
            .all(|(a, b)| a.trim() == b.trim());

        if matches {
            return Err(anyhow::anyhow!(
                "old_string not found exactly, but found with different indentation around line {}.\n\
                 Make sure to preserve the exact whitespace from the file.",
                i + 1
            ));
        }
    }

    Err(anyhow::anyhow!(
        "old_string not found in {}.\n\
         Use the read tool to see the current file contents.",
        file_path
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_diff_single_line_change() {
        let old = "hello world";
        let new = "hello rust";
        let diff = generate_diff(old, new, 10);

        // Compact format: "10- content" / "10+ content"
        assert!(diff.contains("10- hello world"), "Should show deleted line");
        assert!(diff.contains("10+ hello rust"), "Should show added line");
    }

    #[test]
    fn test_generate_diff_multi_line() {
        let old = "line one\nline two\nline three";
        let new = "line one\nmodified two\nline three";
        let diff = generate_diff(old, new, 5);

        // Line 6 should be the changed line (5 + 1 for "line two")
        assert!(diff.contains("6- line two"), "Should show deleted line");
        assert!(diff.contains("6+ modified two"), "Should show added line");
        // Equal lines should not appear
        assert!(
            !diff.contains("line one"),
            "Should not show unchanged lines"
        );
        assert!(
            !diff.contains("line three"),
            "Should not show unchanged lines"
        );
    }

    #[test]
    fn test_generate_diff_addition_only() {
        let old = "first\nthird";
        let new = "first\nsecond\nthird";
        let diff = generate_diff(old, new, 1);

        assert!(diff.contains("+ second"), "Should show added line");
    }

    #[test]
    fn test_generate_diff_deletion_only() {
        let old = "first\nsecond\nthird";
        let new = "first\nthird";
        let diff = generate_diff(old, new, 1);

        assert!(diff.contains("- second"), "Should show deleted line");
    }

    #[test]
    fn test_generate_diff_no_changes() {
        let old = "same content";
        let new = "same content";
        let diff = generate_diff(old, new, 1);

        assert!(diff.is_empty(), "No changes should produce empty diff");
    }

    #[test]
    fn test_changed_span_skips_unchanged_head() {
        let before = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = before.replace("line 10", "changed 10");
        let (old, new, start) = changed_span(&before, &after);
        assert_eq!(start, 10);
        assert_eq!(old, "line 10");
        assert_eq!(new, "changed 10");
        let diff = generate_changed_span_diff(&before, &after);
        assert!(diff.contains("10- line 10"), "{diff}");
        assert!(diff.contains("10+ changed 10"), "{diff}");
        assert!(!diff.contains("line 1"), "{diff}");
        assert!(!diff.contains("line 20"), "{diff}");
    }

    #[test]
    fn test_changed_span_diff_caps() {
        let before = (1..=80)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (1..=80)
            .map(|i| format!("new {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = generate_changed_span_diff(&before, &after);
        let lines = diff.lines().count();
        assert!(lines <= HASHLINE_DIFF_MAX_LINES + 1, "{lines} {diff}");
        assert!(diff.contains("..."), "{diff}");
    }


    #[test]
    fn test_generate_diff_line_number_format() {
        let old = "old";
        let new = "new";
        let diff = generate_diff(old, new, 42);

        // Compact format: no padding
        assert!(
            diff.contains("42- old"),
            "Should have line number directly before minus"
        );
        assert!(
            diff.contains("42+ new"),
            "Should have line number directly before plus"
        );
    }

    #[test]
    fn test_find_line_number() {
        let content = "line 1\nline 2\nline 3\nline 4";

        assert_eq!(find_line_number(content, "line 1"), 1);
        assert_eq!(find_line_number(content, "line 2"), 2);
        assert_eq!(find_line_number(content, "line 3"), 3);
        assert_eq!(find_line_number(content, "line 4"), 4);
        assert_eq!(find_line_number(content, "not found"), 1);
    }

    #[test]
    fn test_extract_context() {
        let content =
            "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10";

        // Edit at line 5, with 2 lines padding
        let (start, end, ctx) = extract_context(content, 5, 5, 2);

        assert_eq!(start, 3, "Should start at line 3 (5 - 2)");
        assert_eq!(end, 7, "Should end at line 7 (5 + 2)");
        assert!(ctx.contains("line 3"), "Should include line 3");
        assert!(ctx.contains("line 5"), "Should include edited line 5");
        assert!(ctx.contains("line 7"), "Should include line 7");
        assert!(!ctx.contains("line 2"), "Should not include line 2");
        assert!(!ctx.contains("line 8"), "Should not include line 8");
    }

    #[test]
    fn test_extract_context_at_start() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";

        // Edit at line 1, with 2 lines padding - shouldn't go negative
        let (start, _end, ctx) = extract_context(content, 1, 1, 2);

        assert_eq!(start, 1, "Should start at line 1 (can't go before)");
        assert!(ctx.contains("line 1"), "Should include line 1");
        assert!(ctx.contains("line 3"), "Should include line 3");
    }

    #[test]
    fn test_extract_context_at_end() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";

        // Edit at line 5, with 2 lines padding - shouldn't go past end
        let (_start, end, ctx) = extract_context(content, 5, 5, 2);

        assert_eq!(end, 5, "Should end at line 5 (can't go past)");
        assert!(ctx.contains("line 5"), "Should include line 5");
        assert!(ctx.contains("line 3"), "Should include line 3");
    }

    #[test]
    fn test_extract_context_range_past_end() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";

        // Edit range extends past the end of the file.
        let (start, end, ctx) = extract_context(content, 4, 10, 1);

        assert_eq!(start, 3, "Should start at line 3 (4 - 1)");
        assert_eq!(end, 5, "Should clamp to last line");
        assert!(ctx.contains("line 3"), "Should include line 3");
        assert!(ctx.contains("line 5"), "Should include line 5");
    }

    fn test_ctx(working_dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "hashline-edit".to_string(),
            message_id: "m".to_string(),
            tool_call_id: "c".to_string(),
            working_dir: Some(working_dir.to_path_buf()),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::Direct,
        }
    }

    #[tokio::test]
    async fn hashline_edit_applies_fresh_tag_and_rejects_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("greet.py");
        std::fs::write(&path, "def greet(name):\n    print(name)\n").unwrap();
        let ctx = test_ctx(temp.path());
        let key = path.canonicalize().unwrap().to_string_lossy().into_owned();
        let tag = jcode_hashline::record_snapshot(
            &ctx.session_id,
            &key,
            "def greet(name):\n    print(name)\n",
            Some([1, 2]),
        )
        .expect("tag");
        let tool = EditTool::new();
        let output = tool
            .execute(
                serde_json::json!({
                    "input": format!("[greet.py#{tag}]\nPUT 2.=2:\n+    print(f\"hi {{name}}\")")
                }),
                ctx.clone(),
            )
            .await
            .expect("fresh hashline should apply");
        assert!(
            output.output.contains('#'),
            "reminted header missing: {}",
            output.output
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("print(f\"hi {name}\")"));

        let err = tool
            .execute(
                serde_json::json!({
                    "input": format!("[greet.py#{tag}]\nPUT 2.=2:\n+    pass")
                }),
                ctx,
            )
            .await
            .expect_err("stale tag must reject");
        assert!(err.to_string().contains("stale"), "{err}");
    }

    #[tokio::test]
    async fn old_string_fallback_still_works() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("note.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let tool = EditTool::new();
        let output = tool
            .execute(
                serde_json::json!({
                    "file_path": "note.txt",
                    "old_string": "hello world",
                    "new_string": "hello rust"
                }),
                test_ctx(temp.path()),
            )
            .await
            .expect("fallback edit");
        assert!(output.output.contains("hello rust") || output.output.contains("replaced"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust\n");
    }
}
