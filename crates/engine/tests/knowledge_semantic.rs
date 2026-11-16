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
async fn semantic_search_without_profile_reports_text_fallback() {
    let root = std::env::temp_dir().join(format!("magi-semantic-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let engine = Arc::new(Engine::open(&root.join("db.redb")).unwrap());
    let ResponseData::Project(project) = send(
        &engine,
        Command::CreateProject {
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap() else {
        panic!("project")
    };
    send(&engine, Command::PutKnowledge { project_id: project.id.clone(), document: serde_json::json!({"id":"d","version":1,"source":"fixture","text":"sample query"}) }).await.unwrap();
    let ResponseData::Data(value) = send(
        &engine,
        Command::SearchKnowledge {
            project_id: project.id,
            query: "sample".into(),
            semantic: true,
        },
    )
    .await
    .unwrap() else {
        panic!("results")
    };
    assert_eq!(value["mode"], "text_fallback");
    assert_eq!(value["results"].as_array().unwrap().len(), 1);
    drop(engine);
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}

#[tokio::test]
async fn semantic_search_uses_persisted_vectors_and_discards_old_profile_generation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!("magi-semantic-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let engine = Arc::new(Engine::open(&root.join("db.redb")).unwrap());
    let ResponseData::Project(project) = send(
        &engine,
        Command::CreateProject {
            name: "fixture".into(),
            path: root.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap() else {
        panic!("project")
    };
    let id = project.id;
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: serde_json::json!({"id":"d","version":1,"source":"fixture","text":"sample"}),
        },
    )
    .await
    .unwrap();
    send(
        &engine,
        Command::SetKnowledgeEmbedding {
            project_id: id.clone(),
            document_id: "d".into(),
            version: 1,
            model: serde_json::json!({"model":"fixture-model","version":"v1","dimensions":2}),
            vector: vec![1.0, 0.0],
        },
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(2);
    let (release_tx, mut release_rx) = tokio::sync::mpsc::channel(2);
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let size = header
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
            if step > 0 {
                ready_tx.send(()).await.unwrap();
                release_rx.recv().await.unwrap();
            }
            let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
    });
    let configure = |model: &str| Command::ConfigureEmbedding {
        project_id: id.clone(),
        endpoint: format!("http://{addr}/embeddings"),
        model: model.into(),
        version: "v1".into(),
        dimensions: 2,
        credential: domain::SecretString("fixture-key".into()),
    };
    send(&engine, configure("fixture-model")).await.unwrap();
    let query = || Command::SearchKnowledge {
        project_id: id.clone(),
        query: "sample".into(),
        semantic: true,
    };
    let ResponseData::Data(value) = send(&engine, query()).await.unwrap() else {
        panic!("results")
    };
    assert_eq!(value["mode"], "semantic");
    assert_eq!(value["results"].as_array().unwrap().len(), 1);
    let pending_engine = engine.clone();
    let pending_query = query();
    let pending = tokio::spawn(async move { send(&pending_engine, pending_query).await });
    ready_rx.recv().await.unwrap();
    send(&engine, configure("other-model")).await.unwrap();
    send(&engine, configure("fixture-model")).await.unwrap();
    release_tx.send(()).await.unwrap();
    assert!(pending.await.unwrap().is_err());
    let pending_engine = engine.clone();
    let pending_query = query();
    let pending = tokio::spawn(async move { send(&pending_engine, pending_query).await });
    ready_rx.recv().await.unwrap();
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: serde_json::json!({"id":"d","version":2,"source":"fixture","text":"changed"}),
        },
    )
    .await
    .unwrap();
    release_tx.send(()).await.unwrap();
    let ResponseData::Data(value) = pending.await.unwrap().unwrap() else {
        panic!("results")
    };
    assert!(value["results"].as_array().unwrap().is_empty());
    server.await.unwrap();
    drop(engine);
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}
