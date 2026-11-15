#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
async fn discovery_returns_metadata_for_configured_project() {
    let root = std::env::temp_dir().join(format!("magi-oauth-{}", uuid::Uuid::new_v4()));
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
    let server = tokio::spawn(async move {
        for step in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
            }
            let body = if step == 0 {
                format!(
                    r#"{{"resource":"http://{address}/mcp","authorization_servers":["http://{address}"]}}"#
                )
            } else {
                format!(
                    r#"{{"issuer":"http://{address}","authorization_endpoint":"http://{address}/authorize","token_endpoint":"http://{address}/token","code_challenge_methods_supported":["S256"]}}"#
                )
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
    });
    let result = send(
        &engine,
        Command::DiscoverMcpOAuth {
            project_id,
            metadata_url: format!("http://{address}/metadata"),
        },
    )
    .await
    .unwrap();
    match result {
        ResponseData::Data(data) => {
            assert_eq!(data["token_endpoint"], format!("http://{address}/token"))
        }
        _ => panic!("data expected"),
    }
    server.await.unwrap();
    drop(engine);
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}

#[tokio::test]
async fn callback_is_single_use_and_endpoint_aba_clears_flow() {
    let root = std::env::temp_dir().join(format!("magi-oauth-{}", uuid::Uuid::new_v4()));
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = format!("http://{address}/mcp");
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: project_id.clone(),
            endpoint: endpoint.clone(),
        },
    )
    .await
    .unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ready = Some(ready_tx);
        let mut release = Some(release_rx);
        for step in 0..6 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
            }
            let body = match step {
                0 | 3 => format!(
                    r#"{{"resource":"http://{address}/mcp","authorization_servers":["http://{address}"]}}"#
                ),
                1 | 4 => format!(
                    r#"{{"issuer":"http://{address}","authorization_endpoint":"http://{address}/authorize","token_endpoint":"http://{address}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
                _ => r#"{"access_token":"fixture-token","token_type":"Bearer"}"#.to_string(),
            };
            if step == 5 {
                ready.take().unwrap().send(()).unwrap();
                release.take().unwrap().await.unwrap();
            }
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
    });
    let begin = || Command::BeginMcpOAuth {
        project_id: project_id.clone(),
        metadata_url: format!("http://{address}/metadata"),
        client_id: "fixture-client".into(),
        redirect_uri: "http://127.0.0.1:43210/callback".into(),
    };
    let get_callback = |response| {
        let ResponseData::Data(value) = response else {
            panic!("data expected")
        };
        let url = value["authorization_url"].as_str().unwrap();
        let state = url
            .split(['?', '&'])
            .find_map(|part| part.strip_prefix("state="))
            .unwrap();
        format!("http://127.0.0.1:43210/callback?state={state}&code=fixture")
    };
    let callback = get_callback(send(&engine, begin()).await.unwrap());
    let other = match send(
        &engine,
        Command::CreateProject {
            name: "other".into(),
            path: root.to_string_lossy().into_owned(),
        },
    )
    .await
    .unwrap()
    {
        ResponseData::Project(p) => p.id,
        _ => panic!("project expected"),
    };
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: other.clone(),
            endpoint: endpoint.clone(),
        },
    )
    .await
    .unwrap();
    assert!(send(
        &engine,
        Command::CompleteMcpOAuth {
            project_id: other,
            callback: domain::SecretString(callback.clone())
        }
    )
    .await
    .is_err());
    let complete = |callback: String| Command::CompleteMcpOAuth {
        project_id: project_id.clone(),
        callback: domain::SecretString(callback),
    };
    send(&engine, complete(callback.clone())).await.unwrap();
    assert!(send(&engine, complete(callback)).await.is_err());
    let stale = get_callback(send(&engine, begin()).await.unwrap());
    let pending_engine = engine.clone();
    let pending_command = complete(stale);
    let pending = tokio::spawn(async move { send(&pending_engine, pending_command).await });
    ready_rx.await.unwrap();
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: project_id.clone(),
            endpoint: "http://127.0.0.1:9/other".into(),
        },
    )
    .await
    .unwrap();
    send(
        &engine,
        Command::ConfigureMcp {
            project_id: project_id.clone(),
            endpoint,
        },
    )
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    assert!(pending.await.unwrap().is_err());
    server.await.unwrap();
    drop(engine);
    let db = std::fs::read(root.join("db.redb")).unwrap();
    assert!(!db
        .windows(b"fixture-token".len())
        .any(|w| w == b"fixture-token"));
    let _ = std::fs::remove_file(root.join("db.redb"));
    let _ = std::fs::remove_dir(root);
}
