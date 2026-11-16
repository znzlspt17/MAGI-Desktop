#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use std::sync::Arc;

async fn send(engine: &Arc<Engine>, command: Command) -> Result<ResponseData, String> {
    engine
        .clone()
        .handle_async(Request {
            id: "fixture".into(),
            command,
        })
        .await
        .result
}

#[tokio::test]
async fn allowlisted_tool_still_requires_explicit_run_approval() {
    let root = std::env::temp_dir().join(format!("magi-policy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let engine = Arc::new(Engine::open(&root.join("db.redb")).unwrap());
    let project_id = match send(
        &engine,
        Command::CreateProject {
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap()
    {
        ResponseData::Project(p) => p.id,
        _ => panic!("project"),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: project_id.clone(),
            endpoint: format!("http://{address}/mcp"),
        },
    )
    .await
    .unwrap();
    send(
        &engine,
        Command::ConfigureMcpPolicy {
            project_id: project_id.clone(),
            allowlist: vec!["read".into()],
        },
    )
    .await
    .unwrap();
    let request = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send(
            &engine,
            Command::CallMcpTool {
                project_id,
                name: "read".into(),
                arguments: serde_json::json!({}),
            },
        ),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("approval"));
    assert!(request.await.unwrap().is_err());
    drop(engine);
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}

#[tokio::test]
async fn read_policy_requires_configured_project_and_rejects_invalid_names() {
    let root = std::env::temp_dir().join(format!("magi-policy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let engine = Arc::new(Engine::open(&root.join("db.redb")).unwrap());
    let project_id = match send(
        &engine,
        Command::CreateProject {
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap()
    {
        ResponseData::Project(p) => p.id,
        _ => panic!("project expected"),
    };
    let policy = |allowlist| Command::ConfigureMcpPolicy {
        project_id: project_id.clone(),
        allowlist,
    };
    assert!(send(&engine, policy(vec!["read".into()])).await.is_err());
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: project_id.clone(),
            endpoint: "http://127.0.0.1:9/mcp".into(),
        },
    )
    .await
    .unwrap();
    assert!(send(&engine, policy(vec![" ".into()])).await.is_err());
    send(&engine, policy(vec!["read".into()])).await.unwrap();
    let denied = send(
        &engine,
        Command::CallMcpTool {
            project_id: project_id.clone(),
            name: "write".into(),
            arguments: serde_json::json!({}),
        },
    )
    .await
    .unwrap_err();
    assert!(denied.contains("approval"));
    drop(engine);
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}

#[tokio::test]
async fn policy_persists_per_project_and_endpoint_reconfiguration_revokes_it() {
    use redb::{Database, TableDefinition};
    let root = std::env::temp_dir().join(format!("magi-policy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let db_path = root.join("db.redb");
    let engine = Arc::new(Engine::open(&db_path).unwrap());
    let mut projects = Vec::new();
    for name in ["a", "b"] {
        let ResponseData::Project(p) = send(
            &engine,
            Command::CreateProject {
                name: name.into(),
                path: root.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap() else {
            panic!("project")
        };
        send(
            &engine,
            Command::ConfigureMcp {
                project_id: p.id.clone(),
                endpoint: "http://127.0.0.1:9/mcp".into(),
            },
        )
        .await
        .unwrap();
        send(
            &engine,
            Command::ConfigureMcpPolicy {
                project_id: p.id.clone(),
                allowlist: vec![name.into()],
            },
        )
        .await
        .unwrap();
        projects.push(p.id);
    }
    drop(engine);
    let db = Database::open(&db_path).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx
        .open_table(TableDefinition::<&str, &str>::new("integration_mcp_policy"))
        .unwrap();
    assert_eq!(
        table.get(projects[0].as_str()).unwrap().unwrap().value(),
        "[\"a\"]"
    );
    assert_eq!(
        table.get(projects[1].as_str()).unwrap().unwrap().value(),
        "[\"b\"]"
    );
    drop(table);
    drop(tx);
    drop(db);
    let engine = Arc::new(Engine::open(&db_path).unwrap());
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: projects[0].clone(),
            endpoint: "http://127.0.0.1:9/mcp".into(),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let db = Database::open(&db_path).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx
        .open_table(TableDefinition::<&str, &str>::new("integration_mcp_policy"))
        .unwrap();
    assert!(table.get(projects[0].as_str()).unwrap().is_none());
    assert_eq!(
        table.get(projects[1].as_str()).unwrap().unwrap().value(),
        "[\"b\"]"
    );
    drop(table);
    drop(tx);
    drop(db);
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_dir(root);
}
