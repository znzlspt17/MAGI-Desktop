use domain::{
    ApprovedSpec, Command, Project, Request, ResponseData, SpecDraft, TaskBinding, TaskSpec,
};
use engine::Engine;
use redb::{Database, ReadableTable, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
};

const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("magi-approval-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("state.redb");
        Self { directory, path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn call(engine: &Engine, id: &str, command: Command) -> Result<ResponseData, String> {
    let response = engine.handle(Request {
        id: id.into(),
        command,
    });
    assert_eq!(response.id, id);
    response.result
}

fn saved_project(engine: &Engine, draft: SpecDraft) -> Project {
    let project = match call(
        engine,
        "create",
        Command::CreateProject {
            name: "가짜 프로젝트".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected project"),
    };
    match call(
        engine,
        "save",
        Command::SaveDraft {
            project_id: project.id,
            draft,
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected project"),
    }
}

fn valid_draft() -> SpecDraft {
    let markdown = provider::sdd::HEADINGS
        .iter()
        .enumerate()
        .map(|(i, heading)| {
            if i == 4 {
                format!("{heading}\n")
            } else {
                format!("{heading}\n내용\n")
            }
        })
        .collect::<String>();
    SpecDraft {
        title: "승인할 문서".into(),
        scope: vec!["src".into()],
        requirements: vec!["요구".into()],
        acceptance: vec!["인수".into()],
        mandatory_questions: vec![],
        required_failures: vec![],
        optional_issues: vec![],
        base_revision: "기준 revision".into(),
        markdown,
        source_links: vec!["source-1".into()],
        constraints: vec!["제약".into()],
        tasks: vec![TaskSpec {
            id: "task-1".into(),
            dependencies: vec![],
            owned_paths: vec!["src".into()],
            validation: vec!["read_contains:src/file.txt:내용".into()],
        }],
        reused_decisions: vec!["사용자가 선택한 결정".into()],
        task_bindings: vec![TaskBinding {
            task_id: "task-1".into(),
            goal: "작업 목표".into(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        }],
    }
}

fn approve(
    engine: &Engine,
    project: &Project,
    accepted_optional: Vec<String>,
) -> Result<ApprovedSpec, String> {
    match call(
        engine,
        "approve",
        Command::ApproveSpec {
            project_id: project.id.clone(),
            expected_draft_hash: project.draft_hash.clone(),
            accepted_optional,
        },
    )? {
        ResponseData::Approved(spec) => Ok(spec),
        _ => panic!("expected approved specification"),
    }
}

// 아직 평가 생성 경로가 없어 가짜 DB에 평가 참조만 저장한다.
fn set_assessment(
    path: &Path,
    id: &str,
    hash: &str,
    assessment_version: u64,
    context_version: u64,
) {
    let db = Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(PROJECTS).unwrap();
        let mut project: serde_json::Value = {
            let value = table.get(id).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        project["assessment"] =
            serde_json::json!({"draft_hash": hash, "context_version": assessment_version});
        project["context_version"] = serde_json::json!(context_version);
        let encoded = project.to_string();
        table.insert(id, encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
}

fn get_project(engine: &Engine, id: &str) -> Project {
    match call(
        engine,
        "get",
        Command::GetProject {
            project_id: id.into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected project"),
    }
}

#[test]
fn approval_versions_preserve_the_complete_draft_and_history() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let mut draft = valid_draft();
    draft.optional_issues.push("선택 이슈".into());
    let project = saved_project(&engine, draft.clone());
    let first = approve(&engine, &project, vec!["선택 이슈".into()]).unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(first.hash, project.draft_hash);
    assert_eq!(first.base_revision, draft.base_revision);
    assert_eq!(first.scope, draft.scope);
    assert_eq!(first.requirements, draft.requirements);
    assert_eq!(first.acceptance, draft.acceptance);
    assert_eq!(first.accepted_optional, vec!["선택 이슈"]);
    assert_eq!(first.markdown, draft.markdown);
    assert_eq!(first.source_links, draft.source_links);
    assert_eq!(first.constraints, draft.constraints);
    assert_eq!(first.reused_decisions, draft.reused_decisions);
    assert_eq!(
        serde_json::to_value(&first.tasks).unwrap(),
        serde_json::to_value(&draft.tasks).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&first.task_bindings).unwrap(),
        serde_json::to_value(&draft.task_bindings).unwrap()
    );
    let second = approve(&engine, &project, vec!["선택 이슈".into()]).unwrap();
    assert_eq!(second.version, 2);
    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    let saved = get_project(&reopened, &project.id);
    assert_eq!(saved.approval_version, 2);
    assert_eq!(saved.approval_history.len(), 2);
    assert_eq!(
        serde_json::to_value(&saved.approval_history[0]).unwrap(),
        serde_json::to_value(first).unwrap()
    );
    assert_eq!(
        serde_json::to_value(saved.approved).unwrap(),
        serde_json::to_value(second).unwrap()
    );
}

#[test]
fn stale_reviews_and_required_items_block_approval_in_order() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let mut draft = valid_draft();
    draft.mandatory_questions.push("질문".into());
    let project = saved_project(&engine, draft);
    let result = call(
        &engine,
        "stale",
        Command::ApproveSpec {
            project_id: project.id.clone(),
            expected_draft_hash: "old".into(),
            accepted_optional: vec![],
        },
    );
    assert_eq!(result.unwrap_err(), "draft changed since review");
    drop(engine);
    set_assessment(&fixture.path, &project.id, "old", 0, 0);
    let engine = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "draft requires a new assessment"
    );
    drop(engine);
    set_assessment(&fixture.path, &project.id, &project.draft_hash, 0, 0);
    let engine = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "mandatory questions or required checks remain unresolved"
    );
    let saved = get_project(&engine, &project.id);
    assert!(saved.approved.is_none());
    assert!(saved.approval_history.is_empty());
    assert!(saved.assessment.is_some());

    let mut draft = valid_draft();
    draft.required_failures.push("필수 검사 실패".into());
    let project = saved_project(&engine, draft);
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "mandatory questions or required checks remain unresolved"
    );
}

#[test]
fn structure_and_optional_issues_are_checked_before_approval() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let mut draft = valid_draft();
    draft.title = " ".into();
    let project = saved_project(&engine, draft);
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "title, base revision, scope, requirements and acceptance are required"
    );
    let mut draft = valid_draft();
    draft.markdown.clear();
    let project = saved_project(&engine, draft);
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "incomplete specification structure"
    );
    let mut draft = valid_draft();
    draft.optional_issues.push("선택 이슈".into());
    let project = saved_project(&engine, draft);
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "optional issues require explicit acceptance"
    );
    assert!(approve(&engine, &project, vec!["선택 이슈".into()]).is_ok());
}

#[test]
fn approval_checks_lexical_windows_paths_and_approved_scope() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    for path in [
        "src/.env",
        "src/.env.example",
        "src/.git/config",
        "src/.config/gcloud/a",
        "src/credentials.json",
        "src/private.key",
        "src/CON",
        "src/COM¹.log",
        "src/LPT3",
        "src/a:stream",
        "src/name.",
        "src/name ",
        "src/../file",
        "/absolute",
    ] {
        let mut draft = valid_draft();
        draft.tasks[0].owned_paths = vec![path.into(), "src/normal".into()];
        draft.tasks[0].validation = vec!["read_contains:src/normal/file:works".into()];
        draft.optional_issues.push("선택".into());
        let project = saved_project(&engine, draft);
        assert_eq!(
            approve(&engine, &project, vec![]).unwrap_err(),
            "unsafe task path",
            "{path}"
        );
    }
    let mut draft = valid_draft();
    draft.tasks[0].owned_paths = vec!["outside".into(), "src/normal".into()];
    draft.tasks[0].validation = vec!["read_contains:src/normal/file:works".into()];
    let project = saved_project(&engine, draft);
    assert_eq!(
        approve(&engine, &project, vec![]).unwrap_err(),
        "task path outside approved scope"
    );
    let mut draft = valid_draft();
    draft.tasks[0].owned_paths = vec!["src/config/private_schema.json".into(), "src/normal".into()];
    draft.tasks[0].validation = vec!["read_contains:src/normal/file:works".into()];
    let project = saved_project(&engine, draft);
    assert!(approve(&engine, &project, vec![]).is_ok());
}

#[test]
fn approval_rejects_missing_or_cyclic_dependencies() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    for dependency in ["missing", "task-1"] {
        let mut draft = valid_draft();
        draft.tasks[0].dependencies.push(dependency.into());
        let project = saved_project(&engine, draft);
        assert_eq!(
            approve(&engine, &project, vec![]).unwrap_err(),
            "task dependencies missing or cyclic"
        );
    }
}

#[test]
fn exhausted_context_rejects_approval_without_changing_project() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let project = saved_project(&engine, valid_draft());
    drop(engine);
    set_assessment(
        &fixture.path,
        &project.id,
        &project.draft_hash,
        u64::MAX,
        u64::MAX,
    );
    let engine = Engine::open(&fixture.path).unwrap();
    let before = get_project(&engine, &project.id);
    assert_eq!(
        approve(&engine, &before, vec![]).unwrap_err(),
        "interview context version exhausted"
    );
    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    let after = get_project(&reopened, &project.id);
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[test]
fn changed_normal_context_requires_a_new_assessment() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let project = saved_project(&engine, valid_draft());
    drop(engine);
    set_assessment(&fixture.path, &project.id, &project.draft_hash, 0, 0);
    let engine = Engine::open(&fixture.path).unwrap();
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "새 입력".into(),
        },
    )
    .unwrap();
    let saved = get_project(&engine, &project.id);
    assert_eq!(
        approve(&engine, &saved, vec![]).unwrap_err(),
        "draft requires a new assessment"
    );
}
