use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "symphony",
    about = "Rust implementation of the Symphony service"
)]
pub struct Cli {
    /// Path to the repo-owned WORKFLOW.md file.
    #[arg(value_name = "path-to-WORKFLOW.md", default_value = "./WORKFLOW.md")]
    pub workflow_path: PathBuf,

    /// Override the optional HTTP status server port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Root directory for log output files.
    #[arg(long, value_name = "path")]
    pub logs_root: Option<PathBuf>,

    /// Required acknowledgement for running without the usual guardrails.
    #[arg(
        long = "i-understand-that-this-will-be-running-without-the-usual-guardrails",
        default_value_t = false
    )]
    pub guardrails_acknowledged: bool,
}

impl Cli {
    pub fn workflow_path(&self) -> PathBuf {
        self.workflow_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn defaults_to_repo_workflow_path() {
        let cli = Cli::parse_from(["symphony"]);

        assert_eq!(cli.workflow_path, PathBuf::from("./WORKFLOW.md"));
        assert_eq!(cli.port, None);
        assert_eq!(cli.logs_root, None);
        assert!(!cli.guardrails_acknowledged);
    }

    #[test]
    fn parses_overrides_and_acknowledgement_flag() {
        let cli = Cli::parse_from([
            "symphony",
            "--port",
            "4000",
            "--logs-root",
            "/tmp/symphony-logs",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
            "/repo/WORKFLOW.md",
        ]);

        assert_eq!(cli.workflow_path, PathBuf::from("/repo/WORKFLOW.md"));
        assert_eq!(cli.port, Some(4000));
        assert_eq!(cli.logs_root, Some(PathBuf::from("/tmp/symphony-logs")));
        assert!(cli.guardrails_acknowledged);
    }
}
