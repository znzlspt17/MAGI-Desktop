use domain::{Command, Request, ResponseData};
use engine::Engine;
use std::{fs, path::PathBuf};

struct Fixture {
    directory: PathBuf,
    database: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("magi-run-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let database = directory.join("state.redb");
        Self {
            directory,
            database,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.database);
        let _ = fs::remove_dir(&self.directory);
    }
}
fn call(engine: &Engine, command: Command) -> Result<ResponseData, String> {
    engine
        .handle(Request {
            id: "test".into(),
            command,
        })
        .result
}

#[test]
fn unapproved_project_is_rejected_without_creating_repository() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "가짜 프로젝트".into(),
            path: fixture
                .directory
                .join("nonexistent-repo")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected project"),
    };
    let error = call(
        &engine,
        Command::StartRun {
            project_id: project.id,
            expected_approved_hash: "unapproved".into(),
        },
    )
    .unwrap_err();
    assert_eq!(error, "approval required");
    assert!(!fixture.directory.join("nonexistent-repo").exists());
}

#[test]
fn stale_approval_and_missing_or_unsupported_model_stop_before_run_creation() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "가짜".into(),
            path: fixture
                .directory
                .join("repo")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(project) => project,
        _ => panic!("expected project"),
    };
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut value = serde_json::to_value(&project).unwrap();
        value["approved"] = serde_json::json!({"version": 1, "hash": project.draft_hash, "base_revision": "fixture", "scope": ["src"], "requirements": ["r"], "acceptance": ["a"], "accepted_optional": [], "markdown": "", "source_links": [], "constraints": [], "tasks": [], "reused_decisions": [], "task_bindings": []});
        let encoded = value.to_string();
        table.insert(project.id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&fixture.database).unwrap();
    let start = |hash: &str| {
        call(
            &engine,
            Command::StartRun {
                project_id: project.id.clone(),
                expected_approved_hash: hash.into(),
            },
        )
    };
    assert_eq!(
        start("stale").unwrap_err(),
        "approved specification changed"
    );
    assert_eq!(
        start(&project.draft_hash).unwrap_err(),
        "model not configured"
    );
    call(
        &engine,
        Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "local".into(),
            model: "fake".into(),
            capabilities: domain::ModelCapabilities {
                streaming: false,
                tools: false,
                context_tokens: None,
            },
        },
    )
    .unwrap();
    assert_eq!(
        start(&project.draft_hash).unwrap_err(),
        "selected local model is unavailable"
    );
    assert!(!fixture.directory.join("repo").exists());
}

#[test]
fn unsupported_journal_stage_is_reported_and_preserved() {
    let fixture = Fixture::new();
    drop(Engine::open(&fixture.database).unwrap());
    let mut journal = fixture.database.as_os_str().to_os_string();
    journal.push(".completion-journal");
    let journal = PathBuf::from(journal);
    std::fs::create_dir(&journal).unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    let file = journal.join(format!("{run_id}.json"));
    let bytes=serde_json::to_vec(&serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":uuid::Uuid::new_v4().to_string(),"outcome":{"stage":"agent","result":{"Err":"fixture"}}})).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let status = call(&engine, Command::ListStorageFailures).unwrap();
    let data = match status {
        ResponseData::Data(value) => value,
        _ => panic!("expected diagnostic"),
    };
    assert!(!data["failures"].as_array().unwrap().is_empty());
    let result = call(
        &engine,
        Command::CreateProject {
            name: "blocked".into(),
            path: "not-created".into(),
        },
    );
    assert_eq!(
        result.unwrap_err(),
        "완료 결과 저장을 먼저 복구해야 합니다."
    );
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    drop(engine);
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn existing_worktree_journal_is_applied_before_interrupted_run_recovery() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "가짜".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("expected project"),
    };
    drop(engine);
    let run_id = uuid::Uuid::new_v4().to_string();
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let run = serde_json::json!({"id":run_id,"project_id":project.id,"status":"preparing","events":[]});
        let encoded = run.to_string();
        table.insert(run_id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let path = journal.join(format!("{run_id}.json"));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project.id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let result = call(
        &engine,
        Command::GetRun {
            run_id: run_id.clone(),
        },
    )
    .unwrap();
    let run = match result {
        ResponseData::Data(value) => value,
        _ => panic!("expected run"),
    };
    assert_eq!(run["status"], "awaiting_implementation");
    assert!(!path.exists());
    drop(engine);
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn restart_marks_unfinished_preparing_run_uncertain_without_replaying_git() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let encoded=serde_json::json!({"id":run_id,"project_id":"fake","status":"preparing","events":[],"resources_unknown":false}).to_string();
        table.insert(run_id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&fixture.database).unwrap();
    let run = match call(&engine, Command::GetRun { run_id }).unwrap() {
        ResponseData::Data(run) => run,
        _ => panic!("expected run"),
    };
    assert_eq!(run["status"], "uncertain");
    assert_eq!(run["resources_unknown"], true);
}

#[test]
fn unknown_journal_prevents_other_replays_until_reviewed() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "fake".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut t = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let json =
            serde_json::json!({"id":id,"project_id":project.id,"status":"preparing","events":[]})
                .to_string();
        t.insert(id.as_str(), json.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let mut journal = fixture.database.as_os_str().to_os_string();
    journal.push(".completion-journal");
    let journal = PathBuf::from(journal);
    std::fs::create_dir(&journal).unwrap();
    let work = journal.join(format!("{id}.json"));
    let other = journal.join(format!("{}.json", uuid::Uuid::new_v4()));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project.id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&work, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::write(&other, b"unknown fixture").unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let run = match call(&engine, Command::GetRun { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "preparing");
    assert!(work.exists());
    assert_eq!(std::fs::read(&other).unwrap(), b"unknown fixture");
    drop(engine);
    std::fs::remove_file(work).unwrap();
    std::fs::remove_file(other).unwrap();
    std::fs::remove_dir(journal).unwrap();
}

#[cfg(windows)]
#[test]
fn cleanup_retry_does_not_duplicate_completion_event() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "fake".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut t = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let json =
            serde_json::json!({"id":id,"project_id":project.id,"status":"preparing","events":[]})
                .to_string();
        t.insert(id.as_str(), json.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let file = journal.join(format!("{id}.json"));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project.id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&file)
        .unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    };
    assert_eq!(status["failures"][0]["state"], "cleanup_pending");
    drop(locked);
    let retry = match call(&engine, Command::RetryCompletionSave { run_id: id.clone() }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    };
    assert_eq!(retry["state"], "saved");
    let events = match call(
        &engine,
        Command::ListEvents {
            run_id: id,
            after: 0,
        },
    )
    .unwrap()
    {
        ResponseData::Events(v) => v,
        _ => panic!("events"),
    };
    assert_eq!(events.len(), 1);
    drop(engine);
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn approved_run_prepares_one_git_worktree_and_records_completion() {
    use std::{
        process::{Command as Process, Stdio},
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let repo = fixture.directory.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let result = Process::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("sample.txt"), "fixture").unwrap();
    git(&["add", "sample.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    let revision = git(&["rev-parse", "HEAD"]);
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "fake".into(),
            path: repo.to_string_lossy().into_owned(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut p = serde_json::to_value(&project).unwrap();
        p["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":revision,"scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
        p["model_settings"] = serde_json::json!({"provider":"mock","model":"fixture","capabilities":{"streaming":false,"tools":false,"context_tokens":null}});
        let json = p.to_string();
        table.insert(project.id.as_str(), json.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&fixture.database).unwrap();
    let id = match call(
        &engine,
        Command::StartRun {
            project_id: project.id,
            expected_approved_hash: project.draft_hash,
        },
    )
    .unwrap()
    {
        ResponseData::Run { run_id } => run_id,
        _ => panic!("run id"),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let run = match call(&engine, Command::GetRun { run_id: id.clone() }).unwrap() {
            ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        if run["status"] == "awaiting_implementation" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worktree completion not observed: {run}"
        );
        thread::sleep(Duration::from_millis(30));
    }
    let events = match call(
        &engine,
        Command::ListEvents {
            run_id: id,
            after: 0,
        },
    )
    .unwrap()
    {
        ResponseData::Events(v) => v,
        _ => panic!("events"),
    };
    assert_eq!(
        events.iter().filter(|e| e.kind == "worktree_ready").count(),
        1
    );
    drop(engine);
    let resolved = fixture.directory.canonicalize().unwrap();
    let temp_root = std::env::temp_dir().canonicalize().unwrap();
    assert_eq!(resolved.parent(), Some(temp_root.as_path()));
    assert!(resolved
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("magi-run-"));
    std::fs::remove_dir_all(resolved).unwrap();
}

#[test]
fn cancelling_an_uncertain_run_preserves_uncertainty() {
    let fixture = Fixture::new();
    drop(Engine::open(&fixture.database).unwrap());
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let data=serde_json::json!({"id":id,"project_id":"fake","status":"preparing","events":[],"resources_unknown":false}).to_string();
        table.insert(id.as_str(), data.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&fixture.database).unwrap();
    // 기동 복구가 preparing을 uncertain으로 바꾸므로, 취소 계약은 live run 테스트에서 별도로 확인한다.
    call(&engine, Command::CancelRun { run_id: id.clone() }).unwrap();
    let run = match call(&engine, Command::GetRun { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "uncertain");
    assert_eq!(run["resources_released"], false);
}

#[test]
fn run_list_is_project_scoped_and_validates_pagination() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "fake".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    let page = match call(
        &engine,
        Command::ListRuns {
            project_id: project.id.clone(),
            offset: 0,
            limit: 10,
        },
    )
    .unwrap()
    {
        ResponseData::Data(v) => v,
        _ => panic!("page"),
    };
    assert_eq!(page["total"], 0);
    assert_eq!(
        call(
            &engine,
            Command::ListRuns {
                project_id: project.id,
                offset: 0,
                limit: 0
            }
        )
        .unwrap_err(),
        "invalid run page"
    );
}

#[test]
fn missing_database_with_journal_remains_missing_and_diagnosable() {
    let fixture = Fixture::new();
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let dir = PathBuf::from(name);
    std::fs::create_dir(&dir).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let data = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("diagnostic"),
    };
    assert!(!data["failures"].as_array().unwrap().is_empty());
    assert!(!fixture.database.exists());
    drop(engine);
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn startup_retry_reopens_only_restored_existing_database() {
    let fixture = Fixture::new();
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("status"),
    };
    let id = status["failures"][0]["run_id"].as_str().unwrap().to_owned();
    let first = match call(&engine, Command::RetryCompletionSave { run_id: id.clone() }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("retry"),
    };
    assert_eq!(first["state"], "pending");
    assert!(!fixture.database.exists());
    let db = redb::Database::create(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut meta = tx
            .open_table(redb::TableDefinition::<&str, u64>::new("meta"))
            .unwrap();
        meta.insert("schema_version", 1).unwrap();
        tx.open_table(redb::TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let second = match call(&engine, Command::RetryCompletionSave { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("retry"),
    };
    assert_eq!(second["state"], "saved");
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("status"),
    };
    assert!(status["failures"].as_array().unwrap().is_empty());
    drop(engine);
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn restart_preserves_legacy_run_fields_and_marks_agent_interrupted() {
    let fixture = Fixture::new();
    drop(Engine::open(&fixture.database).unwrap());
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let value=serde_json::json!({"id":id,"project_id":"fake","status":"awaiting_implementation","events":[],"agent_active":true,"legacy_extra":"preserve"}).to_string();
        table.insert(id.as_str(), value.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&fixture.database).unwrap();
    let run = match call(&engine, Command::GetRun { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "interrupted");
    assert_eq!(run["agent_active"], false);
    assert_eq!(run["legacy_extra"], "preserve");
}

#[test]
fn startup_pending_completion_blocks_all_project_mutations() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let first = match call(
        &engine,
        Command::CreateProject {
            name: "first".into(),
            path: "first".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    let second = match call(
        &engine,
        Command::CreateProject {
            name: "second".into(),
            path: "second".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let dir = PathBuf::from(name);
    std::fs::create_dir(&dir).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.json"));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":first.id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    assert_eq!(
        call(
            &engine,
            Command::SubmitInterview {
                project_id: first.id,
                message: "blocked".into()
            }
        )
        .unwrap_err(),
        "완료 결과 저장을 먼저 복구해야 합니다."
    );
    assert_eq!(
        call(
            &engine,
            Command::SubmitInterview {
                project_id: second.id,
                message: "blocked".into()
            }
        )
        .unwrap_err(),
        "완료 결과 저장을 먼저 복구해야 합니다."
    );
    drop(engine);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn locked_database_keeps_diagnostic_and_same_retry_recovers_after_unlock() {
    let fixture = Fixture::new();
    drop(Engine::open(&fixture.database).unwrap());
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let project = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let data =
            serde_json::json!({"id":id,"project_id":project,"status":"preparing","events":[]})
                .to_string();
        table.insert(id.as_str(), data.as_str()).unwrap();
    }
    tx.commit().unwrap();
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let path = journal.join(format!("{id}.json"));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("status"),
    };
    assert_eq!(status["failures"].as_array().unwrap().len(), 1);
    assert_eq!(status["failures"][0]["run_id"], id);
    drop(db);
    let retry = match call(&engine, Command::RetryCompletionSave { run_id: id.clone() }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("retry"),
    };
    assert_eq!(retry["state"], "saved");
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("status"),
    };
    assert!(status["failures"].as_array().unwrap().is_empty());
    let run = match call(&engine, Command::GetRun { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "awaiting_implementation");
    drop(engine);
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn retry_does_not_apply_journal_to_a_future_schema_database() {
    let fixture = Fixture::new();
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let project = uuid::Uuid::new_v4().to_string();
    let path = journal.join(format!("{id}.json"));
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project,"outcome":{"stage":"worktree","result":{"Ok":null}}});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let db = redb::Database::create(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut meta = tx
            .open_table(redb::TableDefinition::<&str, u64>::new("meta"))
            .unwrap();
        meta.insert("schema_version", 99).unwrap();
        tx.open_table(redb::TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut runs = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let data =
            serde_json::json!({"id":id,"project_id":project,"status":"preparing","events":[]})
                .to_string();
        runs.insert(id.as_str(), data.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let response = match call(&engine, Command::RetryCompletionSave { run_id: id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("retry"),
    };
    assert_eq!(response["state"], "pending");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_vec(&record).unwrap()
    );
    drop(engine);
    std::fs::remove_file(path).unwrap();
    let log = journal.join("events.log");
    if log.exists() {
        std::fs::remove_file(log).unwrap();
    }
    std::fs::remove_dir(journal).unwrap();
}

#[test]
fn journal_with_missing_schema_is_rejected_without_modifying_database() {
    let fixture = Fixture::new();
    let db = redb::Database::create(&fixture.database).unwrap();
    drop(db);
    let before = std::fs::read(&fixture.database).unwrap();
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let result = Engine::open(&fixture.database);
    assert!(result.is_err());
    assert_eq!(std::fs::read(&fixture.database).unwrap(), before);
    std::fs::remove_dir(journal).unwrap();
}

#[cfg(windows)]
#[test]
fn generation_cleanup_retry_does_not_duplicate_generated_event() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.database).unwrap();
    let project = match call(
        &engine,
        Command::CreateProject {
            name: "fake".into(),
            path: "not-created".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let db = redb::Database::open(&fixture.database).unwrap();
    let tx = db.begin_write().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut t = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let json =
            serde_json::json!({"id":id,"project_id":project.id,"status":"generating","events":[],"spec_hash":project.draft_hash,"context_version":project.context_version})
                .to_string();
        t.insert(id.as_str(), json.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let mut name = fixture.database.as_os_str().to_os_string();
    name.push(".completion-journal");
    let journal = PathBuf::from(name);
    std::fs::create_dir(&journal).unwrap();
    let file = journal.join(format!("{id}.json"));
    let draft = domain::SpecDraft {
        title: "생성됨".into(),
        ..Default::default()
    };
    let report = serde_json::json!({"policy_version":1,"draft":draft,"assumptions":[],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null});
    let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project.id,"outcome":{"stage":"generation","result":{"Ok":report}}});
    std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&file)
        .unwrap();
    let engine = Engine::open(&fixture.database).unwrap();
    let status = match call(&engine, Command::ListStorageFailures).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    };
    assert_eq!(status["failures"][0]["state"], "cleanup_pending");
    drop(locked);
    let retry = match call(&engine, Command::RetryCompletionSave { run_id: id.clone() }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    };
    assert_eq!(retry["state"], "saved");
    let events = match call(
        &engine,
        Command::ListEvents {
            run_id: id,
            after: 0,
        },
    )
    .unwrap()
    {
        ResponseData::Events(v) => v,
        _ => panic!("events"),
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "generated");
    drop(engine);
    std::fs::remove_dir(journal).unwrap();
}
