use jcode_shell_minimizer::{hash_settings_file, MinimizerConfig, MinimizerOptions};

pub(super) fn live_minimizer_options() -> MinimizerOptions {
    let cfg = &crate::config::config().shell_minimizer;
    MinimizerOptions {
        enabled: Some(cfg.enabled),
        settings_path: cfg.settings_path.clone(),
        // Trust gate (lex parity): honor an explicit `settings_hash` pin from
        // config; otherwise derive the hash from the resolved settings file
        // content so `from_options` refuses a file mutated after resolution.
        settings_hash: cfg.settings_hash.clone().or_else(|| {
            cfg.settings_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .and_then(hash_settings_file)
        }),
        only: if cfg.only.is_empty() {
            None
        } else {
            Some(cfg.only.clone())
        },
        except: if cfg.except.is_empty() {
            None
        } else {
            Some(cfg.except.clone())
        },
        max_capture_bytes: Some(cfg.max_capture_bytes),
        source_outline_level: Some(cfg.source_outline_level.clone()),
        legacy_filters: cfg.legacy_filters,
    }
}

/// Result of a minimization pass: the (possibly transformed) body plus its
/// `[output minimized via …]` marker kept separate. The caller appends the
/// marker AFTER any `MAX_OUTPUT_LEN` truncation so the marker always
/// survives, even when the minimized body alone exceeds the cap.
pub(super) struct MinimizedOutput {
    pub text:   String,
    pub footer: Option<String>,
}

pub(super) fn minimize_command_output(
    command: &str,
    output: String,
    exit_code: Option<i32>,
) -> MinimizedOutput {
    minimize_with(command, output, exit_code, &live_minimizer_options())
}

pub(super) fn minimize_with(
    command: &str,
    output: String,
    exit_code: Option<i32>,
    options: &MinimizerOptions,
) -> MinimizedOutput {
    let config = MinimizerConfig::from_options(options);
    if !config.enabled {
        return MinimizedOutput { text: output, footer: None };
    }
    let result = jcode_shell_minimizer::apply(command, &output, exit_code.unwrap_or(0), &config);
    if !result.changed {
        return MinimizedOutput { text: output, footer: None };
    }
    MinimizedOutput {
        text: result.text,
        footer: Some(format!(
            "[output minimized via {} : {} -> {} bytes]",
            result.filter, result.input_bytes, result.output_bytes
        )),
    }
}
