use jcode_shell_minimizer::{MinimizerConfig, MinimizerOptions};

pub(super) fn live_minimizer_options() -> MinimizerOptions {
    let cfg = &crate::config::config().shell_minimizer;
    MinimizerOptions {
        enabled: Some(cfg.enabled),
        settings_path: cfg.settings_path.clone(),
        settings_hash: None,
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

pub(super) fn minimize_command_output(
    command: &str,
    output: String,
    exit_code: Option<i32>,
) -> String {
    minimize_with(command, output, exit_code, &live_minimizer_options())
}

pub(super) fn minimize_with(
    command: &str,
    output: String,
    exit_code: Option<i32>,
    options: &MinimizerOptions,
) -> String {
    let config = MinimizerConfig::from_options(options);
    if !config.enabled {
        return output;
    }
    let result = jcode_shell_minimizer::apply(command, &output, exit_code.unwrap_or(0), &config);
    if !result.changed {
        return output;
    }
    let mut text = result.text;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "[output minimized via {} : {} -> {} bytes]",
        result.filter, result.input_bytes, result.output_bytes
    ));
    text
}
