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

fn data(r: ResponseData) -> serde_json::Value {
    match r {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    }
}
#[tokio::test]
async fn selected_decision_is_versioned_project_scoped_and_remains_immutable() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let a = fixture.project(&engine, "a");
    let b = fixture.project(&engine, "b");
    send(
        &engine,
        Command::SaveDecision {
            project_id: a.clone(),
            decision: "first".into(),
        },
    )
    .await
    .unwrap();
    let list = data(
        send(
            &engine,
            Command::ListDecisions {
                project_id: a.clone(),
            },
        )
        .await
        .unwrap(),
    );
    let d = &list["decisions"][0];
    let id = d["id"].as_str().unwrap().to_owned();
    assert_eq!(d["version"], 1);
    assert!(send(
        &engine,
        Command::SelectDecision {
            project_id: b.clone(),
            id: id.clone(),
            version: 1
        }
    )
    .await
    .is_err());
    send(
        &engine,
        Command::SelectDecision {
            project_id: a.clone(),
            id: id.clone(),
            version: 1,
        },
    )
    .await
    .unwrap();
    let revised = data(
        send(
            &engine,
            Command::ReviseDecision {
                project_id: a.clone(),
                id: id.clone(),
                text: "second".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(revised["version"], 2);
    drop(engine);
    let engine = fixture.engine();
    let list = data(
        send(
            &engine,
            Command::ListDecisions {
                project_id: a.clone(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(list["decisions"].as_array().unwrap().len(), 2);
    let project = match send(&engine, Command::GetProject { project_id: a })
        .await
        .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    assert_eq!(project.draft.reused_decisions.len(), 1);
    let snapshot: serde_json::Value =
        serde_json::from_str(&project.draft.reused_decisions[0]).unwrap();
    assert_eq!(snapshot["text"], "first");
    assert_eq!(snapshot["version"], 1);
    let other = data(
        send(&engine, Command::ListDecisions { project_id: b })
            .await
            .unwrap(),
    );
    assert!(other["decisions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn selecting_decision_invalidates_approval_and_requires_existing_version() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    send(
        &engine,
        Command::SaveDecision {
            project_id: id.clone(),
            decision: "first".into(),
        },
    )
    .await
    .unwrap();
    let record = data(
        send(
            &engine,
            Command::ListDecisions {
                project_id: id.clone(),
            },
        )
        .await
        .unwrap(),
    );
    let decision_id = record["decisions"][0]["id"].as_str().unwrap().to_owned();
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
    assert!(p.draft.reused_decisions.is_empty());
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project = serde_json::to_value(p).unwrap();
        project["approved"] = serde_json::json!({"version":1,"hash":project["draft_hash"],"base_revision":"fixture","scope":[],"requirements":[],"acceptance":[],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
        table
            .insert(id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    assert!(send(
        &engine,
        Command::SelectDecision {
            project_id: id.clone(),
            id: decision_id.clone(),
            version: 2
        }
    )
    .await
    .is_err());
    let before = match send(
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
    assert!(before.approved.is_some());
    send(
        &engine,
        Command::SelectDecision {
            project_id: id.clone(),
            id: decision_id,
            version: 1,
        },
    )
    .await
    .unwrap();
    let after = match send(&engine, Command::GetProject { project_id: id })
        .await
        .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    assert!(after.approved.is_none());
    assert_ne!(before.draft_hash, after.draft_hash);
    assert_eq!(after.context_version, before.context_version + 1);
}

#[tokio::test]
async fn blank_decisions_are_rejected_without_project_changes() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    let before = match send(
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
    assert!(send(
        &engine,
        Command::SaveDecision {
            project_id: id.clone(),
            decision: " ".into()
        }
    )
    .await
    .is_err());
    let after = match send(&engine, Command::GetProject { project_id: id })
        .await
        .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    assert_eq!(before.context_version, after.context_version);
    assert!(after.decisions.is_empty());
}

#[tokio::test]
async fn existing_unversioned_decisions_can_be_selected_explicitly() {
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
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project = serde_json::to_value(p).unwrap();
        project["decisions"] = serde_json::json!(["legacy decision"]);
        table
            .insert(id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let records = data(
        send(
            &engine,
            Command::ListDecisions {
                project_id: id.clone(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(records["decisions"][0]["id"], "legacy:0");
    send(
        &engine,
        Command::SelectDecision {
            project_id: id.clone(),
            id: "legacy:0".into(),
            version: 1,
        },
    )
    .await
    .unwrap();
    let project = match send(&engine, Command::GetProject { project_id: id })
        .await
        .unwrap()
    {
        ResponseData::Project(p) => p,
        _ => panic!("project"),
    };
    let selected: serde_json::Value =
        serde_json::from_str(&project.draft.reused_decisions[0]).unwrap();
    assert_eq!(selected["text"], "legacy decision");
}
