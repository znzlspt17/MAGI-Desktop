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
async fn configured_endpoint_survives_restart_and_lists_tools_only_for_its_project() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let a = fixture.project(&engine, "a");
    let b = fixture.project(&engine, "b");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            s.read(&mut b).await.unwrap();
            let body = match step {
                0 => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
                1 => "",
                _ => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read","inputSchema":{"type":"object"}}]}}"#
                }
            };
            s.write_all(format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==1{"202 Accepted"}else{"200 OK"},body.len(),body).as_bytes()).await.unwrap();
        }
    });
    assert!(send(
        &engine,
        Command::ConfigureMcp {
            project_id: a.clone(),
            endpoint: "http://example.com/mcp".into()
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::ConfigureMcp {
            project_id: "missing".into(),
            endpoint: format!("http://{addr}/mcp")
        }
    )
    .await
    .is_err());
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: a.clone(),
            endpoint: format!("http://{addr}/mcp"),
        },
    )
    .await
    .unwrap();
    assert!(send(&engine, Command::ListMcpTools { project_id: b })
        .await
        .is_err());
    drop(engine);
    let engine = fixture.engine();
    let result = send(&engine, Command::ListMcpTools { project_id: a })
        .await
        .unwrap();
    let data = match result {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    };
    assert_eq!(data["tools"][0]["name"], "read");
    server.await.unwrap();
}

#[tokio::test]
async fn changed_then_restored_endpoint_discards_inflight_list_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ready = Some(ready_tx);
        let mut release = Some(release_rx);
        for step in 0..3 {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            s.read(&mut b).await.unwrap();
            if step == 2 {
                ready.take().unwrap().send(()).unwrap();
                release.take().unwrap().await.unwrap();
            }
            let body = match step {
                0 => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
                1 => "",
                _ => r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#,
            };
            s.write_all(format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==1{"202 Accepted"}else{"200 OK"},body.len(),body).as_bytes()).await.unwrap();
        }
    });
    let endpoint = format!("http://{addr}/mcp");
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: id.clone(),
            endpoint: endpoint.clone(),
        },
    )
    .await
    .unwrap();
    let request = tokio::spawn(engine.clone().handle_async(Request {
        id: "list".into(),
        command: Command::ListMcpTools {
            project_id: id.clone(),
        },
    }));
    ready_rx.await.unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send(&engine, Command::Status)
    )
    .await
    .unwrap()
    .is_ok());
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: id.clone(),
            endpoint: "http://127.0.0.1:1/mcp".into(),
        },
    )
    .await
    .unwrap();
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: id,
            endpoint,
        },
    )
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    let err = request.await.unwrap().result.unwrap_err();
    assert!(err.contains("endpoint changed"), "{err}");
    server.await.unwrap();
}

#[tokio::test]
async fn configuring_endpoint_invalidates_prior_policy_atomically() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("integration_mcp_policy"))
            .unwrap();
        table.insert(id.as_str(), "[\"old\"]").unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: id.clone(),
            endpoint: "http://127.0.0.1:1/mcp".into(),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx
        .open_table(TableDefinition::<&str, &str>::new("integration_mcp_policy"))
        .unwrap();
    assert!(table.get(id.as_str()).unwrap().is_none());
}
