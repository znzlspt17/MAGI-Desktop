use domain::{Command, Project, Request, ResponseData, SpecDraft, TaskBinding, TaskSpec};
use engine::Engine;
use redb::{Database, TableDefinition};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("magi-interview-{}", uuid::Uuid::new_v4()));
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

fn project(response: ResponseData) -> Project {
    match response {
        ResponseData::Project(project) => project,
        _ => panic!("expected a project"),
    }
}

// 연결 전인 승인 흐름을 실행하지 않고 가짜 저장 데이터로만 준비한다.
fn seed_approved_project(path: &std::path::Path, context_version: u64) -> Project {
    let engine = Engine::open(path).unwrap();
    let original = project(
        call(
            &engine,
            "create",
            Command::CreateProject {
                name: "프로젝트".into(),
                path: "not-created".into(),
            },
        )
        .unwrap(),
    );
    drop(engine);

    let mut value = serde_json::to_value(original).unwrap();
    let draft_hash = value["draft_hash"].as_str().unwrap().to_owned();
    let approved = serde_json::json!({
        "version": 1, "hash": draft_hash, "base_revision": "fixture-revision",
        "scope": ["src"], "requirements": ["요구"], "acceptance": ["인수"],
        "accepted_optional": [], "markdown": "승인된 원문", "source_links": [],
        "constraints": [], "tasks": [], "reused_decisions": [], "task_bindings": []
    });
    value["approved"] = approved.clone();
    value["approval_history"] = serde_json::json!([approved]);
    value["approval_version"] = serde_json::json!(1);
    value["context_version"] = serde_json::json!(context_version);
    value["assessment"] =
        serde_json::json!({"draft_hash": draft_hash, "context_version": context_version});
    let seeded: Project = serde_json::from_value(value).unwrap();
    let db = Database::open(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(PROJECTS).unwrap();
        let encoded = serde_json::to_string(&seeded).unwrap();
        table.insert(seeded.id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    seeded
}

fn read_project(engine: &Engine, id: &str) -> Project {
    project(
        call(
            engine,
            "get",
            Command::GetProject {
                project_id: id.into(),
            },
        )
        .unwrap(),
    )
}

#[test]
fn blank_interview_is_rejected_without_changes() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    for message in ["", " \t\r\n"] {
        let error = call(
            &engine,
            "blank",
            Command::SubmitInterview {
                project_id: original.id.clone(),
                message: message.into(),
            },
        )
        .unwrap_err();
        assert_eq!(error, "interview message is empty");
    }
    assert_eq!(
        serde_json::to_value(read_project(&engine, &original.id)).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn interview_preserves_input_and_assessment_but_invalidates_approval() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    let message = format!("  한글 답변\n{}  ", "문".repeat(5_000));
    let updated = project(
        call(
            &engine,
            "answer",
            Command::SubmitInterview {
                project_id: original.id.clone(),
                message: message.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(updated.interview, vec![message]);
    assert_eq!(updated.context_version, 8);
    assert!(updated.approved.is_none());
    assert_eq!(
        serde_json::to_value(&updated.assessment).unwrap(),
        serde_json::to_value(&original.assessment).unwrap()
    );
    assert_ne!(
        updated.assessment.as_ref().unwrap().context_version,
        updated.context_version
    );
    assert_eq!(
        serde_json::to_value(&updated.approval_history).unwrap(),
        serde_json::to_value(&original.approval_history).unwrap()
    );

    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        serde_json::to_value(read_project(&reopened, &original.id)).unwrap(),
        serde_json::to_value(updated).unwrap()
    );
}

#[test]
fn draft_save_preserves_all_fields_and_invalidates_approval() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    let draft = SpecDraft {
        title: "  명세  ".into(),
        scope: vec!["src".into()],
        requirements: vec!["요구".into()],
        acceptance: vec!["인수".into()],
        mandatory_questions: vec!["질문".into()],
        required_failures: vec!["실패".into()],
        optional_issues: vec!["선택".into()],
        base_revision: "revision".into(),
        markdown: "전체 문서\n본문".into(),
        source_links: vec!["source-1".into()],
        constraints: vec!["제약".into()],
        tasks: vec![TaskSpec {
            id: "task-1".into(),
            dependencies: vec![],
            owned_paths: vec!["src".into()],
            validation: vec!["read_contains:src/a:값".into()],
        }],
        reused_decisions: vec!["결정 사본".into()],
        task_bindings: vec![TaskBinding {
            task_id: "task-1".into(),
            goal: "목표".into(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        }],
    };
    let updated = project(
        call(
            &engine,
            "save",
            Command::SaveDraft {
                project_id: original.id.clone(),
                draft: draft.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(
        serde_json::to_value(&updated.draft).unwrap(),
        serde_json::to_value(&draft).unwrap()
    );
    assert_eq!(
        updated.draft_hash,
        format!("{:x}", Sha256::digest(serde_json::to_vec(&draft).unwrap()))
    );
    assert_ne!(updated.draft_hash, original.draft_hash);
    assert_eq!(updated.context_version, original.context_version);
    assert!(updated.approved.is_none());
    assert_eq!(
        serde_json::to_value(&updated.assessment).unwrap(),
        serde_json::to_value(&original.assessment).unwrap()
    );
    assert_ne!(
        updated.assessment.as_ref().unwrap().draft_hash,
        updated.draft_hash
    );
    assert_eq!(
        serde_json::to_value(&updated.approval_history).unwrap(),
        serde_json::to_value(&original.approval_history).unwrap()
    );
    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        serde_json::to_value(read_project(&reopened, &original.id)).unwrap(),
        serde_json::to_value(updated).unwrap()
    );
}

#[test]
fn identical_draft_keeps_assessment_but_clears_approval() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    let updated = project(
        call(
            &engine,
            "same",
            Command::SaveDraft {
                project_id: original.id.clone(),
                draft: original.draft.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(updated.draft_hash, original.draft_hash);
    assert_eq!(updated.context_version, original.context_version);
    assert_eq!(
        serde_json::to_value(updated.assessment).unwrap(),
        serde_json::to_value(original.assessment).unwrap()
    );
    assert!(updated.approved.is_none());
}

#[test]
fn saturated_context_rejects_interview_without_changing_project() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, u64::MAX);
    let engine = Engine::open(&fixture.path).unwrap();
    let result = call(
        &engine,
        "saturated",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "답변".into(),
        },
    );
    assert_eq!(result.unwrap_err(), "interview context version exhausted");
    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    let saved = project(
        call(
            &reopened,
            "get",
            Command::GetProject {
                project_id: original.id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}
#[test]
fn updates_to_unknown_project_are_rejected_without_insertion() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let id = "unknown";
    for command in [
        Command::SubmitInterview {
            project_id: id.into(),
            message: "답변".into(),
        },
        Command::SaveDraft {
            project_id: id.into(),
            draft: SpecDraft::default(),
        },
    ] {
        assert_eq!(
            call(&engine, "missing", command).unwrap_err(),
            "project not found"
        );
    }
    drop(engine);
    let db = Database::open(&fixture.path).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(PROJECTS).unwrap();
    assert!(table.get(id).unwrap().is_none());
}

#[tokio::test]
async fn concurrent_interviews_do_not_lose_either_update() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    let request = |id: &str, message: &str| Request {
        id: id.into(),
        command: Command::SubmitInterview {
            project_id: original.id.clone(),
            message: message.into(),
        },
    };
    let (first, second) = tokio::join!(
        std::sync::Arc::clone(&engine).handle_async(request("first", "첫 답변")),
        std::sync::Arc::clone(&engine).handle_async(request("second", "두 번째 답변")),
    );
    assert!(first.result.is_ok() && second.result.is_ok());
    let saved = read_project(&engine, &original.id);
    assert_eq!(saved.context_version, 9);
    assert_eq!(saved.interview.len(), 2);
    assert!(saved.interview.iter().any(|message| message == "첫 답변"));
    assert!(saved
        .interview
        .iter()
        .any(|message| message == "두 번째 답변"));
}
