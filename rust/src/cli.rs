use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::Parser;

use crate::config::WorkflowConfig;

const DEFAULT_WORKFLOW_PATH: &str = "./WORKFLOW.md";
const ACKNOWLEDGEMENT_SWITCH: &str =
    "i-understand-that-this-will-be-running-without-the-usual-guardrails";

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "symphony",
    about = "Rust implementation of the Symphony service"
)]
pub struct Cli {
    /// Path to WORKFLOW.md. Defaults to ./WORKFLOW.md
    #[arg(value_name = "path", default_value = DEFAULT_WORKFLOW_PATH)]
    pub workflow_path: PathBuf,

    /// Override server.port from the workflow config
    #[arg(long)]
    pub port: Option<u16>,

    /// Root directory for structured log output
    #[arg(long, value_name = "path")]
    pub logs_root: Option<PathBuf>,

    /// Required acknowledgement for the preview guardrails posture
    #[arg(long = ACKNOWLEDGEMENT_SWITCH, default_value_t = false)]
    pub guardrails_acknowledged: bool,
}

impl Cli {
    pub fn require_guardrails_acknowledgement(&self) -> Result<()> {
        if self.guardrails_acknowledged {
            Ok(())
        } else {
            bail!(acknowledgement_banner())
        }
    }

    pub fn expanded_workflow_path(&self) -> Result<PathBuf> {
        expand_path(&self.workflow_path)
    }

    pub fn expanded_logs_root(&self) -> Result<Option<PathBuf>> {
        self.logs_root.as_deref().map(expand_path).transpose()
    }

    pub fn validate_workflow_path(&self) -> Result<PathBuf> {
        let path = self.expanded_workflow_path()?;
        if path.is_file() {
            Ok(path)
        } else {
            Err(anyhow!("Workflow file not found: {}", path.display()))
        }
    }

    pub fn apply_overrides(&self, config: &mut WorkflowConfig) {
        if let Some(port) = self.port {
            config.server.port = Some(port);
        }
    }

    pub fn is_headless(&self, has_terminal: bool) -> bool {
        self.port.is_none() && !has_terminal
    }
}

pub fn acknowledgement_banner() -> String {
    let lines = [
        "This Symphony implementation is a low key engineering preview.",
        "Codex will run without any guardrails.",
        "Symphony Rust is not a supported product and is presented as-is.",
        "To proceed, start with `--i-understand-that-this-will-be-running-without-the-usual-guardrails`.",
    ];

    let width = lines
        .iter()
        .map(|line| line.len())
        .max()
        .unwrap_or_default();
    let border = "─".repeat(width + 2);

    let mut output = String::new();
    let _ = writeln!(&mut output, "╭{border}╮");
    let _ = writeln!(&mut output, "│ {:width$} │", "", width = width);
    for line in lines {
        let _ = writeln!(&mut output, "│ {:width$} │", line, width = width);
    }
    let _ = writeln!(&mut output, "│ {:width$} │", "", width = width);
    let _ = write!(&mut output, "╰{border}╯");
    output
}

fn expand_path(path: &Path) -> Result<PathBuf> {
    let raw = path
        .to_str()
        .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {}", path.display()))?;

    let expanded = if raw == "~" {
        dirs::home_dir().ok_or_else(|| anyhow!("Failed to resolve home directory"))?
    } else if let Some(suffix) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| anyhow!("Failed to resolve home directory"))?
            .join(suffix)
    } else {
        PathBuf::from(raw)
    };

    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use std::fs;

    #[test]
    fn parses_default_workflow_path() {
        let cli = Cli::parse_from([
            "symphony",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
        ]);

        assert_eq!(cli.workflow_path, PathBuf::from("./WORKFLOW.md"));
        assert!(cli.guardrails_acknowledged);
    }

    #[test]
    fn parses_explicit_path_port_and_logs_root() {
        let cli = Cli::parse_from([
            "symphony",
            "--port",
            "4040",
            "--logs-root",
            "tmp/logs",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
            "config/WORKFLOW.md",
        ]);

        assert_eq!(cli.workflow_path, PathBuf::from("config/WORKFLOW.md"));
        assert_eq!(cli.port, Some(4040));
        assert_eq!(cli.logs_root, Some(PathBuf::from("tmp/logs")));
    }

    #[test]
    fn applies_server_port_override() {
        let cli = Cli::parse_from([
            "symphony",
            "--port",
            "8080",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
        ]);
        let mut config =
            WorkflowConfig::from_value(json!({})).expect("default workflow config should parse");

        cli.apply_overrides(&mut config);

        assert_eq!(config.server.port, Some(8080));
    }

    #[test]
    fn detects_headless_mode_without_terminal_or_port() {
        let cli = Cli::parse_from([
            "symphony",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
        ]);

        assert!(cli.is_headless(false));
        assert!(!cli.is_headless(true));
    }

    #[test]
    fn rejects_missing_guardrails_acknowledgement() {
        let cli = Cli::parse_from(["symphony"]);

        let error = cli
            .require_guardrails_acknowledgement()
            .expect_err("acknowledgement must be required");

        assert!(error.to_string().contains("without-the-usual-guardrails"));
    }

    #[test]
    fn rejects_missing_workflow_path() {
        let missing = std::env::temp_dir().join("definitely-missing-workflow.md");
        let cli = Cli::parse_from([
            "symphony",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
            missing.to_string_lossy().as_ref(),
        ]);

        let error = cli
            .validate_workflow_path()
            .expect_err("missing workflow path should fail");

        assert!(error.to_string().contains("Workflow file not found"));
    }

    #[test]
    fn expands_logs_root_to_absolute_path() {
        let logs_root = std::env::temp_dir().join("symphony-cli-logs-root");
        fs::create_dir_all(&logs_root).expect("temp logs root should be created");
        let cli = Cli::parse_from([
            "symphony",
            "--logs-root",
            logs_root.to_string_lossy().as_ref(),
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
        ]);

        let expanded = cli
            .expanded_logs_root()
            .expect("logs root should expand")
            .expect("logs root should be present");

        let _ = fs::remove_dir_all(&logs_root);

        assert!(expanded.is_absolute());
    }
}
