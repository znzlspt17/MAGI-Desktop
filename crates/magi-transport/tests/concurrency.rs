#![cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::{mpsc, oneshot};
static NEXT: AtomicUsize = AtomicUsize::new(0);
fn name() -> String {
    format!(
        r"\\.\pipe\magi-ipc-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
#[tokio::test]
async fn concurrent_requests_survive_idle_client_and_duplicate_host_rejected() {
    let pipe = name();
    let (tx, mut rx) = mpsc::channel::<(u64, oneshot::Sender<u64>)>(64);
    let server = tokio::spawn({
        let pipe = pipe.clone();
        let tx = tx.clone();
        async move { magi_transport::serve_typed(&pipe, tx).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let idle = ClientOptions::new().open(&pipe).unwrap();
    let duplicate = magi_transport::serve_typed(&pipe, tx).await;
    assert!(duplicate.is_err());
    let responder = tokio::spawn(async move {
        while let Some((n, reply)) = rx.recv().await {
            let _ = reply.send(n + 1);
        }
    });
    let mut clients = tokio::task::JoinSet::new();
    for n in 0..48u64 {
        let pipe = pipe.clone();
        clients.spawn(async move { (n, magi_transport::request_typed::<_, u64>(&pipe, &n).await) });
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = clients.join_next().await {
            let (n, response) = result.unwrap();
            assert_eq!(response.unwrap(), n + 1);
        }
    })
    .await
    .unwrap();
    drop(idle);
    server.abort();
    responder.abort();
}
#[tokio::test]
async fn request_rejects_mismatched_response_id() {
    use magi_core::{Command, Request, Response, ResponseData};
    let pipe = name();
    let (tx, mut rx) = mpsc::channel::<(Request, oneshot::Sender<Response>)>(1);
    let server = tokio::spawn({
        let pipe = pipe.clone();
        async move { magi_transport::serve_typed(&pipe, tx).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let responder = tokio::spawn(async move {
        let (_, reply) = rx.recv().await.unwrap();
        reply
            .send(Response {
                id: "wrong".into(),
                result: Ok(ResponseData::Empty),
            })
            .unwrap();
    });
    let request = Request {
        id: "expected".into(),
        command: Command::Status,
    };
    assert!(magi_transport::request(&pipe, &request).await.is_err());
    responder.await.unwrap();
    server.abort();
}
#[tokio::test]
async fn cancelling_server_closes_idle_connections() {
    use tokio::io::AsyncReadExt;
    let pipe = name();
    let (tx, _rx) = mpsc::channel::<(u64, oneshot::Sender<u64>)>(1);
    let server = tokio::spawn({
        let pipe = pipe.clone();
        async move { magi_transport::serve_typed(&pipe, tx).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut client = ClientOptions::new().open(&pipe).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.abort();
    let _ = server.await;
    let mut byte = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}
