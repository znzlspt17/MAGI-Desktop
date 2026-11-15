#[tokio::test]
async fn mcp_rejects_unencrypted_non_loopback_and_userinfo() {
    use integrations::mcp::McpClient;
    assert!(McpClient::connect("http://example.com/mcp")
        .await
        .unwrap_err()
        .contains("HTTPS"));
    assert!(McpClient::connect("https://user@example.com/mcp")
        .await
        .unwrap_err()
        .contains("userinfo"));
}

#[tokio::test]
async fn mcp_initializes_and_lists_paginated_tools_with_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|s| {
                            s.strip_prefix("content-length: ")
                                .and_then(|n| n.parse().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(request).unwrap();
            if step > 0 {
                assert!(request.to_lowercase().contains("mcp-session-id: session-1"));
            }
            let body = match step {
                0 => {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}}}"#
                }
                1 => "",
                2 => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read_a","inputSchema":{"type":"object"}}],"nextCursor":"next"}}"#
                }
                3 => {
                    r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"read_b","inputSchema":{"type":"object"}}]}}"#
                }
                _ => {
                    r#"{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"failed"}],"structuredContent":{"reason":"missing"},"isError":true}}"#
                }
            };
            let response = format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nMcp-Session-Id: session-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", if step == 1 { "202 Accepted" } else { "200 OK" }, body.len(), body);
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = integrations::mcp::McpClient::connect(&format!("http://{address}/mcp"))
        .await
        .unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[1]["name"], "read_b");
    let result = client
        .call_tool(
            "read_b",
            serde_json::json!({}),
            &tools[1]["inputSchema"],
            "C:/project",
            &integrations::mcp::CallAuthorization::ReadOnlyAllowlist(vec!["read_b".into()]),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.structured_content.unwrap()["reason"], "missing");
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_tool_authorization_rejects_changed_arguments_before_network() {
    use integrations::mcp::CallAuthorization;
    let auth = CallAuthorization::Approved {
        name: "write".into(),
        arguments: serde_json::json!({"path":"a"}),
        cwd: "C:/project".into(),
    };
    assert!(!auth.permits("write", &serde_json::json!({"path":"b"}), "C:/project"));
    assert!(!auth.permits("write", &serde_json::json!({"path":"a"}), "C:/other"));
    assert!(auth.permits("write", &serde_json::json!({"path":"a"}), "C:/project"));
}

#[tokio::test]
async fn mcp_rejects_legacy_non_streamable_protocol_version() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        let _ = stream.read(&mut buffer).await.unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let error = integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap_err();
    assert!(error.contains("unsupported protocol"), "{error}");
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_cancelled_connect_does_not_wait_for_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let error = integrations::mcp::McpClient::connect_cancelable(
        &format!("http://{}/mcp", listener.local_addr().unwrap()),
        token,
    )
    .await
    .unwrap_err();
    assert!(error.contains("cancelled"));
}

#[tokio::test]
async fn mcp_sends_transient_bearer_during_initialization() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            assert!(request.contains("authorization: bearer fake-test-value"));
            let body = if step == 0 {
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#
            } else {
                ""
            };
            stream.write_all(format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==0{"200 OK"}else{"202 Accepted"},body.len(),body).as_bytes()).await.unwrap();
        }
    });
    let client = integrations::mcp::McpClient::connect_authenticated(
        &format!("http://{addr}/mcp"),
        "fake-test-value".into(),
    )
    .await
    .unwrap();
    assert!(!format!("{client:?}").contains("fake-test-value"));
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_sse_response_ignores_unrelated_events() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            let _ = stream.read(&mut b).await.unwrap();
            let body = if step == 0 {
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n"
            } else {
                ""
            };
            stream.write_all(format!("HTTP/1.1 {}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==0{"200 OK"}else{"202 Accepted"},body.len(),body).as_bytes()).await.unwrap();
        }
    });
    integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_request_times_out_when_server_never_responds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    });
    let start = std::time::Instant::now();
    let error = integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap_err();
    assert!(error.contains("timed out"));
    assert!(start.elapsed() < std::time::Duration::from_secs(19));
    server.abort();
}

#[tokio::test]
async fn mcp_rejects_redirect_without_following_it() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut b = [0; 4096];
        let _ = socket.read(&mut b).await.unwrap();
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/mcp\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let error = integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap_err();
    assert!(error.contains("302"));
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_sse_returns_on_response_without_waiting_for_stream_close() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let _ = first.read(&mut buf).await.unwrap();
        let body="data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n";
        first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",body.len(),body).as_bytes()).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let _ = second.read(&mut buf).await.unwrap();
        second
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        drop(first);
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp")),
    )
    .await
    .expect("must not wait for stream close")
    .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn mcp_unique_empty_cursors_stop_with_bounded_page_count() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            let mut data = Vec::new();
            let mut chunk = [0; 4096];
            while let Ok(n) = s.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&chunk[..n]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    if serde_json::from_slice::<serde_json::Value>(&data[pos + 4..]).is_ok() {
                        break;
                    }
                }
            }
            let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                break;
            };
            let Ok(req) = serde_json::from_slice::<serde_json::Value>(&data[pos + 4..]) else {
                break;
            };
            let method = req["method"].as_str().unwrap_or("");
            let body=match method {"initialize"=>serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}).to_string(),"notifications/initialized"=>String::new(),_=>{let n=seen.fetch_add(1,Ordering::SeqCst);serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":{"tools":[],"nextCursor":format!("cursor-{n}")}}).to_string()}};
            let resp=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
            if s.write_all(resp.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let client = integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), client.list_tools()).await;
    task.abort();
    assert!(result.is_ok(), "목록 전체 시간이 제한되어야 함");
    assert!(result.unwrap().is_err());
    assert!(count.load(Ordering::SeqCst) <= 64);
}

#[tokio::test]
async fn mcp_cancellation_interrupts_stalled_list() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for step in 0..3 {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            let _ = s.read(&mut b).await;
            if step == 2 {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                return;
            }
            let body = if step == 0 {
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#
            } else {
                ""
            };
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
            let _ = s.write_all(response.as_bytes()).await;
        }
    });
    let client = integrations::mcp::McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.list_tools_cancelable(cancel),
    )
    .await;
    task.abort();
    assert!(result.unwrap().unwrap_err().contains("cancelled"));
}
