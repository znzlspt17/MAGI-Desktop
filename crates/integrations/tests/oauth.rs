use integrations::mcp::{discover_oauth, OAuthFlow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                data.extend_from_slice(&buffer[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&data[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(data).unwrap());
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
        requests
    });
    (address, task)
}

#[tokio::test]
async fn pkce_challenge_matches_exchanged_verifier_and_replay_is_rejected() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    let (address, task) = server(vec![
        r#"{"access_token":"fake","token_type":"Bearer"}"#.into()
    ])
    .await;
    let mut flow = OAuthFlow::new(
        &format!("http://{address}/authorize"),
        &format!("http://{address}/token"),
        "test-client",
        "http://127.0.0.1:43210/callback",
    )
    .unwrap();
    let url = reqwest::Url::parse(&flow.authorization_url()).unwrap();
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["code_challenge_method"], "S256");
    assert!(flow
        .exchange("http://127.0.0.1:43210/callback?state=wrong&code=x")
        .await
        .is_err());
    let callback = format!(
        "http://127.0.0.1:43210/callback?state={}&code=fake",
        pairs["state"]
    );
    assert_eq!(flow.exchange(&callback).await.unwrap(), "fake");
    assert!(flow.exchange(&callback).await.is_err());
    let request = task.await.unwrap().remove(0);
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let form: std::collections::HashMap<_, _> =
        reqwest::Url::parse(&format!("http://localhost/?{body}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
    assert_eq!(
        pairs["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()))
    );
}

#[tokio::test]
async fn discovery_rejects_mismatched_resource_without_contacting_authorization_server() {
    let (address, task) = server(vec![r#"{"resource":"https://wrong.example/mcp","authorization_servers":["https://wrong.example"]}"#.into()]).await;
    let err = discover_oauth(
        &format!("http://{address}/mcp"),
        &format!("http://{address}/metadata"),
    )
    .await
    .unwrap_err();
    assert!(err.contains("resource mismatch"), "{err}");
    task.await.unwrap();
}

#[tokio::test]
async fn discovery_accepts_matching_metadata_and_rejects_unsupported_pkce() {
    for supports_s256 in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for step in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = [0; 4096];
                let n = socket.read(&mut data).await.unwrap();
                let request = String::from_utf8_lossy(&data[..n]);
                if step == 1 {
                    assert!(request.starts_with("GET /.well-known/oauth-authorization-server "));
                }
                let body = if step == 0 {
                    format!(
                        r#"{{"resource":"http://{address}/mcp","authorization_servers":["http://{address}"]}}"#
                    )
                } else {
                    let method = if supports_s256 { "S256" } else { "plain" };
                    format!(
                        r#"{{"issuer":"http://{address}","authorization_endpoint":"http://{address}/authorize","token_endpoint":"http://{address}/token","code_challenge_methods_supported":["{method}"]}}"#
                    )
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        let result = discover_oauth(
            &format!("http://{address}/mcp"),
            &format!("http://{address}/metadata"),
        )
        .await;
        if supports_s256 {
            assert_eq!(
                result.unwrap().token_endpoint,
                format!("http://{address}/token")
            );
        } else {
            assert!(result.unwrap_err().contains("S256 unsupported"));
        }
        task.await.unwrap();
    }
}
#[tokio::test]
async fn oauth_reads_metadata_location_from_unauthorized_challenge() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            let _ = socket.read(&mut b).await.unwrap();
            let body = if step == 1 {
                format!(
                    r#"{{"resource":"http://{addr}/mcp","authorization_servers":["http://{addr}"]}}"#
                )
            } else if step == 2 {
                format!(
                    r#"{{"issuer":"http://{addr}","authorization_endpoint":"http://{addr}/authorize","token_endpoint":"http://{addr}/token","code_challenge_methods_supported":["S256"]}}"#
                )
            } else {
                String::new()
            };
            let challenge = if step == 0 {
                format!("WWW-Authenticate: Bearer resource_metadata=\"http://{addr}/metadata\"\r\n")
            } else {
                String::new()
            };
            socket.write_all(format!("HTTP/1.1 {}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",if step==0{"401 Unauthorized"}else{"200 OK"},challenge,body.len(),body).as_bytes()).await.unwrap();
        }
    });
    let m = integrations::mcp::discover_oauth_automatically(&format!("http://{addr}/mcp"))
        .await
        .unwrap();
    assert_eq!(m.resource, format!("http://{addr}/mcp"));
    server.await.unwrap();
}

#[tokio::test]
async fn oauth_discovery_handles_quoted_commas_and_escapes_across_challenges() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            let n = socket.read(&mut b).await.unwrap();
            if step == 1 {
                assert!(String::from_utf8_lossy(&b[..n]).starts_with("GET /metadata,variant "));
            }
            let body = match step {
                1 => format!(
                    r#"{{"resource":"http://{addr}/mcp","authorization_servers":["http://{addr}"]}}"#
                ),
                2 => format!(
                    r#"{{"issuer":"http://{addr}","authorization_endpoint":"http://{addr}/authorize","token_endpoint":"http://{addr}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
                _ => String::new(),
            };
            let challenge = if step == 0 {
                format!("WWW-Authenticate: Basic realm=\"other,realm\", Bearer realm=\"escaped\\\"quote\", resource_metadata=\"http://{addr}/metadata,variant\"\r\n")
            } else {
                String::new()
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        if step == 0 {
                            "401 Unauthorized"
                        } else {
                            "200 OK"
                        },
                        challenge,
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let result =
        integrations::mcp::discover_oauth_automatically(&format!("http://{addr}/mcp")).await;
    assert!(result.is_ok(), "{result:?}");
    server.await.unwrap();
}

// 2026-11-16: 일부 TCP 패킷만 읽고 응답하지 않도록 요청 길이를 확인하고 조기 EOF를 거절한다.
async fn read_complete_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request ended before all bytes arrived");
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length = header
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
}
#[tokio::test]
async fn oauth_ignores_metadata_in_other_authentication_schemes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_complete_request(&mut socket).await;
        let challenge = format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic resource_metadata=\"http://{addr}/metadata\", Bearer realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        socket.write_all(challenge.as_bytes()).await.unwrap();
    });
    let err = integrations::mcp::discover_oauth_automatically(&format!("http://{addr}/mcp"))
        .await
        .unwrap_err();
    assert!(err.contains("metadata challenge missing"), "{err}");
    server.await.unwrap();
}

#[tokio::test]
async fn oauth_rejects_malformed_or_duplicate_metadata_challenges() {
    for header in [
        "Bearer resource_metadata=\"http://localhost/unfinished",
        "Bearer resource_metadata=\"http://localhost/one\", resource_metadata=\"http://localhost/two\"",
        "Bearer resource_metadata=http://localhost/unquoted",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_complete_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {header}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let err = integrations::mcp::discover_oauth_automatically(&format!("http://{addr}/mcp")).await.unwrap_err();
        assert!(err.contains("challenge"), "{err}");
        server.await.unwrap();
    }
}
