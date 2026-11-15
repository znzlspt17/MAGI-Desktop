use engine::agent_loop::{ModelDriver, OpenAiModels};
use provider::{OpenAiConfig, OpenAiProvider};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

fn provider(endpoint: String, model: &str) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig {
        endpoint,
        model: model.into(),
        api_key: "fake-test-key".into(),
        max_output_bytes: 64 * 1024,
    })
    .unwrap()
}

async fn server(response: Vec<u8>) -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let body_offset;
        loop {
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(p) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                body_offset = p + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&request[..body_offset]);
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.trim().parse().unwrap())
            })
            .unwrap();
        while request.len() < body_offset + length {
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            request.extend_from_slice(&buf[..n]);
        }
        tx.send(request[body_offset..body_offset + length].to_vec())
            .unwrap();
        let _ = socket.write_all(&response).await;
    });
    (format!("http://{addr}/v1/responses"), rx)
}
fn wire(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
}
fn tools() -> Vec<Value> {
    vec![
        json!({"type":"function","name":"read","parameters":{"type":"object","properties":{},"additionalProperties":false}}),
    ]
}

#[tokio::test]
async fn streams_more_than_channel_capacity_and_preserves_provider_contract() {
    let mut body = String::new();
    for _ in 0..80 {
        body.push_str("data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n");
    }
    body.push_str("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"reason\"},{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"read\",\"arguments\":\"{}\"}],\"usage\":{\"total_tokens\":12}}}\n\n");
    let (endpoint, request) = server(wire(&body)).await;
    let adapter = OpenAiModels(HashMap::from([(
        "implementer".into(),
        provider(endpoint, "gpt-6"),
    )]));
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        adapter.generate(
            "implementer",
            json!([{"role":"user","content":"hello"}]),
            tools(),
            CancellationToken::new(),
            37,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let request: Value = serde_json::from_slice(&request.await.unwrap()).unwrap();
    assert_eq!(request["model"], "gpt-6");
    assert_eq!(request["max_output_tokens"], 37);
    assert_eq!(request["tools"][0]["name"], "read");
    assert_eq!(result.text.len(), 80);
    assert_eq!(result.output[0]["type"], "reasoning");
    assert_eq!(result.tool_calls[0].name, "read");
    assert_eq!(result.usage.unwrap()["total_tokens"], 12);
}

#[tokio::test]
async fn unknown_role_fails_without_fallback_and_provider_errors_are_preserved() {
    let (endpoint,request)=server(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\nConnection: close\r\n\r\nfake-test-key".to_vec()).await;
    let adapter = OpenAiModels(HashMap::from([(
        "reviewer".into(),
        provider(endpoint, "gpt-6"),
    )]));
    assert_eq!(
        adapter
            .generate(
                "implementer",
                json!([]),
                vec![],
                CancellationToken::new(),
                10
            )
            .await
            .unwrap_err(),
        "role model unavailable"
    );
    let error = adapter
        .generate("reviewer", json!([]), vec![], CancellationToken::new(), 10)
        .await
        .unwrap_err();
    assert!(!error.contains("fake-test-key"));
    assert!(!error.is_empty());
    assert!(request.await.is_ok());
}

#[tokio::test]
async fn cancellation_interrupts_stalled_response() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        let _ = ready_tx.send(());
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let adapter = OpenAiModels(HashMap::from([(
        "coordinator".into(),
        provider(format!("http://{addr}/v1/responses"), "gpt-6"),
    )]));
    let cancel = CancellationToken::new();
    let work = adapter.generate("coordinator", json!([]), vec![], cancel.clone(), 20);
    let stop = async {
        ready_rx.await.unwrap();
        cancel.cancel();
    };
    let (result, _) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(work, stop) })
            .await
            .unwrap();
    assert!(result.is_err());
    server.abort();
}

#[tokio::test]
async fn selects_provider_configured_for_each_role() {
    let completed="data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"total_tokens\":3}}}\n\n";
    let (first, first_request) = server(wire(completed)).await;
    let (second, second_request) = server(wire(completed)).await;
    let adapter = OpenAiModels(HashMap::from([
        ("coordinator".into(), provider(first, "gpt-6")),
        ("reviewer".into(), provider(second, "gpt-7")),
    ]));
    adapter
        .generate("reviewer", json!([]), vec![], CancellationToken::new(), 20)
        .await
        .unwrap();
    adapter
        .generate(
            "coordinator",
            json!([]),
            vec![],
            CancellationToken::new(),
            20,
        )
        .await
        .unwrap();
    let one: Value = serde_json::from_slice(&first_request.await.unwrap()).unwrap();
    let two: Value = serde_json::from_slice(&second_request.await.unwrap()).unwrap();
    assert_eq!(one["model"], "gpt-6");
    assert_eq!(two["model"], "gpt-7");
}
