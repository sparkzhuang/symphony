use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathSafetyError {
    #[error("failed to canonicalize path `{path}`: {reason}")]
    CanonicalizeFailed { path: PathBuf, reason: String },
    #[error("workspace path `{workspace}` must not equal root `{root}`")]
    WorkspaceEqualsRoot { workspace: PathBuf, root: PathBuf },
    #[error("workspace path `{workspace}` escapes root `{root}` via symlink")]
    SymlinkEscape { workspace: PathBuf, root: PathBuf },
    #[error("workspace path `{workspace}` is outside root `{root}`")]
    WorkspaceOutsideRoot { workspace: PathBuf, root: PathBuf },
}

pub fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, PathSafetyError> {
    let absolute = absolute_path(path).map_err(|error| PathSafetyError::CanonicalizeFailed {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;

    resolve_path(&absolute).map_err(|error| PathSafetyError::CanonicalizeFailed {
        path: absolute,
        reason: error.to_string(),
    })
}

pub fn validate_workspace_path(root: &Path, workspace: &Path) -> Result<PathBuf, PathSafetyError> {
    let canonical_root = canonicalize_allow_missing(root)?;
    let canonical_workspace = canonicalize_allow_missing(workspace)?;
    let absolute_root =
        absolute_path(root).map_err(|error| PathSafetyError::CanonicalizeFailed {
            path: root.to_path_buf(),
            reason: error.to_string(),
        })?;
    let absolute_workspace =
        absolute_path(workspace).map_err(|error| PathSafetyError::CanonicalizeFailed {
            path: workspace.to_path_buf(),
            reason: error.to_string(),
        })?;

    if canonical_workspace == canonical_root {
        return Err(PathSafetyError::WorkspaceEqualsRoot {
            workspace: canonical_workspace,
            root: canonical_root,
        });
    }

    if canonical_workspace.starts_with(&canonical_root) {
        return Ok(canonical_workspace);
    }

    if absolute_workspace.starts_with(&absolute_root) {
        return Err(PathSafetyError::SymlinkEscape {
            workspace: absolute_workspace,
            root: canonical_root,
        });
    }

    Err(PathSafetyError::WorkspaceOutsideRoot {
        workspace: canonical_workspace,
        root: canonical_root,
    })
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut components = path.components().peekable();

    while let Some(component) = components.next() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                resolved.push(component.as_os_str());
            }
        }

        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&resolved)?;
                let base = resolved
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()));
                let mut joined = if target.is_absolute() {
                    target
                } else {
                    base.join(target)
                };

                for remaining in components {
                    joined.push(component_to_os_string(remaining));
                }

                return resolve_path(&joined);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                for remaining in components {
                    resolved.push(component_to_os_string(remaining));
                }

                return Ok(resolved);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(resolved)
}

fn component_to_os_string(component: Component<'_>) -> OsString {
    component.as_os_str().to_os_string()
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_allow_missing, validate_workspace_path, PathSafetyError};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic")
            .as_nanos();

        std::env::temp_dir().join(format!("workspace-safety-{name}-{nanos}"))
    }

    #[test]
    fn canonicalize_preserves_missing_suffix() {
        let root = unique_path("missing-root");
        let nested = root.join("one").join("two");

        let canonical = canonicalize_allow_missing(&nested).expect("path should canonicalize");

        assert!(canonical.ends_with("one/two"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = unique_path("root");
        let outside = unique_path("outside");
        fs::create_dir_all(&root).expect("root should exist");
        fs::create_dir_all(&outside).expect("outside should exist");
        symlink(&outside, root.join("escape")).expect("symlink should be created");

        let error = validate_workspace_path(&root, &root.join("escape"))
            .expect_err("escaped path should be rejected");

        match error {
            PathSafetyError::SymlinkEscape { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }
}
