use std::sync::Arc;

use thiserror::Error;

use crate::agent::app_server::{AppServerClient, AppServerConfig, AppServerError, AppServerEvent};
use crate::agent::tools::NonInteractiveDynamicToolExecutor;
use crate::config::{HooksConfig, TrackerConfig, WorkflowConfig};
use crate::prompt::{PromptBuilder, PromptError};
use crate::tracker::{Tracker, TrackerError};
use crate::types::{Issue, WorkflowDefinition, Workspace};
use crate::workspace::{HookName, WorkspaceError, WorkspaceManager};

type EventSink = Arc<dyn Fn(AppServerEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRunResult {
    pub workspace: Workspace,
    pub turn_count: u32,
}

pub struct AgentRunner {
    config: WorkflowConfig,
    tracker: Arc<dyn Tracker>,
    prompt_builder: PromptBuilder,
    workspace_manager: WorkspaceManager,
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRunner")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AgentRunner {
    pub fn new(
        workflow: WorkflowDefinition,
        config: WorkflowConfig,
        tracker: Arc<dyn Tracker>,
    ) -> Result<Self, AgentRunnerError> {
        let prompt_builder = PromptBuilder::new(&workflow)?;
        let workspace_manager =
            WorkspaceManager::new(&config.workspace.root, workspace_hooks(&config.hooks))?;

        Ok(Self {
            config,
            tracker,
            prompt_builder,
            workspace_manager,
        })
    }

    pub async fn run(
        &self,
        mut issue: Issue,
        sink: Option<EventSink>,
    ) -> Result<AgentRunResult, AgentRunnerError> {
        let workspace = self
            .workspace_manager
            .create_for_issue(issue.identifier.as_str(), None)
            .await?;

        let result = async {
            self.workspace_manager
                .run_hook(
                    HookName::BeforeRun,
                    &workspace.path,
                    issue.identifier.as_str(),
                    None,
                )
                .await?;

            let executor = NonInteractiveDynamicToolExecutor::new(self.config.tracker.clone());
            let mut client = AppServerClient::new(
                AppServerConfig::from_codex_config(&self.config.codex),
                self.config.tracker.clone(),
                executor,
            );
            let mut session = client.start_session(&workspace.path, sink).await?;
            let result = async {
                let mut turn_count = 0_u32;

                loop {
                    let prompt = if turn_count == 0 {
                        self.prompt_builder.render(&issue, None)?
                    } else {
                        continuation_guidance(turn_count + 1, self.config.agent.max_turns)
                    };

                    client.run_turn(&mut session, &prompt, &issue).await?;
                    turn_count += 1;

                    if turn_count >= self.config.agent.max_turns {
                        break;
                    }

                    let refreshed = self
                        .tracker
                        .fetch_issue_states_by_ids(&[issue.id.as_str().to_owned()])
                        .await?;
                    let Some(next_issue) = refreshed.into_iter().next() else {
                        break;
                    };

                    if !active_state(&next_issue.state, &self.config.tracker) {
                        break;
                    }

                    issue = next_issue;
                }

                Ok(AgentRunResult {
                    workspace: workspace.clone(),
                    turn_count,
                })
            }
            .await;

            let stop_result = client.stop_session(&mut session).await;
            match (result, stop_result) {
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error.into()),
                (Ok(result), Ok(())) => Ok(result),
            }
        }
        .await;

        self.workspace_manager
            .run_after_run_hook(&workspace.path, issue.identifier.as_str(), None)
            .await?;

        result
    }
}

#[derive(Debug, Error)]
pub enum AgentRunnerError {
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error(transparent)]
    Tracker(#[from] TrackerError),
}

fn workspace_hooks(config: &HooksConfig) -> crate::types::WorkspaceHooks {
    crate::types::WorkspaceHooks {
        after_create: config.after_create.clone(),
        before_run: config.before_run.clone(),
        after_run: config.after_run.clone(),
        before_remove: config.before_remove.clone(),
        timeout_ms: Some(config.timeout_ms),
    }
}

fn active_state(state: &str, tracker: &TrackerConfig) -> bool {
    let current = state.trim().to_ascii_lowercase();
    tracker
        .active_states
        .iter()
        .any(|configured| configured.trim().to_ascii_lowercase() == current)
}

fn continuation_guidance(turn_number: u32, max_turns: u32) -> String {
    format!(
        "Continuation guidance:\n\n- The previous Codex turn completed normally, but the Linear issue is still in an active state.\n- This is continuation turn #{turn_number} of {max_turns} for the current agent run.\n- Resume from the current workspace and workpad state instead of restarting from scratch.\n- The original task instructions and prior turn context are already present in this thread, so do not restate them before acting.\n- Focus on the remaining ticket work and do not end the turn while the issue stays active unless you are truly blocked.\n"
    )
}
