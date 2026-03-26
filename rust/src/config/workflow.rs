use std::fs as stdfs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::time::{self, Duration};

use crate::config::schema::{ConfigError, WorkflowConfig};
use crate::types::WorkflowDefinition;

pub const WORKFLOW_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowSnapshot {
    pub workflow: WorkflowDefinition,
    pub config: WorkflowConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStamp {
    pub modified: SystemTime,
    pub size: u64,
    pub content_hash: [u8; 32],
}

#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    stamp: WorkflowStamp,
    current: WorkflowSnapshot,
}

impl WorkflowStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, WorkflowError> {
        let path = path.into();
        let snapshot = load_snapshot_sync(&path)?;
        let stamp = workflow_stamp_sync(&path)?;

        Ok(Self {
            path,
            stamp,
            current: snapshot,
        })
    }

    pub fn current(&self) -> &WorkflowSnapshot {
        &self.current
    }

    pub fn reload_if_changed(&mut self) -> Result<(), WorkflowError> {
        let stamp = workflow_stamp_sync(&self.path)?;
        if stamp == self.stamp {
            return Ok(());
        }

        match load_snapshot_sync(&self.path) {
            Ok(snapshot) => {
                self.stamp = stamp;
                self.current = snapshot;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn reload_if_changed_async(&mut self) -> Result<(), WorkflowError> {
        let stamp = workflow_stamp(&self.path).await?;
        if stamp == self.stamp {
            return Ok(());
        }

        match load_snapshot(&self.path).await {
            Ok(snapshot) => {
                self.stamp = stamp;
                self.current = snapshot;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn watch(mut self) {
        let mut interval = time::interval(WORKFLOW_POLL_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if self.reload_if_changed_async().await.is_err() {
                continue;
            }
        }
    }
}

pub fn parse_workflow(content: &str) -> Result<WorkflowDefinition, WorkflowError> {
    let (front_matter, prompt_lines) = split_front_matter(content);
    let config = parse_front_matter(&front_matter)?;
    let prompt_template = prompt_lines.join("\n").trim().to_owned();

    Ok(WorkflowDefinition::new(
        Value::Object(config),
        prompt_template,
    ))
}

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("missing workflow file at {path}: {source}")]
    MissingFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workflow yaml front matter must decode to an object")]
    FrontMatterNotAMap,
    #[error("workflow parse error: {message}")]
    Parse {
        message: String,
        line: Option<usize>,
        column: Option<usize>,
    },
    #[error(transparent)]
    InvalidConfig(#[from] ConfigError),
}

async fn load_snapshot(path: &Path) -> Result<WorkflowSnapshot, WorkflowError> {
    let content = fs::read_to_string(path)
        .await
        .map_err(|source| WorkflowError::MissingFile {
            path: path.to_path_buf(),
            source,
        })?;
    let workflow = parse_workflow(&content)?;
    let config = WorkflowConfig::from_value(workflow.config.clone())?;

    Ok(WorkflowSnapshot { workflow, config })
}

fn load_snapshot_sync(path: &Path) -> Result<WorkflowSnapshot, WorkflowError> {
    let content = stdfs::read_to_string(path).map_err(|source| WorkflowError::MissingFile {
        path: path.to_path_buf(),
        source,
    })?;
    let workflow = parse_workflow(&content)?;
    let config = WorkflowConfig::from_value(workflow.config.clone())?;

    Ok(WorkflowSnapshot { workflow, config })
}

async fn workflow_stamp(path: &Path) -> Result<WorkflowStamp, WorkflowError> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|source| WorkflowError::MissingFile {
            path: path.to_path_buf(),
            source,
        })?;
    let modified = metadata
        .modified()
        .map_err(|source| WorkflowError::MissingFile {
            path: path.to_path_buf(),
            source: std::io::Error::other(source),
        })?;
    let content = fs::read(path)
        .await
        .map_err(|source| WorkflowError::MissingFile {
            path: path.to_path_buf(),
            source,
        })?;
    let digest = Sha256::digest(content);
    let mut content_hash = [0_u8; 32];
    content_hash.copy_from_slice(&digest);

    Ok(WorkflowStamp {
        modified,
        size: metadata.len(),
        content_hash,
    })
}

fn workflow_stamp_sync(path: &Path) -> Result<WorkflowStamp, WorkflowError> {
    let metadata = stdfs::metadata(path).map_err(|source| WorkflowError::MissingFile {
        path: path.to_path_buf(),
        source,
    })?;
    let modified = metadata
        .modified()
        .map_err(|source| WorkflowError::MissingFile {
            path: path.to_path_buf(),
            source: std::io::Error::other(source),
        })?;
    let content = stdfs::read(path).map_err(|source| WorkflowError::MissingFile {
        path: path.to_path_buf(),
        source,
    })?;
    let digest = Sha256::digest(content);
    let mut content_hash = [0_u8; 32];
    content_hash.copy_from_slice(&digest);

    Ok(WorkflowStamp {
        modified,
        size: metadata.len(),
        content_hash,
    })
}

fn split_front_matter(content: &str) -> (String, Vec<&str>) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().is_none_or(|line| *line != "---") {
        return (String::new(), lines);
    }

    let mut front_matter = Vec::new();
    for (index, line) in lines.iter().enumerate().skip(1) {
        if *line == "---" {
            let prompt_lines = lines.into_iter().skip(index + 1).collect();
            return (front_matter.join("\n"), prompt_lines);
        }

        front_matter.push(*line);
    }

    (front_matter.join("\n"), Vec::new())
}

fn parse_front_matter(front_matter: &str) -> Result<Map<String, Value>, WorkflowError> {
    let trimmed = front_matter.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }

    let parsed: serde_yaml::Value =
        serde_yaml::from_str(trimmed).map_err(|error| WorkflowError::Parse {
            message: error.to_string(),
            line: error.location().map(|location| location.line()),
            column: error.location().map(|location| location.column()),
        })?;

    let serde_yaml::Value::Mapping(mapping) = parsed else {
        return Err(WorkflowError::FrontMatterNotAMap);
    };

    let json_value = serde_json::to_value(mapping).map_err(|error| WorkflowError::Parse {
        message: error.to_string(),
        line: None,
        column: None,
    })?;

    match json_value {
        Value::Object(object) => Ok(object),
        _ => Err(WorkflowError::FrontMatterNotAMap),
    }
}
