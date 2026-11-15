use integrations::mcp::{CallAuthorization, McpClient};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    responses: Vec<(&'static str, String)>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let task = tokio::spawn(async move {
        for (content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .and_then(|v| v.parse().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let status = if body.is_empty() {
                "202 Accepted"
            } else {
                "200 OK"
            };
            let raw = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            socket.write_all(raw.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}/mcp"), calls, task)
}

fn init() -> Vec<(&'static str, String)> {
    vec![
        (
            "application/json",
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}).to_string(),
        ),
        ("application/json", String::new()),
    ]
}
fn response(id: u64, result: Value) -> (&'static str, String) {
    (
        "application/json",
        json!({"jsonrpc":"2.0","id":id,"result":result}).to_string(),
    )
}
fn auth() -> CallAuthorization {
    CallAuthorization::ReadOnlyAllowlist(vec!["read".into()])
}

#[tokio::test]
async fn malformed_schema_returns_error_without_call_and_client_remains_usable() {
    let mut replies = init();
    replies.push(response(2, json!({"tools":[]})));
    let (endpoint, calls, task) = server(replies).await;
    let client = McpClient::connect(&endpoint).await.unwrap();
    let err = client
        .call_tool(
            "read",
            json!({}),
            &json!({"type":"bogus"}),
            "project",
            &auth(),
        )
        .await;
    assert!(err.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(client.list_tools().await.unwrap().is_empty());
    task.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn sse_batch_selects_matching_response_and_ignores_other_messages() {
    let batch = json!([{"jsonrpc":"2.0","method":"notifications/progress"},{"jsonrpc":"2.0","id":99,"result":{}},{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}]);
    let replies = vec![
        ("text/event-stream", format!("data: {batch}\n\n")),
        ("application/json", String::new()),
        response(2, json!({"tools":[]})),
    ];
    let (endpoint, _, task) = server(replies).await;
    let client = McpClient::connect(&endpoint).await.unwrap();
    assert!(client.list_tools().await.unwrap().is_empty());
    task.await.unwrap();
}

#[tokio::test]
async fn malformed_tool_results_are_errors_and_valid_empty_results_are_preserved() {
    let mut replies = init();
    let bad = vec![
        Value::Null,
        json!(42),
        json!("bad"),
        json!({"content":"bad"}),
        json!({"content":[],"isError":"bad"}),
        json!({}),
    ];
    for (i, result) in bad.iter().enumerate() {
        replies.push(response(2 + i as u64, result.clone()));
    }
    replies.push(response(8, json!({"content":[]})));
    replies.push(response(9, json!({"content":[],"isError":true})));
    replies.push(response(10, json!({"content":[],"isError":false})));
    let (endpoint, _, task) = server(replies).await;
    let client = McpClient::connect(&endpoint).await.unwrap();
    for _ in &bad {
        assert!(client
            .call_tool(
                "read",
                json!({}),
                &json!({"type":"object"}),
                "project",
                &auth()
            )
            .await
            .is_err());
    }
    for expected in [false, true, false] {
        let result = client
            .call_tool(
                "read",
                json!({}),
                &json!({"type":"object"}),
                "project",
                &auth(),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, expected);
        assert!(result.content.is_empty());
    }
    task.await.unwrap();
}

#[tokio::test]
async fn sse_batch_without_final_separator_is_selected_at_eof() {
    let batch = json!([{"jsonrpc":"2.0","method":"notifications/progress"},{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}]);
    let replies = vec![
        ("text/event-stream", format!("data: {batch}")),
        ("application/json", String::new()),
    ];
    let (endpoint, _, task) = server(replies).await;
    McpClient::connect(&endpoint).await.unwrap();
    task.await.unwrap();
}
