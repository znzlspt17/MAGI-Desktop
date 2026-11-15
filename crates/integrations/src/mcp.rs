use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// 2026-11-15: MCP 협상은 고정 프로토콜 버전과 응답 제한을 사용한다.
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct McpClient {
    endpoint: reqwest::Url,
    authorization: Option<reqwest::header::HeaderValue>,
    http: reqwest::Client,
    session: Option<String>,
    version: String,
    next_id: AtomicU64,
}
impl McpClient {
    // 2026-11-14: 서버의 지시와 관계없이 암호화되지 않은 원격 연결과 URL 자격정보를 거절한다.
    pub async fn connect(endpoint: &str) -> Result<Self, String> {
        Self::connect_inner(endpoint, None).await
    }
    async fn connect_inner(
        endpoint: &str,
        authorization: Option<reqwest::header::HeaderValue>,
    ) -> Result<Self, String> {
        let url = validate_endpoint(endpoint)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "client configuration failed")?;
        let mut client = Self {
            endpoint: url,
            authorization,
            http,
            session: None,
            version: MCP_PROTOCOL_VERSION.into(),
            next_id: AtomicU64::new(2),
        };
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": client.version,
                "capabilities": {},
                "clientInfo": {"name": "magi-desktop", "version": CLIENT_VERSION},
            },
        });
        let (response, session) = client.post(&body, false).await?;
        let result = rpc_result(response, 1)?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or("missing protocol version")?;
        if version != MCP_PROTOCOL_VERSION {
            return Err("unsupported protocol version".into());
        }
        client.version = version.to_owned();
        client.session = session;
        client
            .post(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                true,
            )
            .await?;
        Ok(client)
    }
    async fn post(
        &self,
        body: &Value,
        notification: bool,
    ) -> Result<(Value, Option<String>), String> {
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .header("Accept", "application/json, text/event-stream");
        if let Some(value) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, value.clone());
        }
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if self.session.is_some() {
            request = request.header("MCP-Protocol-Version", &self.version);
        }
        let mut response = request
            .json(body)
            .send()
            .await
            .map_err(|_| "MCP request failed or timed out")?;
        if !response.status().is_success() {
            return Err(format!("MCP HTTP status {}", response.status().as_u16()));
        }
        let session = response
            .headers()
            .get("Mcp-Session-Id")
            .map(|v| {
                v.to_str()
                    .map(str::to_owned)
                    .map_err(|_| "invalid session header")
            })
            .transpose()?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "MCP response failed")? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err("MCP response too large".into());
            }
            bytes.extend_from_slice(&chunk);
            if content_type.starts_with("text/event-stream") && !notification {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    let normalized = text.replace("\r\n", "\n");
                    if let Some((complete, _)) = normalized.rsplit_once("\n\n") {
                        for event in complete.split("\n\n").take(1000) {
                            let data = event
                                .lines()
                                .filter_map(|line| {
                                    line.strip_prefix("data:")
                                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            if data.is_empty() {
                                continue;
                            }
                            let item: Value =
                                serde_json::from_str(&data).map_err(|_| "invalid MCP SSE data")?;
                            if item.get("id").is_some() && item.get("id") == body.get("id") {
                                return Ok((item, session));
                            }
                        }
                    }
                }
            }
        }
        if notification && bytes.is_empty() {
            return Ok((Value::Null, session));
        }
        let value = if content_type.starts_with("application/json") {
            serde_json::from_slice(&bytes).map_err(|_| "invalid MCP JSON")?
        } else if content_type.starts_with("text/event-stream") {
            let text = std::str::from_utf8(&bytes).map_err(|_| "invalid MCP SSE")?;
            let normalized = text.replace("\r\n", "\n");
            let expected_id = body.get("id");
            let mut matched = None;
            for event in normalized.split("\n\n").take(1001) {
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                let item: Value =
                    serde_json::from_str(&data).map_err(|_| "invalid MCP SSE data")?;
                if item.get("id") == expected_id && expected_id.is_some() {
                    matched = Some(item);
                    break;
                }
            }
            matched.ok_or("MCP SSE response missing")?
        } else {
            return Err("unsupported MCP content type".into());
        };
        Ok((value, session))
    }
    // 2026-11-14: 서로 다른 커서가 계속 나오는 서버도 전체 시간과 누적 목록 크기를 제한한다.
    pub async fn list_tools(&self) -> Result<Vec<Value>, String> {
        tokio::time::timeout(Duration::from_secs(30), self.list_tools_inner())
            .await
            .map_err(|_| "MCP listing timed out".to_owned())?
    }
    async fn list_tools_inner(&self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut total_bytes = 0usize;
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let params = cursor
                .as_ref()
                .map(|c| json!({"cursor":c}))
                .unwrap_or_else(|| json!({}));
            let (response, _) = self
                .post(
                    &json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}),
                    false,
                )
                .await?;
            let result = rpc_result(response, id)?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("invalid tools list")?;
            if tools.len() + page.len() > 1000 {
                return Err("too many MCP tools".into());
            }
            total_bytes = total_bytes
                .checked_add(
                    serde_json::to_vec(page)
                        .map_err(|_| "invalid tools list")?
                        .len(),
                )
                .ok_or("MCP tools too large")?;
            if total_bytes > 4 * 1024 * 1024 {
                return Err("MCP tools too large".into());
            }
            tools.extend(page.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match &cursor {
                None => return Ok(tools),
                Some(c) if !seen.insert(c.clone()) => return Err("repeated MCP cursor".into()),
                _ => {}
            }
        }
        Err("too many MCP pages".into())
    }
}
fn rpc_result(response: Value, id: u64) -> Result<Value, String> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || response.get("id").and_then(Value::as_u64) != Some(id)
    {
        return Err("invalid MCP response id or version".into());
    }
    if response.get("error").is_some() {
        return Err("MCP server returned an error".into());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "missing MCP result".into())
}
#[derive(Clone, Debug)]
pub enum CallAuthorization {
    ReadOnlyAllowlist(Vec<String>),
    Approved {
        name: String,
        arguments: Value,
        cwd: String,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}
impl McpClient {
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        schema: &Value,
        cwd: &str,
        authorization: &CallAuthorization,
    ) -> Result<ToolResult, String> {
        if !authorization.permits(name, &arguments, cwd) {
            return Err("tool approval required".into());
        }
        if !arguments.is_object() || !jsonschema::is_valid(schema, &arguments) {
            return Err("tool arguments do not match schema".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response, _) = self.post(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}), false).await?;
        let result = rpc_result(response, id)?;
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let structured_content = result.get("structuredContent").cloned();
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(ToolResult {
            content,
            structured_content,
            is_error,
        })
    }
}
// 2026-11-14: 서버의 readOnlyHint는 신뢰하지 않고 사용자 정책 또는 정확히 승인한 인자만 허용한다.
impl CallAuthorization {
    pub fn permits(&self, name: &str, arguments: &Value, cwd: &str) -> bool {
        match self {
            Self::ReadOnlyAllowlist(names) => names.iter().any(|n| n == name),
            Self::Approved {
                name: approved,
                arguments: args,
                cwd: directory,
            } => approved == name && args == arguments && directory == cwd,
        }
    }
}
pub fn validate_endpoint(endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| "invalid endpoint")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo forbidden".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("endpoint query and fragment forbidden".into());
    }
    let loopback = url
        .host_str()
        .map(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("HTTPS required except loopback".into());
    }
    Ok(url)
}

impl McpClient {
    pub async fn connect_cancelable(
        endpoint: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Self, String> {
        tokio::select! { biased; _ = cancel.cancelled() => Err("MCP connection cancelled".into()), result = Self::connect(endpoint) => result }
    }
    pub async fn list_tools_cancelable(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<Value>, String> {
        tokio::select! { biased; _ = cancel.cancelled() => Err("MCP listing cancelled".into()), result = self.list_tools() => result }
    }
    // 2026-11-14: 취소가 서버의 부작용 중단을 보장하지 않으므로 결과 불명을 명시한다.
    pub async fn call_tool_cancelable(
        &self,
        name: &str,
        arguments: Value,
        schema: &Value,
        cwd: &str,
        authorization: &CallAuthorization,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolResult, String> {
        tokio::select! { biased; _ = cancel.cancelled() => Err("MCP call cancelled; outcome may be unknown".into()), result = self.call_tool(name, arguments, schema, cwd, authorization) => result }
    }
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("endpoint", &self.endpoint)
            .field("authorization", &"[redacted]")
            .finish()
    }
}
impl McpClient {
    pub async fn connect_authenticated(endpoint: &str, token: String) -> Result<Self, String> {
        if token.is_empty() {
            return Err("empty MCP token".into());
        }
        let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "invalid MCP token")?;
        header.set_sensitive(true);
        Self::connect_inner(endpoint, Some(header)).await
    }
}
