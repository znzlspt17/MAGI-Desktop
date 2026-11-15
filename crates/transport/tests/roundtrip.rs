#![cfg(windows)]
use domain::{Command, Request, Response, ResponseData};
use tokio::sync::{mpsc, oneshot};
#[tokio::test]
async fn current_user_round_trip() {
    let name = format!(r"\\.\pipe\magi-roundtrip-{}", std::process::id());
    let (tx, mut rx) = mpsc::channel(2);
    let task = tokio::spawn({
        let name = name.clone();
        async move { transport::serve(&name, tx).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let handler = tokio::spawn(async move {
        let (request, reply): (Request, oneshot::Sender<Response>) = rx.recv().await.unwrap();
        reply
            .send(Response {
                id: request.id,
                result: Ok(ResponseData::Empty),
            })
            .unwrap();
    });
    let request = Request {
        id: "roundtrip".into(),
        command: Command::Status,
    };
    assert_eq!(
        transport::request(&name, &request).await.unwrap().id,
        "roundtrip"
    );
    handler.await.unwrap();
    task.abort();
}
