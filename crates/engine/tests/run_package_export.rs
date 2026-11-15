#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-knowledge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Self {
            db: root.join("db.redb"),
            root,
        }
    }
    fn engine(&self) -> Arc<Engine> {
        Arc::new(Engine::open(&self.db).unwrap())
    }
    fn project(&self, engine: &Engine, name: &str) -> String {
        match engine
            .handle(Request {
                id: "create".into(),
                command: Command::CreateProject {
                    name: name.into(),
                    path: self.root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            ResponseData::Project(p) => p.id,
            _ => panic!("project"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db);
        let _ = std::fs::remove_dir(&self.root);
    }
}
async fn send(engine: &Arc<Engine>, command: Command) -> Result<ResponseData, String> {
    engine
        .clone()
        .handle_async(Request {
            id: "test".into(),
            command,
        })
        .await
        .result
}

#[tokio::test]
async fn export_run_uses_recorded_approved_version_and_real_evidence() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let project_id = fixture.project(&engine, "project");
    let p = match send(
        &engine,
        Command::GetProject {
            project_id: project_id.clone(),
        },
    )
    .await
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project = serde_json::to_value(p).unwrap();
        project["approval_history"] = serde_json::json!([{"version":2,"hash":"fixture-spec","base_revision":"fixture-rev","scope":["src"],"requirements":["req"],"acceptance":["accept"],"accepted_optional":[],"markdown":"original approved spec","source_links":[],"constraints":["constraint"],"tasks":[],"reused_decisions":[],"task_bindings":[]}]);
        table
            .insert(project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let run = serde_json::json!({"id":"run1","project_id":project_id,"spec_hash":"fixture-spec","spec_version":2,"context_version":3,"agent_attempt":1,"revision":"fixture-rev","status":"completed","events":[],"pending":null,"evidence":[{"revision":"fixture-rev","spec_hash":"fixture-spec","command":"fixture check","passed":true,"output":"real result","verification_id":"v1","worktree_hash":"snapshot","requirement_ids":[],"acceptance_ids":[]}],"evidence_contexts":{"v1":{"schema_version":1,"spec_version":2,"context_version":3,"agent_attempt":1,"task_id":"t"}}});
        table.insert("run1", run.to_string().as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let destination = fixture.root.join("package");
    let response = send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: destination.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap();
    let result = match response {
        ResponseData::Data(v) => v,
        _ => panic!("manifest"),
    };
    assert_eq!(result["test_status"], "not_collected");
    assert_eq!(
        std::fs::read_to_string(destination.join("SPEC.ko.md")).unwrap(),
        "original approved spec"
    );
    assert!(std::fs::read_to_string(destination.join("VERIFICATION.md"))
        .unwrap()
        .contains("v1"));
    assert!(send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: destination.to_string_lossy().into_owned()
        }
    )
    .await
    .is_err());
    assert_eq!(
        std::fs::read_to_string(destination.join("SPEC.ko.md")).unwrap(),
        "original approved spec"
    );
    let verification = std::fs::read_to_string(destination.join("VERIFICATION.md")).unwrap();
    assert!(!verification.contains("real result"));
    let json = verification
        .split_once("```json\n")
        .unwrap()
        .1
        .split_once("\n```")
        .unwrap()
        .0;
    let entries: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(entries[0]["command"], "fixture check");
    assert_eq!(entries[0]["passed"], true);
    assert_eq!(entries[0]["spec_version"], 2);
    assert_eq!(entries[0]["context_version"], 3);
    assert_eq!(entries[0]["agent_attempt"], 1);
    assert_eq!(entries[0]["task_id"], "t");
    assert_eq!(entries[0]["revision"], "fixture-rev");
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        use redb::ReadableTable;
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let row = table.get("run1").unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        let mut stale = run["evidence"][0].clone();
        stale["revision"] = "old-revision".into();
        stale["passed"] = false.into();
        stale["verification_id"] = "v2".into();
        run["evidence"].as_array_mut().unwrap().push(stale);
        table.insert("run1", run.to_string().as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let other = fixture.root.join("partial");
    let response = send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: other.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap();
    let manifest = match response {
        ResponseData::Data(v) => v,
        _ => panic!("manifest"),
    };
    assert_eq!(manifest["test_status"], "not_collected");
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        use redb::ReadableTable;
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let row = table.get("run1").unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        run["evidence"].as_array_mut().unwrap().truncate(1);
        run["evidence"][0]["passed"] = false.into();
        table.insert("run1", run.to_string().as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let failed = fixture.root.join("failed");
    let result = send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: failed.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap();
    let manifest = match result {
        ResponseData::Data(v) => v,
        _ => panic!("manifest"),
    };
    assert_eq!(manifest["test_status"], "failed");
    assert_eq!(manifest["completeness"], "limited");
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    let verification_id = serde_json::to_string(&("t", "final_validation", 0)).unwrap();
    {
        use redb::ReadableTable;
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let row = table.get("run1").unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        run["evidence"][0]["passed"] = true.into();
        run["evidence"][0]["verification_id"] = verification_id.clone().into();
        run["evidence"][0]["requirement_ids"] = serde_json::json!(["r"]);
        run["evidence"][0]["acceptance_ids"] = serde_json::json!(["a"]);
        run["evidence_contexts"][&verification_id] = serde_json::json!({"schema_version":1,"spec_version":2,"context_version":3,"agent_attempt":1,"task_id":"t"});
        table.insert("run1", run.to_string().as_str()).unwrap();
    }
    {
        use redb::ReadableTable;
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project: serde_json::Value = {
            let row = table.get(project_id.as_str()).unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        project["approval_history"][0]["tasks"] = serde_json::json!([{"id":"t","dependencies":[],"owned_paths":["src"],"validation":["fixture check"]}]);
        project["approval_history"][0]["task_bindings"] = serde_json::json!([{"task_id":"t","goal":"goal","requirement_ids":["r"],"acceptance_ids":["a"]}]);
        table
            .insert(project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let valid = fixture.root.join("verified");
    let response = send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: valid.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap();
    let manifest = match response {
        ResponseData::Data(v) => v,
        _ => panic!("manifest"),
    };
    let verification = std::fs::read_to_string(valid.join("VERIFICATION.md")).unwrap();
    let json = verification
        .split_once("```json\n")
        .unwrap()
        .1
        .split_once("\n```")
        .unwrap()
        .0;
    let entries: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(entries[0]["requirement_ids"], serde_json::json!(["r"]));
    assert_eq!(entries[0]["acceptance_ids"], serde_json::json!(["a"]));
    assert_eq!(entries[0]["spec_hash"], "fixture-spec");
    assert_eq!(manifest["test_status"], "passed");
    assert_eq!(manifest["completeness"], "limited");
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        use redb::ReadableTable;
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let row = table.get("run1").unwrap().unwrap();
            serde_json::from_str(row.value()).unwrap()
        };
        run["spec_version"] = 99.into();
        table.insert("run1", run.to_string().as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let invalid = fixture.root.join("invalid-version");
    assert!(send(
        &engine,
        Command::ExportRunPackage {
            run_id: "run1".into(),
            destination: invalid.to_string_lossy().into_owned()
        }
    )
    .await
    .is_err());
    assert!(!invalid.exists());
}

#[tokio::test]
async fn approved_project_export_is_limited_and_never_overwrites() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    let p = match send(
        &engine,
        Command::GetProject {
            project_id: id.clone(),
        },
    )
    .await
    .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    let missing = fixture.root.join("unapproved");
    assert!(send(
        &engine,
        Command::ExportPackage {
            project_id: id.clone(),
            destination: missing.to_string_lossy().into_owned()
        }
    )
    .await
    .is_err());
    assert!(!missing.exists());
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project = serde_json::to_value(p).unwrap();
        project["approved"] = serde_json::json!({"version":1,"hash":project["draft_hash"],"base_revision":"rev","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"approved spec","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
        table
            .insert(id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let dest = fixture.root.join("bundle.zip");
    let result = send(
        &engine,
        Command::ExportPackage {
            project_id: id.clone(),
            destination: dest.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap();
    let manifest = match result {
        ResponseData::Data(v) => v,
        _ => panic!("manifest"),
    };
    assert_eq!(manifest["completeness"], "limited");
    assert_eq!(manifest["test_status"], "not_collected");
    let original = std::fs::read(&dest).unwrap();
    assert!(send(
        &engine,
        Command::ExportPackage {
            project_id: id,
            destination: dest.to_string_lossy().into_owned()
        }
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), original);
}
