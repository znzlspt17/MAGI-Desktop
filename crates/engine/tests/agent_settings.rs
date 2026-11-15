use domain::{AgentConfig, Command, Project, Request, ResponseData};
use engine::Engine;
use redb::{Database, TableDefinition};
use serde_json::json;
use std::{fs, path::PathBuf};

const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
struct Fixture {
    dir: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("magi-agent-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        Self {
            db: dir.join("state.redb"),
            dir,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_dir(&self.dir);
    }
}
fn call(engine: &Engine, command: Command) -> Result<ResponseData, String> {
    engine
        .handle(Request {
            id: "settings-test".into(),
            command,
        })
        .result
}
fn create(engine: &Engine) -> Project {
    match call(
        engine,
        Command::CreateProject {
            name: "fixture".into(),
            path: "fake-path".into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("expected project"),
    }
}
fn get(engine: &Engine, id: &str) -> Project {
    match call(
        engine,
        Command::GetProject {
            project_id: id.into(),
        },
    )
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("expected project"),
    }
}
fn configs() -> Vec<AgentConfig> {
    ["coordinator", "implementer", "reviewer"].iter().map(|role| AgentConfig {
        role: (*role).into(), model: "  custom-model  ".into(),
        tools: if *role == "reviewer" { vec!["read".into(), "search".into()] } else { vec!["read".into(), "patch".into(), "exec".into(), "git".into(), "mcp".into()] },
        limits: json!({"max_rounds":64,"max_tokens":1000000,"max_output_tokens":1,"extra":{"preserved":true}}),
    }).collect()
}
fn configure(engine: &Engine, id: &str, agents: Vec<AgentConfig>) -> Result<ResponseData, String> {
    call(
        engine,
        Command::ConfigureAgents {
            project_id: id.into(),
            agents,
        },
    )
}
fn mode(engine: &Engine, id: &str, mode: &str) -> Result<ResponseData, String> {
    call(
        engine,
        Command::SetAutonomy {
            project_id: id.into(),
            mode: mode.into(),
        },
    )
}
fn save_fixture(db_path: &std::path::Path, project: &Project) {
    let db = Database::open(db_path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(PROJECTS).unwrap();
        let value = serde_json::to_string(project).unwrap();
        t.insert(project.id.as_str(), value.as_str()).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn settings_and_autonomy_persist_without_rewriting_payload_or_approval() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let mut p = create(&engine);
    drop(engine);
    p.assessment = Some(domain::AssessmentRef {
        draft_hash: p.draft_hash.clone(),
        context_version: 0,
    });
    let approved = domain::ApprovedSpec {
        version: 1,
        hash: p.draft_hash.clone(),
        base_revision: "fixture".into(),
        scope: vec!["src".into()],
        requirements: vec!["r".into()],
        acceptance: vec!["a".into()],
        accepted_optional: vec![],
        markdown: String::new(),
        source_links: vec![],
        constraints: vec![],
        tasks: vec![],
        reused_decisions: vec![],
        task_bindings: vec![],
    };
    p.approved = Some(approved.clone());
    p.approval_history = vec![approved];
    p.approval_version = 1;
    save_fixture(&f.db, &p);
    let engine = Engine::open(&f.db).unwrap();
    let input = configs();
    assert!(matches!(
        configure(&engine, &p.id, input.clone()).unwrap(),
        ResponseData::Project(_)
    ));
    assert!(matches!(
        mode(&engine, &p.id, "approved_scope").unwrap(),
        ResponseData::Project(_)
    ));
    drop(engine);
    let engine = Engine::open(&f.db).unwrap();
    let saved = get(&engine, &p.id);
    assert_eq!(
        serde_json::to_value(&saved.agent_configs).unwrap(),
        serde_json::to_value(input).unwrap()
    );
    assert_eq!(saved.autonomy_mode, "approved_scope");
    assert_eq!(saved.context_version, 2);
    assert_eq!(
        serde_json::to_value(saved.approved).unwrap(),
        serde_json::to_value(p.approved).unwrap()
    );
    assert_eq!(
        serde_json::to_value(saved.approval_history).unwrap(),
        serde_json::to_value(p.approval_history).unwrap()
    );
    assert_eq!(
        serde_json::to_value(saved.assessment).unwrap(),
        serde_json::to_value(p.assessment).unwrap()
    );
    assert_eq!(
        call(
            &engine,
            Command::ApproveSpec {
                project_id: p.id.clone(),
                expected_draft_hash: p.draft_hash.clone(),
                accepted_optional: vec![]
            }
        )
        .unwrap_err(),
        "draft requires a new assessment"
    );
    match mode(&engine, &p.id, "step").unwrap() {
        ResponseData::Project(p) => assert_eq!(p.context_version, 3),
        _ => panic!("expected project"),
    }
}

#[test]
fn rejects_invalid_configs_and_modes_without_changing_project() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let p = create(&engine);
    let mut invalid = Vec::new();
    let mut a = configs();
    a.pop();
    invalid.push(a);
    let mut a = configs();
    a[1].role = "coordinator".into();
    invalid.push(a);
    let mut a = configs();
    a[0].model = "  ".into();
    invalid.push(a);
    for value in [
        json!(0),
        json!(65),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
    ] {
        let mut a = configs();
        a[0].limits["max_rounds"] = value;
        invalid.push(a);
    }
    for value in [
        json!(0),
        json!(1000001),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
    ] {
        let mut a = configs();
        a[1].limits["max_tokens"] = value;
        invalid.push(a);
    }
    for value in [
        json!(0),
        json!(1000001),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
    ] {
        let mut a = configs();
        a[0].limits["max_output_tokens"] = value;
        invalid.push(a);
    }
    let mut a = configs();
    a[2].tools.push("patch".into());
    invalid.push(a);
    let mut a = configs();
    a[0].tools.push("unsupported".into());
    invalid.push(a);
    let baseline = serde_json::to_value(&p).unwrap();
    for agents in invalid {
        assert!(configure(&engine, &p.id, agents).is_err());
        assert_eq!(serde_json::to_value(get(&engine, &p.id)).unwrap(), baseline);
    }
    for value in ["", "auto", "STEP"] {
        assert!(mode(&engine, &p.id, value).is_err());
        assert_eq!(serde_json::to_value(get(&engine, &p.id)).unwrap(), baseline);
    }
    let mut minimum = configs();
    for a in &mut minimum {
        a.limits = json!({"max_rounds":1,"max_tokens":1});
    }
    assert!(configure(&engine, &p.id, minimum).is_ok());
}

#[test]
fn exhausted_context_rejects_both_settings_without_project_mutation() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let mut p = create(&engine);
    drop(engine);
    p.context_version = u64::MAX;
    save_fixture(&f.db, &p);
    let engine = Engine::open(&f.db).unwrap();
    let before = serde_json::to_value(get(&engine, &p.id)).unwrap();
    assert_eq!(
        configure(&engine, &p.id, configs()).unwrap_err(),
        "interview context version exhausted"
    );
    assert_eq!(serde_json::to_value(get(&engine, &p.id)).unwrap(), before);
    assert_eq!(
        mode(&engine, &p.id, "approved_scope").unwrap_err(),
        "interview context version exhausted"
    );
    assert_eq!(serde_json::to_value(get(&engine, &p.id)).unwrap(), before);
}
