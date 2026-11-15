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

fn configure(
    engine: &Engine,
    id: &str,
    provider: &str,
    model: &str,
) -> Result<ResponseData, String> {
    call(
        engine,
        "configure",
        Command::ConfigureModel {
            project_id: id.into(),
            provider: provider.into(),
            model: model.into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(4321),
            },
        },
    )
}

#[test]
fn model_settings_are_persisted_and_make_assessment_stale() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    for (index, provider) in ["mock", "openai", "local"].iter().enumerate() {
        let updated =
            project(configure(&engine, &original.id, provider, "  explicit-model  ").unwrap());
        assert_eq!(updated.context_version, 8 + index as u64);
        let settings = updated.model_settings.unwrap();
        assert_eq!(settings.provider, *provider);
        assert_eq!(settings.model, "  explicit-model  ");
        assert!(settings.capabilities.streaming);
        assert!(!settings.capabilities.tools);
        assert_eq!(settings.capabilities.context_tokens, Some(4321));
        assert_eq!(
            serde_json::to_value(updated.approved).unwrap(),
            serde_json::to_value(&original.approved).unwrap()
        );
        assert_eq!(
            serde_json::to_value(updated.assessment).unwrap(),
            serde_json::to_value(&original.assessment).unwrap()
        );
    }
    assert_eq!(
        call(
            &engine,
            "approve",
            Command::ApproveSpec {
                project_id: original.id.clone(),
                expected_draft_hash: original.draft_hash.clone(),
                accepted_optional: vec![]
            }
        )
        .unwrap_err(),
        "draft requires a new assessment"
    );
    let repeated =
        project(configure(&engine, &original.id, "local", "  explicit-model  ").unwrap());
    assert_eq!(repeated.context_version, 11);
    drop(engine);
    let engine = Engine::open(&fixture.path).unwrap();
    let saved = read_project(&engine, &original.id);
    assert_eq!(
        serde_json::to_value(&saved).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    assert_eq!(saved.context_version, 11);
    assert_eq!(saved.model_settings.unwrap().provider, "local");
}

#[test]
fn invalid_model_settings_leave_project_unchanged() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    for (provider, model) in [("other", "model"), ("mock", " \t")] {
        assert_eq!(
            configure(&engine, &original.id, provider, model).unwrap_err(),
            "unsupported provider or empty model"
        );
    }
    drop(engine);
    let engine = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        serde_json::to_value(read_project(&engine, &original.id)).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn exhausted_context_rejects_model_settings_without_changes() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, u64::MAX);
    let engine = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        configure(&engine, &original.id, "mock", "model").unwrap_err(),
        "interview context version exhausted"
    );
    drop(engine);
    let engine = Engine::open(&fixture.path).unwrap();
    assert_eq!(
        serde_json::to_value(read_project(&engine, &original.id)).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn mock_generation_returns_unapproved_mock_draft_and_preserves_interview() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = Engine::open(&fixture.path).unwrap();
    configure(&engine, &original.id, "mock", "fixture-model").unwrap();
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "요청 원문".into(),
        },
    )
    .unwrap();
    let generated = project(
        call(
            &engine,
            "generate",
            Command::GenerateSpec {
                project_id: original.id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(generated.draft.title, "[Mock 초안] 프로젝트");
    assert_eq!(generated.draft.requirements, vec!["요청 원문"]);
    assert_eq!(
        generated.draft.mandatory_questions,
        vec!["범위와 수용 기준, 기준 Git revision을 확인하세요."]
    );
    assert!(generated.approved.is_none());
    assert_eq!(generated.approval_history.len(), 1);
    assert_eq!(generated.interview, vec!["요청 원문"]);
    drop(engine);
    let reopened = Engine::open(&fixture.path).unwrap();
    let saved = read_project(&reopened, &original.id);
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(generated).unwrap()
    );
}

#[test]
fn mock_generation_requires_model_and_interview_and_rejects_local() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let created = project(
        call(
            &engine,
            "create",
            Command::CreateProject {
                name: "가짜".into(),
                path: "not-created".into(),
            },
        )
        .unwrap(),
    );
    let generate = || {
        call(
            &engine,
            "generate",
            Command::GenerateSpec {
                project_id: created.id.clone(),
            },
        )
    };
    assert_eq!(generate().unwrap_err(), "configure a model first");
    configure(&engine, &created.id, "mock", "fake").unwrap();
    assert_eq!(generate().unwrap_err(), "interview is empty");
    configure(&engine, &created.id, "local", "fake").unwrap();
    assert_eq!(
        generate().unwrap_err(),
        "selected provider is not available"
    );
}

#[test]
fn runtime_credential_validation_does_not_persist_fake_secret_in_project() {
    let fixture = TestDatabase::new();
    let engine = Engine::open(&fixture.path).unwrap();
    let created = project(
        call(
            &engine,
            "create",
            Command::CreateProject {
                name: "가짜".into(),
                path: "not-created".into(),
            },
        )
        .unwrap(),
    );
    let invalid = call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: created.id.clone(),
            endpoint: " ".into(),
            credential: domain::SecretString("fake-secret-test".into()),
        },
    );
    assert_eq!(invalid.unwrap_err(), "provider configuration incomplete");
    let response = call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: created.id.clone(),
            endpoint: "http://127.0.0.1:1234".into(),
            credential: domain::SecretString("fake-secret-test".into()),
        },
    )
    .unwrap();
    assert!(matches!(response, ResponseData::Empty));
    let saved = read_project(&engine, &created.id);
    assert!(!serde_json::to_string(&saved)
        .unwrap()
        .contains("fake-secret-test"));
    drop(engine);
    let engine = Engine::open(&fixture.path).unwrap();
    let saved = read_project(&engine, &created.id);
    assert!(!serde_json::to_string(&saved)
        .unwrap()
        .contains("fake-secret-test"));
}

#[test]
fn generation_journal_replays_complete_report_and_updates_project_atomically() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let db = Database::open(&fixture.path).unwrap();
    let tx = db.begin_write().unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    let completion_id = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let run = serde_json::json!({"id":run_id,"project_id":original.id,"status":"generating","events":[],"spec_hash":original.draft_hash,"context_version":7});
        let encoded = run.to_string();
        table.insert(run_id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let draft = SpecDraft {
        title: "생성된 초안".into(),
        ..Default::default()
    };
    let base = serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[{"requirement_id":"r","source":"s","quote":"q"}],"verdicts":[{"rubric_id":"v","passed":true,"section":"s","quote":"q","reason":"r","issue_checklist_id":null}],"issues":[{"checklist_id":"i","requirement_id":"r","rubric_ids":["v"],"severity":"optional","evidence":[{"source":"s","quote":"q"}]}],"revision_directives":["수정"],"score_reason":"근거","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d"});
    let mut report = base.clone();
    report["question_review"] = base;
    let mut name = fixture.path.as_os_str().to_os_string();
    name.push(".completion-journal");
    let dir = PathBuf::from(name);
    fs::create_dir(&dir).unwrap();
    let path = dir.join(format!("{run_id}.json"));
    let record = serde_json::json!({"version":1,"id":completion_id,"run_id":run_id,"project_id":original.id,"outcome":{"stage":"generation","result":{"Ok":report}}});
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.path).unwrap();
    let saved = read_project(&engine, &original.id);
    assert_eq!(saved.draft.title, "생성된 초안");
    assert!(saved.approved.is_none());
    assert_eq!(saved.approval_history.len(), 1);
    assert_eq!(
        saved.assessment.as_ref().unwrap().draft_hash,
        saved.draft_hash
    );
    assert_eq!(saved.assessment.unwrap().context_version, 7);
    let run = match call(&engine, "run", Command::GetRun { run_id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "generated");
    assert_eq!(run["report"], report);
    assert!(!path.exists());
    drop(engine);
    fs::remove_dir(dir).unwrap();
}

#[tokio::test]
async fn openai_generation_returns_run_without_waiting_for_stalled_response() {
    let fixture = TestDatabase::new();
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    let created = project(
        call(
            &engine,
            "create",
            Command::CreateProject {
                name: "가짜".into(),
                path: "not-created".into(),
            },
        )
        .unwrap(),
    );
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: created.id.clone(),
            message: "요청".into(),
        },
    )
    .unwrap();
    call(
        &engine,
        "draft",
        Command::SaveDraft {
            project_id: created.id.clone(),
            draft: SpecDraft {
                base_revision: "revision".into(),
                acceptance: vec!["검증".into()],
                ..Default::default()
            },
        },
    )
    .unwrap();
    call(
        &engine,
        "model",
        Command::ConfigureModel {
            project_id: created.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: created.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-generation-test".into()),
        },
    )
    .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        engine.clone().handle_async(Request {
            id: "generate".into(),
            command: Command::GenerateSpec {
                project_id: created.id.clone(),
            },
        }),
    )
    .await
    .expect("generation request blocked");
    assert_eq!(response.id, "generate");
    let run_id = match response.result.unwrap() {
        ResponseData::Run { run_id } => run_id,
        _ => panic!("expected run"),
    };
    let (_socket, _) = tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
        .await
        .expect("provider was not called")
        .unwrap();
    let before_cancel = serde_json::to_value(read_project(&engine, &created.id)).unwrap();
    let active = match call(
        &engine,
        "active",
        Command::GetRun {
            run_id: run_id.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Data(value) => value,
        _ => panic!("expected run"),
    };
    assert_eq!(active["resources_released"], false);
    let status = engine
        .clone()
        .handle_async(Request {
            id: "status".into(),
            command: Command::Status,
        })
        .await;
    assert!(status.result.is_ok());
    let cancelled = engine
        .clone()
        .handle_async(Request {
            id: "cancel".into(),
            command: Command::CancelRun {
                run_id: run_id.clone(),
            },
        })
        .await;
    assert!(cancelled.result.is_ok());
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = match call(
                &engine,
                "run",
                Command::GetRun {
                    run_id: run_id.clone(),
                },
            )
            .unwrap()
            {
                ResponseData::Data(value) => value,
                _ => panic!("expected run"),
            };
            if value["resources_released"] == true {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled worker did not release resources");
    assert_eq!(run["status"], "cancelled");
    assert_eq!(
        serde_json::to_value(read_project(&engine, &created.id)).unwrap(),
        before_cancel
    );
}

const HEADINGS: [&str; 24] = [
    "# 최종 에이전트 명세",
    "## 1. 목표",
    "## 2. 배경",
    "## 3. 사용자 의도",
    "## 4. 범위",
    "### 4.1 범위 포함",
    "### 4.2 범위 제외",
    "## 5. 용어 정의",
    "## 6. 근거 요약",
    "## 7. 필수 요구사항",
    "## 8. 권장 요구사항",
    "## 9. 선택 요구사항",
    "## 10. 금지 행동",
    "## 11. 입력 계약",
    "## 12. 출력 계약",
    "## 13. 아키텍처",
    "## 14. 모듈 책임",
    "## 15. 데이터 흐름",
    "## 16. 오류 처리 정책",
    "## 17. 설정 정책",
    "## 18. 저장 및 산출물 정책",
    "## 19. 구현 순서",
    "## 20. 수용 기준",
    "## 21. 테스트 계획",
];
fn markdown() -> String {
    let mut s = HEADINGS
        .iter()
        .map(|h| format!("{h}\n검색 제공\n"))
        .collect::<String>();
    s.push_str("## 22. 수동 검증 확인 목록\n검색 제공\n## 23. AI 코딩 에이전트 지침\n검색 제공\n");
    s
}

fn candidate() -> serde_json::Value {
    serde_json::json!({"policy_version":4,"draft":{"title":"검색","scope":["검색 제공"],"requirements":["검색 제공"],"acceptance":["검색 결과 확인"],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":"abc","markdown":markdown(),"source_links":["requirement-0"],"constraints":["제약 없음"],"tasks":[{"id":"task1","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/readme.txt:검색"]}],"reused_decisions":[],"task_bindings":[{"task_id":"task1","goal":"검색 구현","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]},"assumptions":[],"evidence":[{"requirement_id":"requirement-0","source":"request","quote":"검색"}],"verdicts":[{"rubric_id":"criterion-0","passed":true,"section":"## 7. 필수 요구사항","quote":"검색 제공","reason":"요구사항 포함","issue_checklist_id":null}],"issues":[],"revision_directives":[],"score_reason":"기준 충족","proposed_score":10,"score":10})
}

#[tokio::test]
async fn loopback_generation_saves_report_draft_and_assessment_without_approving() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "검색 제공".into(),
        },
    )
    .unwrap();
    call(
        &engine,
        "draft",
        Command::SaveDraft {
            project_id: original.id.clone(),
            draft: SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        },
    )
    .unwrap();
    call(
        &engine,
        "model",
        Command::ConfigureModel {
            project_id: original.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: original.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-loopback-secret".into()),
        },
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let mut best = candidate();
        best["proposed_score"] = 8.into();
        best["score"] = 8.into();
        let mut review = candidate();
        review["draft"]["title"] = "질문 검토".into();
        review["draft"]["mandatory_questions"] = serde_json::json!(["결정 필요"]);
        review["verdicts"][0]["passed"] = false.into();
        review["verdicts"][0]["issue_checklist_id"] = "issue-1".into();
        review["issues"] = serde_json::json!([{"checklist_id":"issue-1","requirement_id":"requirement-0","rubric_ids":["criterion-0"],"severity":"major","evidence":[{"source":"## 7. 필수 요구사항","quote":"검색 제공"}]}]);
        review["revision_directives"] = serde_json::json!(["검색 보완"]);
        for candidate in [best, review] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 16384];
            let _ = socket.read(&mut request).await.unwrap();
            let body = format!(
                "data: {}\n\n",
                serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":candidate.to_string()}]}]}})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let response = engine
        .clone()
        .handle_async(Request {
            id: "generation".into(),
            command: Command::GenerateSpec {
                project_id: original.id.clone(),
            },
        })
        .await;
    let run_id = match response.result.unwrap() {
        ResponseData::Run { run_id } => run_id,
        _ => panic!("expected run"),
    };
    server.await.unwrap();
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = match call(
                &engine,
                "run",
                Command::GetRun {
                    run_id: run_id.clone(),
                },
            )
            .unwrap()
            {
                ResponseData::Data(value) => value,
                _ => panic!("expected run"),
            };
            if value["resources_released"] == true {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("generation did not finish");
    assert_eq!(run["status"], "generated", "report: {}", run["report"]);
    assert_eq!(
        run["report"]["evidence"][0]["requirement_id"],
        "requirement-0"
    );
    assert_eq!(
        run["report"]["question_review"]["draft"]["title"],
        "질문 검토"
    );
    assert_eq!(
        run["report"]["question_review"]["issues"][0]["checklist_id"],
        "issue-1"
    );
    let saved = read_project(&engine, &original.id);
    assert_eq!(saved.draft.title, "검색");
    assert!(saved.approved.is_none());
    assert_eq!(saved.approval_history.len(), 1);
    assert_eq!(
        saved.assessment.as_ref().unwrap().draft_hash,
        saved.draft_hash
    );
    assert_eq!(
        saved.assessment.as_ref().unwrap().context_version,
        saved.context_version
    );
    assert!(!serde_json::to_string(&run)
        .unwrap()
        .contains("fake-loopback-secret"));
}

#[tokio::test]
async fn openai_preflight_rejects_missing_credential_without_network_or_project_changes() {
    let fixture = TestDatabase::new();
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    let created = project(
        call(
            &engine,
            "create",
            Command::CreateProject {
                name: "가짜".into(),
                path: "not-created".into(),
            },
        )
        .unwrap(),
    );
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: created.id.clone(),
            message: "요청".into(),
        },
    )
    .unwrap();
    call(
        &engine,
        "draft",
        Command::SaveDraft {
            project_id: created.id.clone(),
            draft: SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검증".into()],
                ..Default::default()
            },
        },
    )
    .unwrap();
    call(
        &engine,
        "model",
        Command::ConfigureModel {
            project_id: created.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let before = serde_json::to_value(read_project(&engine, &created.id)).unwrap();
    let response = engine
        .clone()
        .handle_async(Request {
            id: "generate".into(),
            command: Command::GenerateSpec {
                project_id: created.id.clone(),
            },
        })
        .await;
    assert_eq!(response.result.unwrap_err(), "runtime credential required");
    assert_eq!(
        serde_json::to_value(read_project(&engine, &created.id)).unwrap(),
        before
    );
    call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: created.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-preflight-secret".into()),
        },
    )
    .unwrap();
    call(
        &engine,
        "small-context",
        Command::ConfigureModel {
            project_id: created.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(1),
            },
        },
    )
    .unwrap();
    let before = serde_json::to_value(read_project(&engine, &created.id)).unwrap();
    let response = engine
        .clone()
        .handle_async(Request {
            id: "generate-small".into(),
            command: Command::GenerateSpec {
                project_id: created.id.clone(),
            },
        })
        .await;
    assert_eq!(
        response.result.unwrap_err(),
        "configured model context is too small"
    );
    assert_eq!(
        serde_json::to_value(read_project(&engine, &created.id)).unwrap(),
        before
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn stale_generation_journal_keeps_changed_project_and_retains_report() {
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let db = Database::open(&fixture.path).unwrap();
    let tx = db.begin_write().unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    let completion_id = uuid::Uuid::new_v4().to_string();
    {
        let mut table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let run = serde_json::json!({"id":run_id,"project_id":original.id,"status":"generating","events":[],"spec_hash":original.draft_hash,"context_version":6});
        let encoded = run.to_string();
        table.insert(run_id.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let draft = SpecDraft {
        title: "생성된 초안".into(),
        ..Default::default()
    };
    let base = serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[{"requirement_id":"r","source":"s","quote":"q"}],"verdicts":[{"rubric_id":"v","passed":true,"section":"s","quote":"q","reason":"r","issue_checklist_id":null}],"issues":[{"checklist_id":"i","requirement_id":"r","rubric_ids":["v"],"severity":"optional","evidence":[{"source":"s","quote":"q"}]}],"revision_directives":["수정"],"score_reason":"근거","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d"});
    let mut report = base.clone();
    report["question_review"] = base;
    let mut name = fixture.path.as_os_str().to_os_string();
    name.push(".completion-journal");
    let dir = PathBuf::from(name);
    fs::create_dir(&dir).unwrap();
    let path = dir.join(format!("{run_id}.json"));
    let record = serde_json::json!({"version":1,"id":completion_id,"run_id":run_id,"project_id":original.id,"outcome":{"stage":"generation","result":{"Ok":report}}});
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let engine = Engine::open(&fixture.path).unwrap();
    let saved = read_project(&engine, &original.id);
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    let run = match call(&engine, "run", Command::GetRun { run_id }).unwrap() {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "stale");
    assert_eq!(run["report"], report);
    assert!(!path.exists());
    drop(engine);
    fs::remove_dir(dir).unwrap();
}

#[tokio::test]
async fn loopback_output_echoing_runtime_secret_is_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "검색 제공".into(),
        },
    )
    .unwrap();
    call(
        &engine,
        "draft",
        Command::SaveDraft {
            project_id: original.id.clone(),
            draft: SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        },
    )
    .unwrap();
    call(
        &engine,
        "model",
        Command::ConfigureModel {
            project_id: original.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: original.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-loopback-secret".into()),
        },
    )
    .unwrap();
    let before = serde_json::to_value(read_project(&engine, &original.id)).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 16384];
        let _ = socket.read(&mut request).await.unwrap();
        let mut echo = candidate();
        echo["draft"]["title"] = "fake-loopback-secret".into();
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":echo.to_string()}]}]}})
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let response = engine
        .clone()
        .handle_async(Request {
            id: "generation".into(),
            command: Command::GenerateSpec {
                project_id: original.id.clone(),
            },
        })
        .await;
    let run_id = match response.result.unwrap() {
        ResponseData::Run { run_id } => run_id,
        _ => panic!("expected run"),
    };
    server.await.unwrap();
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = match call(
                &engine,
                "run",
                Command::GetRun {
                    run_id: run_id.clone(),
                },
            )
            .unwrap()
            {
                ResponseData::Data(value) => value,
                _ => panic!("expected run"),
            };
            if value["resources_released"] == true {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("generation did not finish");
    assert_eq!(run["status"], "failed");
    assert_eq!(
        serde_json::to_value(read_project(&engine, &original.id)).unwrap(),
        before
    );
    assert!(!serde_json::to_string(&run)
        .unwrap()
        .contains("fake-loopback-secret"));
}

#[tokio::test]
async fn generation_preserves_new_interview_and_keeps_stale_report() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = TestDatabase::new();
    let original = seed_approved_project(&fixture.path, 7);
    let engine = std::sync::Arc::new(Engine::open(&fixture.path).unwrap());
    call(
        &engine,
        "interview",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "검색 제공".into(),
        },
    )
    .unwrap();
    call(
        &engine,
        "draft",
        Command::SaveDraft {
            project_id: original.id.clone(),
            draft: SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        },
    )
    .unwrap();
    call(
        &engine,
        "model",
        Command::ConfigureModel {
            project_id: original.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    call(
        &engine,
        "credential",
        Command::SetProviderCredential {
            project_id: original.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-loopback-secret".into()),
        },
    )
    .unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 16384];
        let _ = socket.read(&mut request).await.unwrap();
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let echo = candidate();
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":echo.to_string()}]}]}})
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let response = engine
        .clone()
        .handle_async(Request {
            id: "generation".into(),
            command: Command::GenerateSpec {
                project_id: original.id.clone(),
            },
        })
        .await;
    let run_id = match response.result.unwrap() {
        ResponseData::Run { run_id } => run_id,
        _ => panic!("expected run"),
    };
    ready_rx.await.unwrap();
    call(
        &engine,
        "changed",
        Command::SubmitInterview {
            project_id: original.id.clone(),
            message: "새 사실".into(),
        },
    )
    .unwrap();
    let before = serde_json::to_value(read_project(&engine, &original.id)).unwrap();
    release_tx.send(()).unwrap();
    server.await.unwrap();
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = match call(
                &engine,
                "run",
                Command::GetRun {
                    run_id: run_id.clone(),
                },
            )
            .unwrap()
            {
                ResponseData::Data(value) => value,
                _ => panic!("expected run"),
            };
            if value["resources_released"] == true {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("generation did not finish");
    assert_eq!(run["status"], "stale");
    assert_eq!(run["report"]["draft"]["title"], "검색");
    assert_eq!(
        serde_json::to_value(read_project(&engine, &original.id)).unwrap(),
        before
    );
    assert!(!serde_json::to_string(&run)
        .unwrap()
        .contains("fake-loopback-secret"));
}
