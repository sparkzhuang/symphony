use chrono::{DateTime, Utc};
use symphony_rust::prompt::PromptBuilder;
use symphony_rust::types::{BlockerRef, Issue, IssueId, IssueIdentifier, WorkflowDefinition};

fn sample_issue(description: Option<&str>) -> Issue {
    Issue {
        id: IssueId::new("issue-1"),
        identifier: IssueIdentifier::new("SPA-13"),
        title: "Prompt Renderer".to_owned(),
        description: description.map(str::to_owned),
        priority: Some(2),
        state: "In Progress".to_owned(),
        branch_name: Some("feature/SPA-13".to_owned()),
        url: Some("https://linear.app/sparkzhuang/issue/SPA-13".to_owned()),
        labels: vec!["module-6".to_owned(), "rust".to_owned()],
        blocked_by: vec![BlockerRef {
            id: Some(IssueId::new("dep-1")),
            identifier: Some(IssueIdentifier::new("SPA-12")),
            state: Some("Done".to_owned()),
        }],
        created_at: Some(parse_datetime("2026-03-25T08:30:00Z")),
        updated_at: Some(parse_datetime("2026-03-26T10:45:00Z")),
    }
}

fn parse_datetime(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test datetime should parse")
        .with_timezone(&Utc)
}

fn workflow(prompt_template: &str) -> WorkflowDefinition {
    WorkflowDefinition::new(serde_json::json!({}), prompt_template)
}

#[test]
fn renders_issue_fields_and_nested_data() {
    let issue = sample_issue(Some("Render Liquid templates"));
    let workflow = workflow(
        concat!(
            "{{ issue.identifier }}|",
            "{{ issue.title }}|",
            "{{ issue.description }}|",
            "{{ issue.priority }}|",
            "{{ issue.state }}|",
            "{{ issue.branch_name }}|",
            "{{ issue.url }}|",
            "{{ issue.created_at }}|",
            "{{ issue.updated_at }}|",
            "{% for label in issue.labels %}{{ label }}{% unless forloop.last %},{% endunless %}{% endfor %}|",
            "{% for blocker in issue.blocked_by %}{{ blocker.identifier }}:{{ blocker.state }}{% endfor %}"
        ),
    );

    let rendered = PromptBuilder::new(&workflow)
        .expect("template should parse")
        .render(&issue, None)
        .expect("prompt should render");

    assert_eq!(
        rendered,
        "SPA-13|Prompt Renderer|Render Liquid templates|2|In Progress|feature/SPA-13|https://linear.app/sparkzhuang/issue/SPA-13|2026-03-25T08:30:00Z|2026-03-26T10:45:00Z|module-6,rust|SPA-12:Done"
    );
}

#[test]
fn supports_conditional_logic_for_optional_description() {
    let workflow =
        workflow("{% if issue.description %}has description{% else %}missing{% endif %}");

    let rendered = PromptBuilder::new(&workflow)
        .expect("template should parse")
        .render(&sample_issue(None), None)
        .expect("prompt should render");

    assert_eq!(rendered, "missing");
}

#[test]
fn renders_attempt_as_nil_on_first_run_and_integer_on_retry() {
    let workflow = workflow("{% if attempt %}retry={{ attempt }}{% else %}retry=first{% endif %}");
    let builder = PromptBuilder::new(&workflow).expect("template should parse");

    let first_run = builder
        .render(&sample_issue(Some("description")), None)
        .expect("first render should succeed");
    let retry = builder
        .render(&sample_issue(Some("description")), Some(3))
        .expect("retry render should succeed");

    assert_eq!(first_run, "retry=first");
    assert_eq!(retry, "retry=3");
}

#[test]
fn errors_on_unknown_variables() {
    let issue = sample_issue(Some("description"));
    let workflow = workflow("{{ issue.missing_field }}");
    let builder = PromptBuilder::new(&workflow).expect("template should parse");

    let error = builder
        .render(&issue, None)
        .expect_err("unknown variable should fail rendering");

    assert!(error.to_string().contains("missing_field"));
}

#[test]
fn errors_on_unknown_filters() {
    let workflow = workflow("{{ issue.title | does_not_exist }}");

    let error = PromptBuilder::new(&workflow).expect_err("unknown filter should fail parsing");

    assert!(error.to_string().contains("does_not_exist"));
}
