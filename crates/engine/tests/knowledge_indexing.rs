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
fn data(response: ResponseData) -> serde_json::Value {
    match response {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    }
}
fn doc(id: &str, version: u64, source: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"id":id,"version":version,"source":source,"text":text})
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn delayed_server() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut b = [0; 4096];
            let n = stream.read(&mut b).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&b[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                let size = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length: ")
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + size {
                    break;
                }
            }
        }
        ready_tx.send(()).unwrap();
        let _ = release_rx.await;
        let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
        let _=stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await;
    });
    (
        format!("http://{address}/embeddings"),
        ready_rx,
        release_tx,
        server,
    )
}
async fn configure(engine: &Arc<Engine>, id: &str, endpoint: &str) {
    send(
        engine,
        Command::ConfigureEmbedding {
            project_id: id.into(),
            endpoint: endpoint.into(),
            model: "fake-model".into(),
            version: "v1".into(),
            dimensions: 2,
            credential: domain::SecretString("fake-key".into()),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_index_does_not_publish_late_vector() {
    let f = Fixture::new();
    let engine = f.engine();
    let id = f.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("d", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    let (endpoint, ready, release, server) = delayed_server().await;
    configure(&engine, &id, &endpoint).await;
    send(
        &engine,
        Command::IndexKnowledge {
            project_id: id.clone(),
            document_id: "d".into(),
        },
    )
    .await
    .unwrap();
    ready.await.unwrap();
    send(
        &engine,
        Command::CancelKnowledgeIndex {
            project_id: id.clone(),
            document_id: "d".into(),
        },
    )
    .await
    .unwrap();
    release.send(()).unwrap();
    server.await.unwrap();
    let result = send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: id.clone(),
            model: serde_json::json!({"model":"fake-model","version":"v1","dimensions":2}),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10,
        },
    )
    .await;
    assert!(result.is_err());
}

async fn status(engine: &Arc<Engine>, id: &str) -> String {
    let v = data(
        send(
            engine,
            Command::KnowledgeIndexStatus {
                project_id: id.into(),
                document_id: "d".into(),
            },
        )
        .await
        .unwrap(),
    );
    v["status"].as_str().unwrap().into()
}
fn status_is_finished(status: &serde_json::Value) -> bool {
    status["resources_released"] == true
        && matches!(
            status["status"].as_str(),
            Some("completed" | "cancelled" | "failed" | "uncertain" | "interrupted")
        )
}

#[test]
fn publishing_is_not_a_finished_indexing_job() {
    assert!(!status_is_finished(
        &serde_json::json!({"status":"publishing","resources_released":false})
    ));
    assert!(!status_is_finished(
        &serde_json::json!({"status":"completed","resources_released":false})
    ));
    assert!(status_is_finished(
        &serde_json::json!({"status":"completed","resources_released":true})
    ));
}

async fn wait_finished(engine: &Arc<Engine>, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = data(
                send(
                    engine,
                    Command::KnowledgeIndexStatus {
                        project_id: id.to_owned(),
                        document_id: "d".into(),
                    },
                )
                .await
                .unwrap(),
            );
            if status_is_finished(&value) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn indexing_success_persists_real_vector_and_never_replays_after_restart() {
    let f = Fixture::new();
    let engine = f.engine();
    let id = f.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("d", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    let (endpoint, ready, release, server) = delayed_server().await;
    configure(&engine, &id, &endpoint).await;
    send(
        &engine,
        Command::IndexKnowledge {
            project_id: id.clone(),
            document_id: "d".into(),
        },
    )
    .await
    .unwrap();
    ready.await.unwrap();
    assert_eq!(status(&engine, &id).await, "running");
    release.send(()).unwrap();
    server.await.unwrap();
    wait_finished(&engine, &id).await;
    assert_eq!(status(&engine, &id).await, "completed");
    drop(engine);
    let bytes = std::fs::read(&f.db).unwrap();
    assert!(!bytes.windows(b"fake-key".len()).any(|w| w == b"fake-key"));
    let engine = f.engine();
    assert_eq!(status(&engine, &id).await, "completed");
    assert!(send(
        &engine,
        Command::IndexKnowledge {
            project_id: id.clone(),
            document_id: "d".into()
        }
    )
    .await
    .is_err());
    let found = data(
        send(
            &engine,
            Command::SearchKnowledgeVector {
                project_id: id,
                model: serde_json::json!({"model":"fake-model","version":"v1","dimensions":2}),
                vector: vec![1.0, 0.0],
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(found["results"][0]["document"]["id"], "d");
}

#[tokio::test]
async fn changing_document_while_indexing_never_publishes_stale_vector() {
    let f = Fixture::new();
    let engine = f.engine();
    let id = f.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("d", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    let (endpoint, ready, release, server) = delayed_server().await;
    configure(&engine, &id, &endpoint).await;
    send(
        &engine,
        Command::IndexKnowledge {
            project_id: id.clone(),
            document_id: "d".into(),
        },
    )
    .await
    .unwrap();
    ready.await.unwrap();
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("d", 2, "updated", "new"),
        },
    )
    .await
    .unwrap();
    release.send(()).unwrap();
    server.await.unwrap();
    wait_finished(&engine, &id).await;
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: id,
            model: serde_json::json!({"model":"fake-model","version":"v1","dimensions":2}),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn reconfiguring_a_to_b_to_a_cannot_publish_old_result() {
    let f = Fixture::new();
    let engine = f.engine();
    let id = f.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("d", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    let (endpoint, ready, release, server) = delayed_server().await;
    configure(&engine, &id, &endpoint).await;
    send(
        &engine,
        Command::IndexKnowledge {
            project_id: id.clone(),
            document_id: "d".into(),
        },
    )
    .await
    .unwrap();
    ready.await.unwrap();
    configure(&engine, &id, "http://127.0.0.1:1/embeddings").await;
    configure(&engine, &id, &endpoint).await;
    release.send(()).unwrap();
    server.await.unwrap();
    wait_finished(&engine, &id).await;
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: id,
            model: serde_json::json!({"model":"fake-model","version":"v1","dimensions":2}),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn shared_capacity_and_document_exclusivity_prevent_unbounded_workers() {
    let f = Fixture::new();
    let engine = f.engine();
    let mut jobs = Vec::new();
    for n in 0..4 {
        let id = f.project(&engine, &format!("project-{n}"));
        send(
            &engine,
            Command::PutKnowledge {
                project_id: id.clone(),
                document: doc("d", 1, "source", "text"),
            },
        )
        .await
        .unwrap();
        let (endpoint, ready, release, server) = delayed_server().await;
        configure(&engine, &id, &endpoint).await;
        send(
            &engine,
            Command::IndexKnowledge {
                project_id: id.clone(),
                document_id: "d".into(),
            },
        )
        .await
        .unwrap();
        ready.await.unwrap();
        assert!(send(
            &engine,
            Command::IndexKnowledge {
                project_id: id.clone(),
                document_id: "d".into()
            }
        )
        .await
        .is_err());
        jobs.push((id, release, server));
    }
    let extra = f.project(&engine, "extra");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: extra.clone(),
            document: doc("d", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    configure(&engine, &extra, "http://127.0.0.1:1/embeddings").await;
    let err = send(
        &engine,
        Command::IndexKnowledge {
            project_id: extra,
            document_id: "d".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(err.contains("capacity"), "{err}");
    for (id, release, server) in jobs {
        send(
            &engine,
            Command::CancelKnowledgeIndex {
                project_id: id.clone(),
                document_id: "d".into(),
            },
        )
        .await
        .unwrap();
        release.send(()).unwrap();
        server.await.unwrap();
        wait_finished(&engine, &id).await;
        let state = data(
            send(
                &engine,
                Command::KnowledgeIndexStatus {
                    project_id: id,
                    document_id: "d".into(),
                },
            )
            .await
            .unwrap(),
        );
        assert_eq!(state["resources_released"], true);
    }
}

#[tokio::test]
async fn persisted_running_job_is_interrupted_after_restart_without_replay() {
    use redb::{Database, TableDefinition};
    let f = Fixture::new();
    let engine = f.engine();
    let id = f.project(&engine, "project");
    drop(engine);
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new(
                "integration_embedding_status",
            ))
            .unwrap();
        let key = serde_json::to_string(&(id.as_str(), "d")).unwrap();
        table.insert(key.as_str(), "running").unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = f.engine();
    let state = data(
        send(
            &engine,
            Command::KnowledgeIndexStatus {
                project_id: id.clone(),
                document_id: "d".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(state["status"], "interrupted");
    assert_eq!(state["resources_released"], true);
    assert!(send(
        &engine,
        Command::IndexKnowledge {
            project_id: id,
            document_id: "d".into()
        }
    )
    .await
    .is_err());
}
