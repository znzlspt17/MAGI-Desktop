use magi_provider::{OpenAiConfig, OpenAiProvider};
#[test]
fn rejects_old_model_and_unsafe_url() {
    let config = |model: &str, endpoint: &str| OpenAiConfig {
        endpoint: endpoint.into(),
        model: model.into(),
        api_key: "fake-test-key".into(),
        max_output_bytes: 4096,
    };
    assert!(OpenAiProvider::new(config("gpt-4o", "https://api.openai.com/v1/responses")).is_err());
    assert!(OpenAiProvider::new(config("gpt-6", "http://example.com/v1/responses")).is_err());
    assert!(OpenAiProvider::new(config("gpt-6", "https://fake@example.com/v1/responses")).is_err());
    assert!(OpenAiProvider::new(config(
        "gpt-7-future",
        "https://api.openai.com/v1/responses"
    ))
    .is_ok());
}

async fn server(response: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 8192];
        let _ = socket.read(&mut buf).await;
        for part in response.as_bytes().chunks(7) {
            socket.write_all(part).await.unwrap();
        }
    });
    format!("http://{address}/v1/responses")
}
fn config(endpoint: String) -> OpenAiConfig {
    OpenAiConfig {
        endpoint,
        model: "gpt-6".into(),
        api_key: "fake-test-key".into(),
        max_output_bytes: 4096,
    }
}
async fn generate(
    provider: &OpenAiProvider,
) -> Result<magi_provider::Generation, magi_provider::ProviderError> {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    provider.generate(serde_json::json!("hi"), vec![serde_json::json!({"type":"function","name":"run","parameters":{"type":"object","properties":{},"additionalProperties":false}})], tokio_util::sync::CancellationToken::new(), tx).await
}
#[tokio::test]
async fn streams_split_sse_and_preserves_reasoning_and_tool_calls() {
    let body = concat!(
 "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
 "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"run\",\"arguments\":\"\"}}\n\n",
 "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{}\"}\n\n",
 "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"r1\"},{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"run\",\"arguments\":\"{}\"}],\"usage\":{\"input_tokens\":1}}}\n\n");
    let response = Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).into_boxed_str());
    let provider = OpenAiProvider::new(config(server(response).await)).unwrap();
    let result = generate(&provider).await.unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(result.tool_calls[0].arguments, "{}");
    assert_eq!(result.output[0]["type"], "reasoning");
}
#[tokio::test]
async fn rejects_incomplete_stream_and_redacts_error_body() {
    let endpoint = server(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\nConnection: close\r\n\r\nfake-test-key",
    )
    .await;
    let error = generate(&OpenAiProvider::new(config(endpoint)).unwrap())
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("fake-test-key"));
    let endpoint =
        server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    assert!(generate(&OpenAiProvider::new(config(endpoint)).unwrap())
        .await
        .is_err());
}

async fn response_provider(body: &str) -> OpenAiProvider {
    let raw = Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).into_boxed_str());
    OpenAiProvider::new(config(server(raw).await)).unwrap()
}
#[tokio::test]
async fn completed_requires_completed_status_and_recovers_final_text() {
    let bad = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"incomplete\",\"output\":[]}}\n\n";
    assert!(generate(&response_provider(bad).await).await.is_err());
    let good = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"안녕\"}]}]}}\n\n";
    assert_eq!(
        generate(&response_provider(good).await).await.unwrap().text,
        "안녕"
    );
}
#[tokio::test]
async fn rejects_unknown_duplicate_and_invalid_tool_calls() {
    for calls in [
        r#"[{"type":"function_call","call_id":"c1","name":"unknown","arguments":"{}"}]"#,
        r#"[{"type":"function_call","call_id":"c1","name":"run","arguments":"{}"},{"type":"function_call","call_id":"c1","name":"run","arguments":"{}"}]"#,
        r#"[{"type":"function_call","call_id":"c1","name":"run","arguments":"{\"unexpected\":1}"}]"#,
    ] {
        let body=format!("data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"output\":{calls}}}}}\n\n");
        assert!(generate(&response_provider(&body).await).await.is_err());
    }
}
#[tokio::test]
async fn parses_multiline_crlf_with_utf8_and_enforces_limit() {
    let body="data: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"안녕\"}\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\r\n\r\n";
    assert_eq!(
        generate(&response_provider(body).await).await.unwrap().text,
        "안녕"
    );
    let raw = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    );
    let mut c = config(server(raw).await);
    c.max_output_bytes = 10;
    assert!(generate(&OpenAiProvider::new(c).unwrap()).await.is_err());
}
#[tokio::test]
async fn cancellation_interrupts_stalled_stream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut b = [0; 4096];
        let _ = s.read(&mut b).await;
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    });
    let provider = OpenAiProvider::new(config(format!("http://{addr}/v1/responses"))).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        provider.generate(serde_json::json!("hi"), vec![], cancel, tx),
    )
    .await
    .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn does_not_replay_ambiguous_stream_failure() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let task = tokio::spawn(async move {
        while let Ok(Ok((mut s, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
        {
            observed.fetch_add(1, Ordering::SeqCst);
            let mut b = [0; 4096];
            let _ = s.read(&mut b).await;
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    assert!(
        generate(&OpenAiProvider::new(config(format!("http://{addr}/v1/responses"))).unwrap())
            .await
            .is_err()
    );
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn retries_rate_limit_at_most_twice() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let task = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut s, _) = listener.accept().await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            let mut b = [0; 4096];
            let _ = s.read(&mut b).await;
            let _=s.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        }
    });
    assert!(
        generate(&OpenAiProvider::new(config(format!("http://{addr}/v1/responses"))).unwrap())
            .await
            .is_err()
    );
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn rejects_disagreement_between_streamed_and_completed_tool_arguments() {
    let body=concat!("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"run\",\"arguments\":\"\"}}\n\n", "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"x\\\":1}\"}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"run\",\"arguments\":\"{}\"}]}}\n\n");
    assert!(generate(&response_provider(body).await).await.is_err());
}
#[tokio::test]
async fn rejects_unsupported_multimodal_input_without_network_request() {
    let provider = OpenAiProvider::new(config("http://127.0.0.1:9/v1/responses".into())).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let input = serde_json::json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.org/x.png"}]}]);
    let error = provider
        .generate(
            input,
            vec![],
            tokio_util::sync::CancellationToken::new(),
            tx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Invalid OpenAI provider configuration");
}
#[tokio::test]
async fn default_generation_sends_bounded_output_tokens() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = s.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(pos) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes[pos + 4..]) {
                    let _ = tx.send(v);
                    break;
                }
            }
        }
        let body="data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
    });
    let p = OpenAiProvider::new(config(format!("http://{addr}/v1/responses"))).unwrap();
    assert!(generate(&p).await.is_ok());
    let body = rx.await.unwrap();
    assert!(body["max_output_tokens"].as_u64().is_some_and(|v| v > 0));
}
#[tokio::test]
async fn explicit_limit_is_sent_and_zero_is_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = s.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(pos) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes[pos + 4..]) {
                    let _ = tx.send(v);
                    break;
                }
            }
        }
        let body="data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
    });
    let p = OpenAiProvider::new(config(format!("http://{addr}/v1/responses"))).unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(8);
    assert!(p
        .generate_with_limit(
            serde_json::json!("hello"),
            vec![],
            tokio_util::sync::CancellationToken::new(),
            events,
            37
        )
        .await
        .is_ok());
    let body = rx.await.unwrap();
    assert_eq!(body["max_output_tokens"], 37);
    let (events, _rx) = tokio::sync::mpsc::channel(8);
    assert!(p
        .generate_with_limit(
            serde_json::json!("hello"),
            vec![],
            tokio_util::sync::CancellationToken::new(),
            events,
            0
        )
        .await
        .is_err());
}
