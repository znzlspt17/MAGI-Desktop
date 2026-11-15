use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fmt, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 호출별 출력 한도를 지정하지 않을 때 사용하는 기본 토큰 수다.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct OpenAiConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub max_output_bytes: usize,
}
impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("endpoint", &"[redacted]")
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
}
#[derive(Debug)]
pub struct ProviderError(&'static str);
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ProviderError {}
#[derive(Debug)]
pub enum ProviderEvent {
    TextDelta(String),
}
#[derive(Debug)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}
#[derive(Debug)]
pub struct Generation {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub output: Vec<Value>,
    pub usage: Option<Value>,
}
fn invalid() -> ProviderError {
    ProviderError("Invalid OpenAI provider configuration")
}
fn protocol() -> ProviderError {
    ProviderError("Invalid or incomplete OpenAI response")
}

// 2026-11-15: 두 제공자의 URL과 인증 정보 검증 기준을 일치시킨다.
fn validate_endpoint_and_key(endpoint: &str, api_key: &str) -> Result<(), ProviderError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| invalid())?;
    let loopback = url
        .host_str()
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|host| host.is_loopback())
        || url.host_str() == Some("localhost");
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || api_key.trim().is_empty()
        || api_key.contains(['\r', '\n'])
    {
        return Err(invalid());
    }
    Ok(())
}

fn provider_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| invalid())
}

impl OpenAiProvider {
    // 2026-11-14: 명시된 모델만 호출하고 URL 또는 디버그 출력에서 인증 정보가 새지 않도록 제한한다.
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        validate_endpoint_and_key(&config.endpoint, &config.api_key)?;
        if config.max_output_bytes == 0 {
            return Err(invalid());
        }
        let version = config.model.strip_prefix("gpt-").and_then(|s| {
            let n: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            n.parse::<u32>().ok()
        });
        if version.is_none_or(|v| v < 6)
            || !config
                .model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        {
            return Err(invalid());
        }
        let client = provider_client()?;
        Ok(Self { config, client })
    }
    /// 호출별 제한을 지정하지 않은 경우 최대 4096 output token을 요청한다.
    pub async fn generate(
        &self,
        input: Value,
        tools: Vec<Value>,
        cancel: CancellationToken,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<Generation, ProviderError> {
        self.generate_with_limit(input, tools, cancel, events, DEFAULT_MAX_OUTPUT_TOKENS)
            .await
    }
    // 2026-11-14: 완료 이벤트를 받아야만 도구 호출을 반환해 끊긴 응답으로 부작용을 실행하지 않는다.
    pub async fn generate_with_limit(
        &self,
        input: Value,
        tools: Vec<Value>,
        cancel: CancellationToken,
        events: mpsc::Sender<ProviderEvent>,
        max_output_tokens: u64,
    ) -> Result<Generation, ProviderError> {
        if max_output_tokens == 0 {
            return Err(invalid());
        }
        if !(input.is_string()
            || input.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| match item.get("type").and_then(Value::as_str) {
                        Some("reasoning" | "function_call" | "function_call_output") => true,
                        None | Some("message") => match item.get("content") {
                            Some(Value::String(_)) => true,
                            Some(Value::Array(parts)) => parts.iter().all(|part| {
                                matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("input_text" | "output_text")
                                ) && part.get("text").is_some_and(Value::is_string)
                            }),
                            _ => false,
                        },
                        _ => false,
                    })
            }))
        {
            return Err(invalid());
        }
        let mut schemas = BTreeMap::new();
        for tool in &tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                })
                .ok_or_else(invalid)?;
            let schema = tool
                .get("parameters")
                .filter(|v| v.is_object())
                .ok_or_else(invalid)?;
            if tool.get("type").and_then(Value::as_str) != Some("function")
                || schema.get("type").and_then(Value::as_str) != Some("object")
                || schemas.contains_key(name)
            {
                return Err(invalid());
            }
            schemas.insert(
                name.to_owned(),
                jsonschema::validator_for(schema).map_err(|_| invalid())?,
            );
        }
        let body = json!({"model":self.config.model,"input":input,"tools":tools,"stream":true,"store":false,"max_output_tokens":max_output_tokens});
        let mut attempt = 0;
        let response = loop {
            let req = self
                .client
                .post(&self.config.endpoint)
                .bearer_auth(&self.config.api_key)
                .json(&body)
                .send();
            let response = tokio::select! { _=cancel.cancelled()=>return Err(ProviderError("Request cancelled")), r=req=>r.map_err(|_|ProviderError("OpenAI request failed"))? };
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 2 {
                attempt += 1;
                tokio::select! { _=cancel.cancelled()=>return Err(ProviderError("Request cancelled")), _=tokio::time::sleep(Duration::from_millis(200*attempt))=>{} };
                continue;
            }
            if !response.status().is_success() {
                return Err(ProviderError("OpenAI returned an HTTP error"));
            }
            break response;
        };
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut total = 0usize;
        let mut text = String::new();
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        while let Some(chunk) = tokio::select! { _=cancel.cancelled()=>return Err(ProviderError("Request cancelled")), c=stream.next()=>c }
        {
            let chunk = chunk.map_err(|_| protocol())?;
            total = total.checked_add(chunk.len()).ok_or_else(protocol)?;
            if total > self.config.max_output_bytes {
                return Err(ProviderError("OpenAI response exceeded limit"));
            }
            buffer.extend_from_slice(&chunk);
            while let Some(end) = buffer
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| (p, 2))
                .or_else(|| {
                    buffer
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| (p, 4))
                })
            {
                let frame = buffer.drain(..end.0 + end.1).collect::<Vec<_>>();
                let frame = std::str::from_utf8(&frame).map_err(|_| protocol())?;
                let data = frame
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|v| v.strip_prefix(' ').unwrap_or(v))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(&data).map_err(|_| protocol())?;
                match event
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(protocol)?
                {
                    "response.output_text.delta" => {
                        let delta = event
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or_else(protocol)?;
                        text.push_str(delta);
                        tokio::select! {
                            _ = cancel.cancelled() => return Err(ProviderError("Request cancelled")),
                            sent = events.send(ProviderEvent::TextDelta(delta.to_owned())) => {
                                if sent.is_err() {
                                    return Err(ProviderError("Event receiver closed"));
                                }
                            }
                        }
                    }
                    "response.output_item.added" => {
                        let item = &event["item"];
                        if item["type"] == "function_call" {
                            let index = event["output_index"].as_u64().ok_or_else(protocol)?;
                            if calls
                                .insert(
                                    index,
                                    ToolCall {
                                        call_id: item["call_id"]
                                            .as_str()
                                            .ok_or_else(protocol)?
                                            .into(),
                                        name: item["name"].as_str().ok_or_else(protocol)?.into(),
                                        arguments: item["arguments"].as_str().unwrap_or("").into(),
                                    },
                                )
                                .is_some()
                            {
                                return Err(protocol());
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        let index = event["output_index"].as_u64().ok_or_else(protocol)?;
                        calls
                            .get_mut(&index)
                            .ok_or_else(protocol)?
                            .arguments
                            .push_str(event["delta"].as_str().ok_or_else(protocol)?);
                    }
                    "response.completed" => {
                        let response = &event["response"];
                        if response["status"].as_str() != Some("completed") {
                            return Err(protocol());
                        }
                        let output = response["output"].as_array().ok_or_else(protocol)?.clone();
                        let mut final_calls = Vec::new();
                        let mut seen = std::collections::HashSet::new();
                        let mut final_text = String::new();
                        for (index, item) in output.iter().enumerate() {
                            match item["type"].as_str().ok_or_else(protocol)? {
                                "function_call" => {
                                    let call = ToolCall {
                                        call_id: item["call_id"]
                                            .as_str()
                                            .filter(|s| !s.is_empty())
                                            .ok_or_else(protocol)?
                                            .into(),
                                        name: item["name"]
                                            .as_str()
                                            .filter(|s| !s.is_empty())
                                            .ok_or_else(protocol)?
                                            .into(),
                                        arguments: item["arguments"]
                                            .as_str()
                                            .ok_or_else(protocol)?
                                            .into(),
                                    };
                                    if let Some(streamed) = calls.get(&(index as u64)) {
                                        if streamed.call_id != call.call_id
                                            || streamed.name != call.name
                                            || streamed.arguments != call.arguments
                                        {
                                            return Err(protocol());
                                        }
                                    }
                                    let args: Value = serde_json::from_str(&call.arguments)
                                        .map_err(|_| protocol())?;
                                    if !seen.insert(call.call_id.clone())
                                        || !schemas
                                            .get(&call.name)
                                            .ok_or_else(protocol)?
                                            .is_valid(&args)
                                    {
                                        return Err(protocol());
                                    }
                                    final_calls.push(call);
                                }
                                "message" => {
                                    for part in item["content"].as_array().ok_or_else(protocol)? {
                                        match part["type"].as_str() {
                                            Some("output_text") => final_text.push_str(
                                                part["text"].as_str().ok_or_else(protocol)?,
                                            ),
                                            Some("refusal") => return Err(protocol()),
                                            _ => return Err(protocol()),
                                        }
                                    }
                                }
                                "reasoning" => {}
                                _ => return Err(protocol()),
                            }
                        }
                        if !calls.is_empty() && calls.len() != final_calls.len() {
                            return Err(protocol());
                        }
                        if text.is_empty() {
                            text = final_text;
                        } else if !final_text.is_empty() && text != final_text {
                            return Err(protocol());
                        }
                        return Ok(Generation {
                            text,
                            tool_calls: final_calls,
                            output,
                            usage: response.get("usage").cloned(),
                        });
                    }
                    "response.failed" | "response.incomplete" | "error" => return Err(protocol()),
                    _ => {}
                }
            }
        }
        Err(protocol())
    }
}

#[derive(Clone)]
pub struct OpenAiEmbeddingConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub dimensions: usize,
    pub max_response_bytes: usize,
}
impl fmt::Debug for OpenAiEmbeddingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiEmbeddingConfig")
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish()
    }
}
pub struct OpenAiEmbeddingProvider {
    config: OpenAiEmbeddingConfig,
    client: reqwest::Client,
}
impl OpenAiEmbeddingProvider {
    // 2026-11-14: 모델과 차원을 명시하여 서로 다른 임베딩 공간이 섞이지 않게 한다.
    pub fn new(config: OpenAiEmbeddingConfig) -> Result<Self, ProviderError> {
        validate_endpoint_and_key(&config.endpoint, &config.api_key)?;
        if config.model.is_empty()
            || !config
                .model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            || config.dimensions == 0
            || config.max_response_bytes == 0
        {
            return Err(invalid());
        }
        let client = provider_client()?;
        Ok(Self { config, client })
    }
    // 2026-11-14: 응답 인덱스와 차원을 검증한 뒤 입력 순서대로 반환하여 검색 결과 오염을 막는다.
    pub async fn embed(
        &self,
        input: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        if input.is_empty() || input.iter().any(|s| s.is_empty()) {
            return Err(invalid());
        }
        let count = input.len();
        let request = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(&self.config.api_key)
            .json(&json!({
                "model": self.config.model,
                "input": input,
                "dimensions": self.config.dimensions,
                "encoding_format": "float",
            }))
            .send();
        let response = tokio::select! {_=cancel.cancelled()=>return Err(ProviderError("Request cancelled")), r=request=>r.map_err(|_|ProviderError("OpenAI request failed"))?};
        if !response.status().is_success() {
            return Err(ProviderError("OpenAI returned an HTTP error"));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::select! {_=cancel.cancelled()=>return Err(ProviderError("Request cancelled")),r=stream.next()=>r}
        {
            let chunk = chunk.map_err(|_| protocol())?;
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|n| n > self.config.max_response_bytes)
            {
                return Err(ProviderError("OpenAI response exceeded limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: Value = serde_json::from_slice(&bytes).map_err(|_| protocol())?;
        let data = response["data"].as_array().ok_or_else(protocol)?;
        if data.len() != count {
            return Err(protocol());
        }
        let mut ordered = vec![None; count];
        for item in data {
            let index = item["index"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n < count)
                .ok_or_else(protocol)?;
            if ordered[index].is_some() {
                return Err(protocol());
            }
            let values = item["embedding"]
                .as_array()
                .filter(|v| v.len() == self.config.dimensions)
                .ok_or_else(protocol)?;
            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                let number = value.as_f64().ok_or_else(protocol)? as f32;
                if !number.is_finite() {
                    return Err(protocol());
                }
                vector.push(number);
            }
            ordered[index] = Some(vector);
        }
        ordered
            .into_iter()
            .map(|v| v.ok_or_else(protocol))
            .collect()
    }
}
pub mod sdd;
