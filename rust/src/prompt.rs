use std::fmt;

use liquid::model::Value;
use liquid::{Object, Parser, Template};
use thiserror::Error;

use crate::types::{Issue, WorkflowDefinition};

/// Builds agent prompts from workflow Liquid templates and issue data.
pub struct PromptBuilder {
    template: Template,
}

impl fmt::Debug for PromptBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptBuilder")
            .field("template", &"<parsed>")
            .finish()
    }
}

impl PromptBuilder {
    /// Parses the workflow prompt template once and reuses it across renders.
    pub fn new(workflow: &WorkflowDefinition) -> Result<Self, PromptError> {
        let parser = parser()?;
        let template = parser
            .parse(&workflow.prompt_template)
            .map_err(PromptError::TemplateParse)?;

        Ok(Self { template })
    }

    /// Renders a prompt for an issue and optional retry/continuation attempt number.
    pub fn render(&self, issue: &Issue, attempt: Option<u32>) -> Result<String, PromptError> {
        let mut globals = Object::new();
        let issue_object = liquid::to_object(issue).map_err(PromptError::IssueSerialization)?;

        globals.insert("issue".into(), Value::Object(issue_object));
        globals.insert(
            "attempt".into(),
            attempt.map(Value::scalar).unwrap_or(Value::Nil),
        );

        self.template.render(&globals).map_err(PromptError::Render)
    }
}

fn parser() -> Result<Parser, PromptError> {
    liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(PromptError::ParserBuild)
}

/// Errors produced while preparing or rendering a prompt template.
#[derive(Debug, Error)]
pub enum PromptError {
    #[error("failed to build Liquid parser: {0}")]
    ParserBuild(liquid::Error),
    #[error("failed to parse prompt template: {0}")]
    TemplateParse(liquid::Error),
    #[error("failed to serialize issue for prompt rendering: {0}")]
    IssueSerialization(liquid::Error),
    #[error("failed to render prompt template: {0}")]
    Render(liquid::Error),
}
