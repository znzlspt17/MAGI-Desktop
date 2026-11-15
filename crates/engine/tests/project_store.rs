use domain::{Command, Request, ResponseData};
use engine::Engine;
use redb::{Database, TableDefinition};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("magi-project-store-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // 이 테스트가 직접 만든 고정 이름만 정리하고 재귀 삭제는 하지 않는다.
        for database in ["state.redb", "missing.redb", "future.redb"] {
            let journal = self.0.join(format!("{database}.completion-journal"));
            let _ = fs::remove_file(journal.join("fake-pending.json"));
            let _ = fs::remove_dir(journal);
            let _ = fs::remove_file(self.0.join(database));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
fn completion_journal_prevents_opening_or_modifying_existing_database() {
    let dir = TestDirectory::new();
    let path = dir.path().join("state.redb");
    {
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("schema_version", 1).unwrap();
            tx.open_table(PROJECTS).unwrap();
        }
        tx.commit().unwrap();
    }
    let original_database = fs::read(&path).unwrap();
    let journal = dir.path().join("state.redb.completion-journal");
    fs::create_dir(&journal).unwrap();
    let journal_record = journal.join("fake-pending.json");
    fs::write(&journal_record, b"fake pending record").unwrap();

    let engine = Engine::open(&path).unwrap();
    let status = call(&engine, "status", Command::ListStorageFailures).unwrap();
    assert!(
        matches!(status,ResponseData::Data(value) if !value["failures"].as_array().unwrap().is_empty())
    );
    drop(engine);
    assert_eq!(fs::read(&path).unwrap(), original_database);
    assert_eq!(fs::read(&journal_record).unwrap(), b"fake pending record");
}

#[test]
fn completion_journal_prevents_creating_a_missing_database() {
    let dir = TestDirectory::new();
    let path = dir.path().join("missing.redb");
    fs::create_dir(dir.path().join("missing.redb.completion-journal")).unwrap();

    let engine = Engine::open(&path).unwrap();
    let status = call(&engine, "status", Command::ListStorageFailures).unwrap();
    assert!(
        matches!(status,ResponseData::Data(value) if !value["failures"].as_array().unwrap().is_empty())
    );
    assert!(!path.exists(), "a missing database must not be recreated");
}

fn call(engine: &Engine, id: &str, command: Command) -> Result<ResponseData, String> {
    let response = engine.handle(Request {
        id: id.into(),
        command,
    });
    assert_eq!(response.id, id);
    response.result
}

#[test]
fn project_creation_rejects_blank_input_without_changing_the_store() {
    let dir = TestDirectory::new();
    let engine = Engine::open(&dir.path().join("state.redb")).unwrap();
    for (name, path) in [("  ", "project"), ("project", "\t")] {
        let error = call(
            &engine,
            "invalid",
            Command::CreateProject {
                name: name.into(),
                path: path.into(),
            },
        )
        .unwrap_err();
        assert_eq!(error, "project name and path required");
    }
    match call(&engine, "list", Command::ListProjects).unwrap() {
        ResponseData::Projects(projects) => assert!(projects.is_empty()),
        _ => panic!("expected project list"),
    }
}

#[tokio::test]
async fn projects_keep_original_contract_and_survive_reopening() {
    let dir = TestDirectory::new();
    let database = dir.path().join("state.redb");
    let project_path = dir.path().join("아직 없는 프로젝트");
    let project_path_string = project_path.to_string_lossy().into_owned();
    let engine = std::sync::Arc::new(Engine::open(&database).unwrap());

    let first = match std::sync::Arc::clone(&engine)
        .handle_async(Request {
            id: "create-first".into(),
            command: Command::CreateProject {
                name: "  첫 번째  ".into(),
                path: project_path_string.clone(),
            },
        })
        .await
    {
        response if response.id == "create-first" => match response.result.unwrap() {
            ResponseData::Project(project) => project,
            _ => panic!("expected created project"),
        },
        _ => panic!("request ID was not preserved"),
    };
    assert_eq!(first.name, "  첫 번째  ");
    assert_eq!(first.path, project_path_string);
    assert!(
        !project_path.exists(),
        "project creation must not create the path"
    );
    assert!(uuid::Uuid::parse_str(&first.id).is_ok());
    assert_eq!(first.autonomy_mode, "step");
    assert!(
        first.assessment.is_none() && first.approved.is_none() && first.model_settings.is_none()
    );
    assert_eq!(first.approval_version, 0);
    assert_eq!(first.context_version, 0);
    assert!(first.interview.is_empty() && first.decisions.is_empty());
    assert!(first.approval_history.is_empty() && first.agent_configs.is_empty());

    // 명세 초안의 필드 순서를 포함하는 기존 직렬화 계약을 고정한다.
    let default_draft_json = br#"{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":"","markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]}"#;
    assert_eq!(
        serde_json::to_vec(&first.draft).unwrap(),
        default_draft_json
    );
    assert_eq!(
        first.draft_hash,
        format!("{:x}", Sha256::digest(default_draft_json))
    );

    let second = match call(
        &engine,
        "create-second",
        Command::CreateProject {
            name: "두 번째".into(),
            path: "nonexistent".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected created project"),
    };
    assert_ne!(first.id, second.id);
    let mut expected_ids = vec![first.id.clone(), second.id.clone()];
    expected_ids.sort();
    let list = match call(&engine, "list", Command::ListProjects).unwrap() {
        ResponseData::Projects(projects) => projects,
        _ => panic!("expected project list"),
    };
    assert_eq!(
        list.iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>(),
        expected_ids
    );
    assert_eq!(
        call(
            &engine,
            "missing",
            Command::GetProject {
                project_id: "missing".into()
            }
        )
        .unwrap_err(),
        "project not found"
    );
    assert_eq!(
        call(
            &engine,
            "unsupported",
            Command::GenerateSpec {
                project_id: first.id.clone()
            }
        )
        .unwrap_err(),
        "requested feature is not available"
    );

    drop(engine);
    let reopened = Engine::open(&database).unwrap();
    let saved = match call(
        &reopened,
        "get",
        Command::GetProject {
            project_id: first.id.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected saved project"),
    };
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(first).unwrap()
    );
}

#[test]
fn future_storage_schema_is_rejected_without_modifying_the_file() {
    let dir = TestDirectory::new();
    let path = dir.path().join("future.redb");
    {
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("schema_version", 999).unwrap();
        }
        tx.commit().unwrap();
    }
    let before = fs::read(&path).unwrap();
    assert!(Engine::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}
