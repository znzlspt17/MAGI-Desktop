// 2026-11-16: 실행 연결 전 사전 검증은 독립 테스트하며 StartAgent 성공 응답을 만들지 않는다.
fn preflight(
    spec: &domain::ApprovedSpec,
    configs: &[domain::AgentConfig],
    autonomy: &str,
    provider: &str,
    script: &[serde_json::Value],
) -> Result<(), String> {
    crate::agent_loop::validate_plan(spec)?;
    if !["step", "approved_scope"].contains(&autonomy) {
        return Err("invalid autonomy mode".into());
    }
    if script.len() > 100
        || serde_json::to_vec(script)
            .map_err(|_| "invalid mock script")?
            .len()
            > 1024 * 1024
    {
        return Err("invalid mock script".into());
    }
    if !match provider {
        "mock" => !script.is_empty() && configs.iter().all(|c| c.model == "mock"),
        "openai" => script.is_empty(),
        _ => false,
    } {
        return Err("invalid provider or mock script".into());
    }
    let roles = ["coordinator", "implementer", "reviewer"];
    if configs.len() != roles.len()
        || roles
            .iter()
            .any(|r| configs.iter().filter(|c| c.role == *r).count() != 1)
    {
        return Err("invalid agent roles".into());
    }
    for config in configs {
        if config.model.trim().is_empty() {
            return Err("role model missing".into());
        }
        if config.tools.iter().any(|t| {
            !["read", "search", "patch", "mcp", "exec"].contains(&t.as_str())
                || (config.role == "reviewer" && !["read", "search"].contains(&t.as_str()))
        }) {
            return Err("unsupported agent tool".into());
        }
        for (key, maximum) in [("max_rounds", 64), ("max_tokens", 1_000_000)] {
            if !config
                .limits
                .get(key)
                .and_then(|v| v.as_u64())
                .is_some_and(|n| (1..=maximum).contains(&n))
            {
                return Err(format!("invalid {key}"));
            }
        }
        if config
            .limits
            .get("max_output_tokens")
            .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=1_000_000).contains(&n)))
        {
            return Err("invalid output limit".into());
        }
    }
    let coordinator = configs
        .iter()
        .find(|c| c.role == "coordinator")
        .ok_or("coordinator missing")?;
    if coordinator
        .limits
        .get("max_parallel_tasks")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=8).contains(&n)))
    {
        return Err("invalid parallel limit".into());
    }
    let implementer = configs
        .iter()
        .find(|c| c.role == "implementer")
        .ok_or("implementer missing")?;
    if !implementer.tools.iter().any(|t| t == "read") {
        return Err("validation read not allowed".into());
    }
    for task in &spec.tasks {
        for command in &task.validation {
            let (path, _) = command
                .strip_prefix("read_contains:")
                .and_then(|s| s.split_once(':'))
                .ok_or("unsupported validation command")?;
            let name = path.replace('\\', "/").to_lowercase();
            if !crate::worktree::safe_relative(path)
                || crate::worktree::protected(&name)
                || name.split('/').any(|p| p == ".git")
                || !task.owned_paths.iter().any(|owned| {
                    let owned = owned
                        .replace('\\', "/")
                        .trim_end_matches('/')
                        .to_lowercase();
                    name == owned || name.starts_with(&(owned + "/"))
                })
            {
                return Err("validation path outside task scope".into());
            }
        }
    }
    Ok(())
}

fn register_token(
    engine: &crate::Engine,
    run_id: &str,
) -> Result<std::sync::Arc<tokio_util::sync::CancellationToken>, String> {
    let mut tokens = engine
        .cancellations
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if tokens.contains_key(run_id) {
        return Err("agent already active".into());
    }
    let token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    tokens.insert(run_id.to_owned(), std::sync::Arc::clone(&token));
    Ok(token)
}
fn release_token(
    engine: &crate::Engine,
    run_id: &str,
    token: &std::sync::Arc<tokio_util::sync::CancellationToken>,
) {
    let mut tokens = engine
        .cancellations
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if tokens
        .get(run_id)
        .is_some_and(|current| std::sync::Arc::ptr_eq(current, token))
    {
        tokens.remove(run_id);
    }
}
struct AgentLease {
    engine: std::sync::Arc<crate::Engine>,
    run_id: String,
    token: std::sync::Arc<tokio_util::sync::CancellationToken>,
}
impl Drop for AgentLease {
    fn drop(&mut self) {
        release_token(&self.engine, &self.run_id, &self.token);
        self.engine
            .slots
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(test)]
type StartHook = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
static TEST_AFTER_REGISTER: std::sync::Mutex<Option<StartHook>> = std::sync::Mutex::new(None);
#[cfg(all(test, windows))]
static TEST_HANDOFF_NONE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
fn prepare_start(
    engine: std::sync::Arc<crate::Engine>,
    run_id: &str,
    expected: &serde_json::Value,
) -> Result<(AgentLease, serde_json::Value), String> {
    prepare_start_inner(engine, run_id, expected, None)
}
fn prepare_start_inner(
    engine: std::sync::Arc<crate::Engine>,
    run_id: &str,
    expected: &serde_json::Value,
    resume: Option<(&str, &str)>,
) -> Result<(AgentLease, serde_json::Value), String> {
    use std::sync::atomic::Ordering;
    engine
        .slots
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| "run capacity reached")?;
    let token = match register_token(&engine, run_id) {
        Ok(token) => token,
        Err(error) => {
            engine.slots.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
    };
    #[cfg(test)]
    {
        let hook = {
            let mut hook = TEST_AFTER_REGISTER.lock().unwrap();
            if hook.as_ref().is_some_and(|h| h.0 == run_id) {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, ready, proceed)) = hook {
            ready.send(()).unwrap();
            proceed.recv().unwrap();
        }
    }
    let result = if token.is_cancelled() {
        Err("cancelled".to_string())
    } else {
        engine
            .db
            .get()
            .ok_or("storage unavailable".to_string())
            .and_then(|db| match resume {
                Some((model, config)) => {
                    crate::runs::begin_resume(&db, run_id, expected, model, config)
                }
                None => crate::runs::begin_agent(&db, run_id, expected),
            })
    };
    match result {
        Ok(run) => Ok((
            AgentLease {
                engine,
                run_id: run_id.to_owned(),
                token,
            },
            run,
        )),
        Err(error) => {
            release_token(&engine, run_id, &token);
            engine.slots.fetch_sub(1, Ordering::AcqRel);
            Err(error)
        }
    }
}

struct ScriptedMock {
    engine: std::sync::Arc<crate::Engine>,
    run_id: String,
    script: Vec<serde_json::Value>,
}
impl crate::agent_loop::ModelDriver for ScriptedMock {
    async fn generate(
        &self,
        role: &str,
        input: serde_json::Value,
        tools: Vec<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        remaining_tokens: u64,
    ) -> Result<provider::Generation, String> {
        use sha2::{Digest, Sha256};
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if remaining_tokens == 0 {
            return Err("token budget exhausted".into());
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(role, &input, &tools)).map_err(|_| "invalid mock input")?
            )
        );
        let db = self.engine.db.get().ok_or("storage unavailable")?;
        let run = self.run_id.clone();
        let index =
            tokio::task::spawn_blocking(move || crate::runs::mock_index(&db, &run, &fingerprint))
                .await
                .map_err(|_| "mock worker failed")??;
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let step = self
            .script
            .get(index as usize)
            .ok_or("mock script exhausted")?;
        let mut calls = Vec::new();
        if let Some(items) = step.get("tool_calls").and_then(|v| v.as_array()) {
            for item in items {
                let id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .ok_or("mock call ID required")?;
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or("mock tool name required")?;
                let args = item
                    .get("arguments")
                    .filter(|v| v.is_object())
                    .ok_or("mock arguments required")?;
                calls.push(provider::ToolCall {
                    call_id: id.into(),
                    name: name.into(),
                    arguments: args.to_string(),
                });
            }
        }
        let output=calls.iter().map(|c|serde_json::json!({"type":"function_call","call_id":c.call_id,"name":c.name,"arguments":c.arguments})).collect();
        Ok(provider::Generation {
            text: step
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            tool_calls: calls,
            output,
            usage: Some(serde_json::json!({"total_tokens":1})),
        })
    }
}

#[cfg(windows)]
enum ExecutionModel {
    Mock(ScriptedMock),
    OpenAi {
        models: crate::agent_loop::OpenAiModels,
        limits: std::collections::HashMap<String, u64>,
        contexts: std::collections::HashMap<String, Option<u64>>,
        secret: domain::SecretString,
    },
}
#[cfg(windows)]
fn bounded_output(
    input: &serde_json::Value,
    tools: &[serde_json::Value],
    remaining: u64,
    limit: u64,
    context: Option<u64>,
) -> Result<u64, String> {
    let input_bound = (serde_json::to_vec(input)
        .map_err(|_| "invalid input")?
        .len()
        + serde_json::to_vec(tools)
            .map_err(|_| "invalid tools")?
            .len()
        + 64) as u64;
    let available = match context {
        Some(max) => max
            .checked_sub(input_bound)
            .filter(|n| *n > 0)
            .ok_or("model context too small")?,
        None => u64::MAX,
    };
    let output = remaining.min(limit).min(available);
    if output == 0 {
        return Err("token budget exhausted".into());
    }
    Ok(output)
}
#[cfg(windows)]
impl crate::agent_loop::ModelDriver for ExecutionModel {
    async fn generate(
        &self,
        role: &str,
        input: serde_json::Value,
        tools: Vec<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        remaining_tokens: u64,
    ) -> Result<provider::Generation, String> {
        match self {
            Self::Mock(model) => {
                model
                    .generate(role, input, tools, cancel, remaining_tokens)
                    .await
            }
            Self::OpenAi {
                models,
                limits,
                contexts,
                secret,
            } => {
                let limit = *limits.get(role).ok_or("role model unavailable")?;
                let context = *contexts.get(role).ok_or("role model unavailable")?;
                let output = bounded_output(&input, &tools, remaining_tokens, limit, context)?;
                let result = models
                    .generate(role, input, tools, cancel, output)
                    .await
                    .map_err(|_| "provider request failed")?;
                if result.text.contains(&secret.0)
                    || serde_json::to_string(&result.output)
                        .map_err(|_| "invalid provider output")?
                        .contains(&secret.0)
                    || result
                        .tool_calls
                        .iter()
                        .any(|c| c.arguments.contains(&secret.0))
                {
                    return Err("provider output rejected".into());
                }
                Ok(result)
            }
        }
    }
}

#[cfg(windows)]
fn tool_schema(name: &str) -> serde_json::Value {
    let (properties, required) = match name {
        "read" => (serde_json::json!({"path":{"type":"string"}}), vec!["path"]),
        "search" => (
            serde_json::json!({"path":{"type":"string"},"query":{"type":"string"}}),
            vec!["path", "query"],
        ),
        "patch" => (
            serde_json::json!({"path":{"type":"string"},"content":{"type":"string"}}),
            vec!["path", "content"],
        ),
        "exec" => (
            serde_json::json!({"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"}}),
            vec!["program", "args", "cwd"],
        ),
        "mcp" => (
            serde_json::json!({"endpoint":{"type":"string"},"name":{"type":"string"},"arguments":{"type":"object"},"cwd":{"type":"string"},"side_effect":{"type":"boolean","const":true}}),
            vec!["endpoint", "name", "arguments", "cwd", "side_effect"],
        ),
        _ => unreachable!("preflight filters tool names"),
    };
    serde_json::json!({"type":"function","name":name,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

#[cfg(windows)]
type Prepared = (
    AgentLease,
    serde_json::Value,
    domain::ApprovedSpec,
    crate::agent_loop::LoopConfig,
    ExecutionModel,
    String,
);
// 2026-11-16: 선점 이전에 기존 루프와 동일한 설정 식별자를 비교해 잘못된 재개를 차단한다.
#[cfg(windows)]
fn resume_config_identity(
    spec: &domain::ApprovedSpec,
    config: &crate::agent_loop::LoopConfig,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let roles = [
        ("coordinator", &config.coordinator),
        ("implementer", &config.implementer),
        ("reviewer", &config.reviewer),
    ];
    let mut settings = serde_json::json!({
        "roles":roles.iter().map(|(kind, role)| serde_json::json!({"kind":kind,"name":role.name,"tools":role.tools,"rounds":role.max_rounds})).collect::<Vec<_>>(),
        "tokens":config.max_tokens,"parallel":config.max_parallel_tasks,"timeout_ms":config.timeout.as_millis().to_string(),
    });
    if config.max_parallel_tasks > 1 {
        settings["task_order"] = serde_json::json!(crate::agent_loop::validate_plan(spec)?);
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&settings).map_err(|_| "invalid configuration")?)
    ))
}
// 2026-11-16: preflight, token 등록과 실행 세대 선점이 다른 명세 변경과 교차하지 않도록 한 gate에서 준비한다.
#[cfg(windows)]
fn prepare(
    engine: std::sync::Arc<crate::Engine>,
    run_id: &str,
    script: Vec<serde_json::Value>,
    resume: bool,
) -> Result<Prepared, String> {
    use sha2::{Digest, Sha256};
    let _gate = engine
        .mutation_gate
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if engine
        .startup_recovery_pending
        .load(std::sync::atomic::Ordering::Acquire)
        || engine.completions.blocks_global()
        || engine.completions.blocks_run(run_id)
    {
        return Err("완료 결과 저장을 먼저 복구해야 합니다.".into());
    }
    let db = engine.db.get().ok_or("storage unavailable")?;
    let run = crate::runs::get(&db, run_id)?;
    if resume && crate::runs::load_checkpoint(&db, run_id)?.is_none() {
        return Err("checkpoint unavailable".into());
    }
    let project: domain::Project = {
        let tx = db.begin_read().map_err(|e| e.to_string())?;
        let table = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
        let row = table
            .get(run["project_id"].as_str().ok_or("invalid project")?)
            .map_err(|e| e.to_string())?
            .ok_or("project not found")?;
        serde_json::from_str(row.value()).map_err(|_| "invalid project")?
    };
    let spec = project.approved.clone().ok_or("approval required")?;
    let configs: Vec<domain::AgentConfig> = serde_json::from_value(run["agent_configs"].clone())
        .map_err(|_| "invalid role configuration")?;
    let mode = run["autonomy_mode"]
        .as_str()
        .ok_or("invalid autonomy mode")?;
    let selected = run["model_provider"].as_str().ok_or("invalid provider")?;
    preflight(&spec, &configs, mode, selected, &script)?;
    let caps: domain::ModelCapabilities = serde_json::from_value(run["model_capabilities"].clone())
        .map_err(|_| "model capabilities missing")?;
    let mut providers = std::collections::HashMap::new();
    let mut limits = std::collections::HashMap::new();
    let mut contexts = std::collections::HashMap::new();
    let credential = if selected == "openai" {
        Some(
            engine
                .credentials
                .lock()
                .map_err(|_| "credential storage unavailable")?
                .get(&project.id)
                .cloned()
                .ok_or("runtime credential required")?,
        )
    } else {
        None
    };
    for config in &configs {
        let profile = match config.limits.get("capabilities") {
            Some(v) => serde_json::from_value::<domain::ModelCapabilities>(v.clone())
                .map_err(|_| "invalid role capabilities")?,
            None => caps.clone(),
        };
        if (!config.tools.is_empty() && !profile.tools)
            || (selected == "openai" && !profile.streaming)
        {
            return Err("configured model capability unavailable".into());
        }
        let limit = config
            .limits
            .get("max_output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(provider::DEFAULT_MAX_OUTPUT_TOKENS);
        limits.insert(config.role.clone(), limit);
        contexts.insert(config.role.clone(), profile.context_tokens);
        if let Some((endpoint, secret)) = &credential {
            let provider = provider::OpenAiProvider::new(provider::OpenAiConfig {
                endpoint: endpoint.clone(),
                model: config.model.clone(),
                api_key: secret.0.clone(),
                max_output_bytes: 2 * 1024 * 1024,
            })
            .map_err(|_| "invalid agent model configuration")?;
            providers.insert(config.role.clone(), provider);
        }
    }
    let endpoint = credential.as_ref().map(|v| v.0.as_str());
    let identity = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(selected, &configs, &script, endpoint))
                .map_err(|_| "invalid model configuration")?
        )
    );
    let role = |name: &str| -> Result<crate::agent_loop::RoleConfig, String> {
        let c = configs
            .iter()
            .find(|c| c.role == name)
            .ok_or("role missing")?;
        Ok(crate::agent_loop::RoleConfig {
            name: name.into(),
            tools: c.tools.iter().map(|v| tool_schema(v)).collect(),
            max_rounds: c.limits["max_rounds"].as_u64().ok_or("invalid rounds")? as usize,
        })
    };
    let total = configs
        .iter()
        .try_fold(0u64, |sum, c| {
            sum.checked_add(c.limits["max_tokens"].as_u64()?)
        })
        .ok_or("invalid token budget")?;
    let parallel = configs
        .iter()
        .find(|c| c.role == "coordinator")
        .and_then(|c| c.limits.get("max_parallel_tasks"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let config = crate::agent_loop::LoopConfig {
        coordinator: role("coordinator")?,
        implementer: role("implementer")?,
        reviewer: role("reviewer")?,
        max_tokens: total,
        max_parallel_tasks: parallel as usize,
        timeout: std::time::Duration::from_secs(120),
    };
    let model = match credential {
        Some((_, secret)) => ExecutionModel::OpenAi {
            models: crate::agent_loop::OpenAiModels(providers),
            limits,
            contexts,
            secret,
        },
        None => ExecutionModel::Mock(ScriptedMock {
            engine: std::sync::Arc::clone(&engine),
            run_id: run_id.into(),
            script,
        }),
    };
    if resume {
        crate::runs::verify_resume_revision(&run)?;
    }
    let config_identity = if resume {
        Some(resume_config_identity(&spec, &config)?)
    } else {
        None
    };
    drop(db);
    let (lease, claimed) = if let Some(config_identity) = &config_identity {
        prepare_start_inner(
            std::sync::Arc::clone(&engine),
            run_id,
            &run,
            Some((&identity, config_identity)),
        )?
    } else {
        prepare_start(std::sync::Arc::clone(&engine), run_id, &run)?
    };
    Ok((lease, claimed, spec, config, model, identity))
}

// 2026-11-16: 마지막 reviewer는 전체 검증과 실제 snapshot에 귀속되며 완료 저장자와 같은 입력 digest를 사용한다.
#[cfg(windows)]
struct ReviewWork<'a> {
    engine: &'a std::sync::Arc<crate::Engine>,
    run_id: &'a str,
    spec: &'a domain::ApprovedSpec,
    context_version: u64,
    attempt: u64,
    model: &'a ExecutionModel,
    runtime: &'a crate::agent_runtime::Runtime,
    store: &'a crate::agent_runtime::Store,
    config: &'a crate::agent_loop::LoopConfig,
}
#[cfg(windows)]
async fn final_review(
    work: ReviewWork<'_>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    use crate::agent_loop::{CheckpointStore, DurableToolDriver, ModelDriver, ToolDriver};
    let ReviewWork {
        engine,
        run_id,
        spec,
        context_version,
        attempt,
        model,
        runtime,
        store,
        config,
    } = work;
    let mut evidence = Vec::new();
    let mut ids = Vec::new();
    for task in &spec.tasks {
        for (index, command) in task.validation.iter().enumerate() {
            let id = serde_json::to_string(&(&task.id, "final_validation", index))
                .map_err(|_| "invalid verification ID")?;
            let result = runtime
                .verify_durable(task, command, &id, cancel.clone())
                .await?;
            if !result.passed || result.verification_id != id {
                return Err("final validation failed".into());
            }
            ids.push(id);
            evidence.push(result);
        }
    }
    if ids.is_empty() {
        return Err("final validation missing".into());
    }
    runtime.fresh(spec).await?;
    let diff = {
        let engine = std::sync::Arc::clone(engine);
        let id = run_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _gate = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            crate::runs::diff(&db, &id)
        })
        .await
        .map_err(|_| "diff worker failed")??
    };
    let revision = diff["current_revision"]
        .as_str()
        .ok_or("invalid revision")?
        .to_owned();
    let worktree_hash = diff["worktree_hash"]
        .as_str()
        .ok_or("invalid snapshot")?
        .to_owned();
    if evidence.iter().any(|e| {
        e.revision != revision || e.worktree_hash != worktree_hash || e.spec_hash != spec.hash
    }) {
        return Err("verification snapshot changed".into());
    }
    let binding = crate::agent_completion::ReviewBinding {
        schema_version: 1,
        spec_hash: spec.hash.clone(),
        spec_version: spec.version,
        context_version,
        agent_attempt: attempt,
        revision,
        worktree_hash,
        verification_ids: ids,
    };
    let (input, digest) = crate::agent_completion::review_input(spec, &evidence, &diff, &binding)?;
    let record = {
        let engine = std::sync::Arc::clone(engine);
        let id = run_id.to_owned();
        let hash = digest.clone();
        let call_id = crate::agent_completion::review_call_id(attempt);
        tokio::task::spawn_blocking(move || {
            let _gate = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            crate::runs::claim_call(&db, &id, &call_id, &hash)
        })
        .await
        .map_err(|_| "review claim worker failed")??
    };
    if record["status"] == "completed" {
        let saved: crate::agent_completion::ReviewBinding =
            serde_json::from_value(record["result"]["binding"].clone())
                .map_err(|_| "invalid saved review")?;
        if record["result"]["approved"] == true && saved == binding {
            return Ok(());
        }
        return Err("saved review mismatch".into());
    }
    if record["fresh"] != true {
        return Err("final review outcome uncertain".into());
    }
    let mut state = store.load().await?.ok_or("checkpoint unavailable")?;
    let input_bound = (serde_json::to_vec(&input)
        .map_err(|_| "invalid review input")?
        .len()
        + 64) as u64;
    let reservation = config.max_tokens.saturating_sub(state.tokens_used);
    if reservation <= input_bound {
        return Err("token budget insufficient for final review".into());
    }
    state.tokens_used += reservation;
    state.reserved_tokens = reservation;
    state.model_inflight = true;
    let previous = state.revision;
    state.revision = store.save(previous, state.clone()).await?;
    let result = model
        .generate(
            &config.reviewer.name,
            input,
            vec![],
            cancel,
            reservation - input_bound,
        )
        .await?;
    let usage = result
        .usage
        .as_ref()
        .and_then(|v| v["total_tokens"].as_u64())
        .ok_or("provider token usage missing")?;
    if usage > reservation {
        return Err("provider exceeded reserved token budget".into());
    }
    state.tokens_used = state.tokens_used - reservation + usage;
    state.reserved_tokens = 0;
    state.model_inflight = false;
    let previous = state.revision;
    store.save(previous, state).await?;
    if !result.tool_calls.is_empty()
        || serde_json::from_str::<serde_json::Value>(&result.text)
            .ok()
            .and_then(|v| v["approved"].as_bool())
            != Some(true)
    {
        return Err("final review rejected or invalid".into());
    }
    runtime.fresh(spec).await?;
    let engine = std::sync::Arc::clone(engine);
    let id = run_id.to_owned();
    let call_id = crate::agent_completion::review_call_id(attempt);
    tokio::task::spawn_blocking(move || {
        let _gate = engine
            .mutation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let db = engine.db.get().ok_or("storage unavailable")?;
        crate::runs::complete_call(
            &db,
            &id,
            &call_id,
            serde_json::json!({"approved":true,"binding":binding}),
        )
    })
    .await
    .map_err(|_| "review save worker failed")??;
    Ok(())
}

#[cfg(windows)]
pub(crate) async fn start(
    engine: std::sync::Arc<crate::Engine>,
    run_id: String,
    script: Vec<serde_json::Value>,
    resume: bool,
) -> Result<(), String> {
    let prepared = {
        let engine = std::sync::Arc::clone(&engine);
        let id = run_id.clone();
        tokio::task::spawn_blocking(move || prepare(engine, &id, script, resume))
            .await
            .map_err(|_| "agent preparation failed")??
    };
    let (lease, claimed, spec, config, model, identity) = prepared;
    let attempt = claimed["agent_attempt"].as_u64().ok_or("invalid attempt")?;
    let context = claimed["context_version"]
        .as_u64()
        .ok_or("invalid context")?;
    let project_id = claimed["project_id"]
        .as_str()
        .ok_or("invalid project")?
        .to_owned();
    let cancel = lease.token.as_ref().clone();
    tokio::spawn(async move {
        let _lease = lease;
        let runtime = crate::agent_runtime::Runtime::new(
            std::sync::Arc::clone(&engine),
            run_id.clone(),
            spec.clone(),
            context,
            attempt,
        );
        let store = crate::agent_runtime::Store::new(
            std::sync::Arc::clone(&engine),
            run_id.clone(),
            spec.clone(),
            context,
            attempt,
        );
        let result = match crate::agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &config,
            &identity,
            cancel.clone(),
        )
        .await
        {
            Ok(()) => {
                final_review(
                    ReviewWork {
                        engine: &engine,
                        run_id: &run_id,
                        spec: &spec,
                        context_version: context,
                        attempt,
                        model: &model,
                        runtime: &runtime,
                        store: &store,
                        config: &config,
                    },
                    cancel,
                )
                .await
            }
            Err(error) => Err(error),
        };
        // 2026-11-16: 중단된 외부 도구 worker의 기록과 실제 종료가 끝난 뒤에만 완료와 슬롯 반환을 진행한다.
        runtime.wait_effect_workers().await;
        let record = crate::completion::CompletionManager::agent_record(
            &run_id,
            &project_id,
            result.is_ok(),
            attempt,
        );
        let db = engine.db.get();
        #[cfg(test)]
        let db = if TEST_HANDOFF_NONE.lock().unwrap().take().as_deref() == Some(run_id.as_str()) {
            None
        } else {
            db
        };
        crate::save_agent_result(std::sync::Arc::clone(&engine.completions), db, record).await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (domain::ApprovedSpec, Vec<domain::AgentConfig>) {
        let spec = serde_json::from_value(serde_json::json!({
            "version":1,"hash":"fixture","base_revision":"abc","scope":["src"],
            "requirements":["requirement"],"acceptance":["acceptance"],
            "tasks":[{"id":"task","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/a.txt:done"]}],
            "task_bindings":[{"task_id":"task","goal":"goal","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]
        })).unwrap();
        let configs = ["coordinator", "implementer", "reviewer"]
            .iter()
            .map(|role| domain::AgentConfig {
                role: (*role).into(),
                model: "mock".into(),
                tools: vec!["read".into()],
                limits: serde_json::json!({"max_rounds":4,"max_tokens":10000}),
            })
            .collect();
        (spec, configs)
    }
    #[cfg(windows)]
    #[test]
    fn output_limit_accounts_for_context_before_provider_call() {
        let input = serde_json::json!([{"role":"user","content":"fixture"}]);
        assert!(bounded_output(&input, &[], 1000, 100, Some(1)).is_err());
        assert_eq!(bounded_output(&input, &[], 1000, 100, None).unwrap(), 100);
    }
    #[test]
    fn allows_mcp_for_implementer_but_not_reviewer() {
        let (spec, mut configs) = fixture();
        configs[1].tools.push("mcp".into());
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_ok());
        configs[2].tools.push("mcp".into());
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_err());
    }
    #[test]
    fn allows_exec_for_implementer_but_not_reviewer() {
        let (spec, mut configs) = fixture();
        configs[1].tools.push("exec".into());
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_ok());
        configs[2].tools.push("exec".into());
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_err());
    }
    #[test]
    fn supports_only_explicit_initial_tool_and_validation_contract() {
        let (spec, configs) = fixture();
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_ok());
        for name in ["git", "knowledge"] {
            let mut invalid = configs.clone();
            invalid[1].tools.push(name.into());
            assert!(
                preflight(&spec, &invalid, "step", "mock", &[serde_json::json!({})]).is_err(),
                "{name}"
            );
        }
        let mut invalid = spec.clone();
        invalid.tasks[0].validation = vec!["exec_json:{}".into()];
        assert!(preflight(&invalid, &configs, "step", "mock", &[serde_json::json!({})]).is_err());
        invalid.tasks[0].validation = vec!["read_contains:outside/file.txt:done".into()];
        assert!(preflight(&invalid, &configs, "step", "mock", &[serde_json::json!({})]).is_err());
        assert!(preflight(
            &spec,
            &configs,
            "automatic",
            "mock",
            &[serde_json::json!({})]
        )
        .is_err());
        assert!(preflight(&spec, &configs, "step", "mock", &[]).is_err());
        assert!(preflight(&spec, &configs, "step", "openai", &[serde_json::json!({})]).is_err());
        let mut invalid_parallel = configs.clone();
        invalid_parallel[0].limits["max_parallel_tasks"] = 0.into();
        assert!(preflight(
            &spec,
            &invalid_parallel,
            "step",
            "mock",
            &[serde_json::json!({})]
        )
        .is_err());
    }
    #[test]
    fn stale_worker_cannot_remove_replacement_cancellation_token() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!("magi-agent-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = Arc::new(crate::Engine::open(&root.join("test.redb")).unwrap());
        let old = register_token(&engine, "run").unwrap();
        assert!(register_token(&engine, "run").is_err());
        engine.slots.store(1, std::sync::atomic::Ordering::Release);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = {
            let engine = Arc::clone(&engine);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                entered.wait();
                release.wait();
                drop(AgentLease {
                    engine,
                    run_id: "run".into(),
                    token: old,
                });
            })
        };
        entered.wait();
        let replacement = Arc::new(tokio_util::sync::CancellationToken::new());
        engine
            .cancellations
            .lock()
            .unwrap()
            .insert("run".into(), Arc::clone(&replacement));
        release.wait();
        worker.join().unwrap();
        engine
            .cancellations
            .lock()
            .unwrap()
            .get("run")
            .unwrap()
            .cancel();
        assert!(replacement.is_cancelled());
        assert_eq!(engine.slots.load(std::sync::atomic::Ordering::Acquire), 0);
        release_token(&engine, "run", &replacement);
        assert!(!engine.cancellations.lock().unwrap().contains_key("run"));
        drop(engine);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-agent-token-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn four_agent_leases_can_call_mcp_without_nested_slot() {
        use crate::agent_loop::DurableToolDriver;
        use std::sync::{atomic::Ordering, Arc};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root =
            std::env::temp_dir().join(format!("magi-agent-capacity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let mut engine = Arc::new(crate::Engine::open(&root.join("state.redb")).unwrap());
        let db = engine.db.get().unwrap();
        let (spec, mut configs) = fixture();
        configs[1].tools.push("mcp".into());
        let project = serde_json::json!({"id":"project","name":"fixture","path":"fixture","draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2,"agent_configs":configs});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(crate::PROJECTS)
                .unwrap()
                .insert("project", project.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            for n in 0..4 {
                let id = format!("run-{n}");
                let run = serde_json::json!({"id":id,"project_id":"project","spec_hash":"fixture","spec_version":1,"revision":"abc","scope":["src"],"worktree":"fixture","status":"awaiting_implementation","pending":null,"events":[],"context_version":2,"agent_active":false,"agent_attempt":0,"agent_configs":configs,"used_calls":[],"evidence":[]});
                runs.insert(id.as_str(), run.to_string().as_str()).unwrap();
            }
        }
        tx.commit().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        crate::mcp_connections::configure(&db, "project", &endpoint).unwrap();
        let server = tokio::spawn(async move {
            for step in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                        let len = headers
                            .lines()
                            .find_map(|v| {
                                v.strip_prefix("content-length: ")
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let body = match step {
                    0 => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
                    1 => "",
                    2 => {
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"write","inputSchema":{"type":"object"}}]}}"#
                    }
                    _ => {
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"ok"}]}}"#
                    }
                };
                let status = if step == 1 { "202 Accepted" } else { "200 OK" };
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let mut leases = Vec::new();
        for n in 0..4 {
            let id = format!("run-{n}");
            let expected = crate::runs::get(&db, &id).unwrap();
            let (lease, _) = prepare_start(Arc::clone(&engine), &id, &expected).unwrap();
            leases.push(lease);
        }
        assert_eq!(engine.slots.load(Ordering::Acquire), 4);
        let token = leases[0].token.as_ref().clone();
        let mut holds = Vec::new();
        let mut releases = Vec::new();
        for lease in leases.drain(..) {
            let (send, recv) = tokio::sync::oneshot::channel::<()>();
            releases.push(send);
            holds.push(tokio::spawn(async move {
                let _lease = lease;
                let _ = recv.await;
            }));
        }
        let runtime = crate::agent_runtime::Runtime::new(
            Arc::clone(&engine),
            "run-0".into(),
            spec.clone(),
            2,
            1,
        );
        let task = spec.tasks[0].clone();
        let work = tokio::spawn(async move {
            runtime.execute_durable("implementer",&task,"mcp","slot-call",serde_json::json!({"endpoint":endpoint,"name":"write","arguments":{},"cwd":".","side_effect":true}),token).await
        });
        let run = loop {
            let run = crate::runs::get(&db, "run-0").unwrap();
            if run["status"] == "awaiting_approval" {
                break run;
            }
            assert!(!work.is_finished());
            tokio::task::yield_now().await;
        };
        crate::runs::approve_patch(&db, "run-0", run["pending"]["id"].as_str().unwrap(), true)
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), work)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        server.await.unwrap();
        assert_eq!(engine.slots.load(Ordering::Acquire), 4);
        for release in releases {
            release.send(()).unwrap();
        }
        for hold in holds {
            hold.await.unwrap();
        }
        assert_eq!(engine.slots.load(Ordering::Acquire), 0);
        drop(db);
        Arc::get_mut(&mut engine).unwrap().db =
            crate::storage::StorageSlot::empty(root.join("closed.redb"));
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resume_claim_rejects_unfinished_calls_and_serializes_competing_attempts() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!("magi-resume-claim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let engine = Arc::new(crate::Engine::open(&root.join("state.redb")).unwrap());
        let db = engine.db.get().unwrap();
        let (spec, configs) = fixture();
        let project = serde_json::json!({"id":"project","name":"fixture","path":"fake","draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2,"agent_configs":configs});
        let run = serde_json::json!({"id":"run","project_id":"project","spec_hash":"fixture","spec_version":1,"revision":"abc","scope":["src"],"status":"interrupted","pending":null,"events":[],"context_version":2,"agent_active":false,"agent_attempt":1,"agent_configs":configs,"used_calls":[],"evidence":[]});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(crate::PROJECTS)
                .unwrap()
                .insert("project", project.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("run", run.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints")).unwrap().insert("run", r#"{"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":"model","config_hash":"config","task_index":0,"role_index":0,"round":0,"input":[],"pending_calls":[],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0}"#).unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap()
                .insert("run:tool", r#"{"status":"planned"}"#)
                .unwrap();
        }
        tx.commit().unwrap();
        let expected = crate::runs::get(&db, "run").unwrap();
        assert!(crate::runs::begin_resume(&db, "run", &expected, "other-model", "config").is_err());
        assert!(crate::runs::begin_resume(&db, "run", &expected, "model", "other-config").is_err());
        assert!(
            crate::runs::begin_resume(&db, "run", &expected, "model", "config")
                .unwrap_err()
                .contains("uncertain")
        );
        assert_eq!(crate::runs::get(&db, "run").unwrap(), expected);
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap()
                .insert(
                    "run:tool",
                    r#"{"status":"completed","result":{"fixture":true}}"#,
                )
                .unwrap();
        }
        tx.commit().unwrap();
        let mut old_evidence = expected.clone();
        old_evidence["evidence"] = serde_json::json!([{"fixture":"previous-attempt"}]);
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("run", old_evidence.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(
            crate::runs::begin_resume(&db, "run", &old_evidence, "model", "config")
                .unwrap_err()
                .contains("verification id")
        );
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("run", expected.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                let expected = expected.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    crate::runs::begin_resume(&db, "run", &expected, "model", "config")
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
        let saved = crate::runs::get(&db, "run").unwrap();
        assert_eq!(saved["agent_attempt"], 2);
        assert_eq!(saved["events"][0]["attempt"], 2);
        assert_eq!(saved["events"][0]["kind"], "agent_resumed");
        drop(db);
        drop(engine);
        let _ = std::fs::remove_file(root.join("state.redb"));
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn concurrent_start_claims_only_one_attempt() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!("magi-agent-cas-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = Arc::new(crate::Engine::open(&root.join("test.redb")).unwrap());
        let db = engine.db.get().unwrap();
        let (spec, configs) = fixture();
        let project = serde_json::json!({"id":"project","name":"fixture","path":"fake","draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2,"agent_configs":configs});
        let run = serde_json::json!({"id":"run","project_id":"project","spec_hash":"fixture","spec_version":1,"revision":"abc","scope":["src"],"status":"awaiting_implementation","pending":null,"events":[],"context_version":2,"agent_active":false,"agent_attempt":0,"agent_configs":configs,"used_calls":[],"evidence":[]});
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(crate::PROJECTS).unwrap();
            t.insert("project", project.to_string().as_str()).unwrap();
        }
        {
            let mut t = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            t.insert("run", run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let cancel_run = {
            let mut v = run.clone();
            v["id"] = "cancel-run".into();
            v
        };
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            t.insert("cancel-run", cancel_run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let cancel_expected = crate::runs::get(&db, "cancel-run").unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        *TEST_AFTER_REGISTER.lock().unwrap() = Some(("cancel-run".into(), ready_tx, go_rx));
        let worker = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || prepare_start(engine, "cancel-run", &cancel_expected))
        };
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let response = engine.handle(domain::Request {
            id: "test".into(),
            command: domain::Command::CancelRun {
                run_id: "cancel-run".into(),
            },
        });
        assert!(response.result.is_ok());
        go_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(!engine
            .cancellations
            .lock()
            .unwrap()
            .contains_key("cancel-run"));
        assert_eq!(engine.slots.load(std::sync::atomic::Ordering::Acquire), 0);
        assert_eq!(
            crate::runs::get(&db, "cancel-run").unwrap()["agent_active"],
            false
        );
        let expected = crate::runs::get(&db, "run").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let db = Arc::clone(&db);
                let barrier = Arc::clone(&barrier);
                let expected = expected.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    crate::runs::begin_agent(&db, "run", &expected)
                })
            })
            .collect();
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 1);
        let saved = crate::runs::get(&db, "run").unwrap();
        assert_eq!(saved["agent_attempt"], 1);
        assert_eq!(saved["agent_active"], true);
        drop(db);
        drop(engine);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-agent-cas-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn mock_request_index_is_durable_and_scoped_to_each_run() {
        let root = std::env::temp_dir().join(format!("magi-mock-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("test.redb");
        let db = redb::Database::create(&path).unwrap();
        assert_eq!(crate::runs::mock_index(&db, "run-a", "first").unwrap(), 0);
        assert_eq!(crate::runs::mock_index(&db, "run-a", "second").unwrap(), 1);
        assert_eq!(crate::runs::mock_index(&db, "run-b", "first").unwrap(), 0);
        drop(db);
        let db = redb::Database::open(&path).unwrap();
        assert_eq!(crate::runs::mock_index(&db, "run-a", "first").unwrap(), 0);
        assert_eq!(crate::runs::mock_index(&db, "run-a", "third").unwrap(), 2);
        drop(db);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-mock-index-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn scripted_model_reuses_input_and_returns_structured_tool_call() {
        use crate::agent_loop::ModelDriver;
        let root = std::env::temp_dir().join(format!("magi-mock-model-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = std::sync::Arc::new(crate::Engine::open(&root.join("test.redb")).unwrap());
        let model = ScriptedMock {
            engine: std::sync::Arc::clone(&engine),
            run_id: "run".into(),
            script: vec![
                serde_json::json!({"text":"first","tool_calls":[{"call_id":"call","name":"read","arguments":{"path":"src/a"}}]}),
                serde_json::json!({"text":"second"}),
            ],
        };
        let first = model
            .generate(
                "implementer",
                serde_json::json!(["first"]),
                vec![],
                tokio_util::sync::CancellationToken::new(),
                100,
            )
            .await
            .unwrap();
        assert_eq!(first.text, "first");
        assert_eq!(first.tool_calls[0].name, "read");
        let replay = model
            .generate(
                "implementer",
                serde_json::json!(["first"]),
                vec![],
                tokio_util::sync::CancellationToken::new(),
                100,
            )
            .await
            .unwrap();
        assert_eq!(replay.text, "first");
        let second = model
            .generate(
                "reviewer",
                serde_json::json!(["second"]),
                vec![],
                tokio_util::sync::CancellationToken::new(),
                100,
            )
            .await
            .unwrap();
        assert_eq!(second.text, "second");
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        assert!(model
            .generate(
                "reviewer",
                serde_json::json!(["third"]),
                vec![],
                cancelled,
                100
            )
            .await
            .is_err());
        drop(model);
        drop(engine);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-mock-model-"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn start_agent_changes_worktree_verifies_and_completes_review() {
        use std::{os::windows::process::CommandExt, sync::Arc};
        fn git(root: &std::path::Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .creation_flags(0x08000000)
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().into()
        }
        let base = std::env::temp_dir().join(format!("magi-start-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.txt"), "before").unwrap();
        git(&repo, &["add", "src/a.txt"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let revision = git(&repo, &["rev-parse", "HEAD"]);
        let work = base.join("work");
        let target = work
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(work.to_str().unwrap());
        git(
            &repo,
            &["worktree", "add", "--detach", "--", target, &revision],
        );
        let engine = Arc::new(crate::Engine::open(&base.join("state.redb")).unwrap());
        let db = engine.db.get().unwrap();
        let (mut spec, mut configs) = fixture();
        spec.base_revision = revision.clone();
        spec.tasks[0].validation = vec!["read_contains:src/a.txt:after".into()];
        configs[1].tools.push("patch".into());
        let project = serde_json::json!({"id":"project","name":"fixture","path":repo,"draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2,"agent_configs":configs,"autonomy_mode":"approved_scope","model_settings":{"provider":"mock","model":"mock","capabilities":{"streaming":false,"tools":true,"context_tokens":null}}});
        let run = serde_json::json!({"id":"run","project_id":"project","spec_hash":"fixture","spec_version":1,"revision":revision,"scope":["src"],"worktree":work,"status":"awaiting_implementation","pending":null,"events":[],"context_version":2,"agent_active":false,"agent_attempt":0,"agent_configs":configs,"autonomy_mode":"approved_scope","model_provider":"mock","model_capabilities":{"streaming":false,"tools":true,"context_tokens":null},"used_calls":[],"evidence":[]});
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(crate::PROJECTS).unwrap();
            t.insert("project", project.to_string().as_str()).unwrap();
        }
        {
            let mut t = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            t.insert("run", run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let script = vec![
            serde_json::json!({"text":"coordinate"}),
            serde_json::json!({"tool_calls":[{"call_id":"patch","name":"patch","arguments":{"path":"src/a.txt","content":"after"}}]}),
            serde_json::json!({"text":"implemented"}),
            serde_json::json!({"text":"{\"approved\":true}"}),
            serde_json::json!({"text":"{\"approved\":true}"}),
        ];
        let before_resume = crate::runs::get(&db, "run").unwrap();
        let missing = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "resume-without-checkpoint".into(),
                command: domain::Command::ResumeAgent {
                    run_id: "run".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(missing.result.unwrap_err().contains("checkpoint"));
        assert_eq!(crate::runs::get(&db, "run").unwrap(), before_resume);
        let mut changed_revision = before_resume.clone();
        changed_revision["revision"] = "0000000000000000000000000000000000000000".into();
        assert!(crate::runs::verify_resume_revision(&changed_revision)
            .unwrap_err()
            .contains("revision changed"));
        let make_role = |name: &str| {
            let value = configs.iter().find(|v| v.role == name).unwrap();
            crate::agent_loop::RoleConfig {
                name: name.into(),
                tools: value.tools.iter().map(|v| tool_schema(v)).collect(),
                max_rounds: 4,
            }
        };
        let resume_config = crate::agent_loop::LoopConfig {
            coordinator: make_role("coordinator"),
            implementer: make_role("implementer"),
            reviewer: make_role("reviewer"),
            max_tokens: 30000,
            max_parallel_tasks: 1,
            timeout: std::time::Duration::from_secs(120),
        };
        use sha2::{Digest, Sha256};
        let identity = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&("mock", &configs, &script, Option::<&str>::None)).unwrap()
            )
        );
        let checkpoint = serde_json::json!({"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":identity,"config_hash":resume_config_identity(&spec, &resume_config).unwrap(),"task_index":0,"role_index":0,"round":0,"input":[],"pending_calls":[],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0});
        let mut resumed_run = before_resume.clone();
        resumed_run["id"] = "resumable".into();
        resumed_run["status"] = "interrupted".into();
        resumed_run["agent_attempt"] = 1.into();
        let logical_id = serde_json::to_string(&("task", "final_validation", 0)).unwrap();
        let old_row = serde_json::json!({"revision":revision,"spec_hash":"fixture","command":"read_contains:src/a.txt:after","passed":true,"output":"old fixture","verification_id":logical_id,"worktree_hash":"old-snapshot","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]});
        let old_context = serde_json::json!({"schema_version":1,"spec_version":1,"context_version":2,"agent_attempt":1,"task_id":"task"});
        resumed_run["evidence"] = serde_json::json!([old_row]);
        resumed_run["evidence_contexts"] = serde_json::json!({logical_id.clone():old_context});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("resumable", resumed_run.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert("resumable", checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let resumed = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "resume".into(),
                command: domain::Command::ResumeAgent {
                    run_id: "resumable".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(resumed.result.is_ok(), "{:?}", resumed.result);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let resumed_run = loop {
            let run = crate::runs::get(&db, "resumable").unwrap();
            if engine.slots.load(std::sync::atomic::Ordering::Acquire) == 0 {
                break run;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(resumed_run["status"], "completed", "{resumed_run}");
        assert_eq!(resumed_run["agent_attempt"], 2);
        assert_eq!(resumed_run["events"][0]["attempt"], 2);
        assert_eq!(resumed_run["evidence"][0], old_row);
        assert_eq!(resumed_run["evidence_contexts"][&logical_id], old_context);
        let (new_row, context) = crate::evidence::resolve(&resumed_run, &logical_id, 2)
            .unwrap()
            .expect("fresh verification missing");
        assert_eq!(new_row["agent_attempt"], 2);
        assert_eq!(context["agent_attempt"], 2);
        let mut waiting_run = before_resume.clone();
        waiting_run["id"] = "waiting-resume".into();
        waiting_run["agent_active"] = true.into();
        waiting_run["agent_attempt"] = 1.into();
        waiting_run["autonomy_mode"] = "step".into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("waiting-resume", waiting_run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let call_id = serde_json::to_string(&("task", "implementer", 0, "patch")).unwrap();
        let args = serde_json::json!({"path":"src/pending.txt","content":"approved"});
        let waiting = crate::runs::submit_bound_patch(
            &db,
            "waiting-resume",
            "implementer",
            "task",
            &call_id,
            args.clone(),
        )
        .unwrap();
        let approval = waiting["approval_id"].as_str().unwrap().to_owned();
        let mut waiting_run = crate::runs::get(&db, "waiting-resume").unwrap();
        waiting_run["agent_active"] = false.into();
        let mut waiting_checkpoint = checkpoint.clone();
        waiting_checkpoint["role_index"] = 1.into();
        waiting_checkpoint["pending_calls"] =
            serde_json::json!([{"call_id":"patch","name":"patch","arguments":args}]);
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("waiting-resume", waiting_run.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert("waiting-resume", waiting_checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let response = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "resume-waiting".into(),
                command: domain::Command::ResumeAgent {
                    run_id: "waiting-resume".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(response.result.is_ok(), "{:?}", response.result);
        assert!(!work.join("src/pending.txt").exists());
        let denied = engine.handle(domain::Request {
            id: "wrong-approval".into(),
            command: domain::Command::ApproveTool {
                run_id: "waiting-resume".into(),
                approval_id: "wrong".into(),
                approved: true,
            },
        });
        assert!(denied.result.is_err());
        assert!(!work.join("src/pending.txt").exists());
        let accepted = engine.handle(domain::Request {
            id: "approve".into(),
            command: domain::Command::ApproveTool {
                run_id: "waiting-resume".into(),
                approval_id: approval,
                approved: true,
            },
        });
        assert!(accepted.result.is_ok(), "{:?}", accepted.result);
        assert_eq!(
            std::fs::read_to_string(work.join("src/pending.txt")).unwrap(),
            "approved"
        );
        let _ = engine.handle(domain::Request {
            id: "cancel".into(),
            command: domain::Command::CancelRun {
                run_id: "waiting-resume".into(),
            },
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while engine.slots.load(std::sync::atomic::Ordering::Acquire) != 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // 실제 대기 호출과 checkpoint가 재시작 초기화 뒤 같은 승인으로 재개되는지 확인한다.
        let endpoint = "http://127.0.0.1:1/mcp";
        crate::mcp_connections::configure(&db, "project", endpoint).unwrap();
        let mut mcp_configs = configs.clone();
        mcp_configs[1].tools.push("mcp".into());
        let mut mcp_project = project.clone();
        mcp_project["agent_configs"] = serde_json::to_value(&mcp_configs).unwrap();
        let mut mcp_run = before_resume.clone();
        mcp_run["id"] = "waiting-mcp".into();
        mcp_run["agent_active"] = true.into();
        mcp_run["agent_attempt"] = 1.into();
        mcp_run["autonomy_mode"] = "step".into();
        mcp_run["agent_configs"] = serde_json::to_value(&mcp_configs).unwrap();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(crate::PROJECTS)
                .unwrap()
                .insert("project", mcp_project.to_string().as_str())
                .unwrap();
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("waiting-mcp", mcp_run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let mcp_id = serde_json::to_string(&("task", "implementer", 0, "mcp")).unwrap();
        let mcp_args = serde_json::json!({"endpoint":endpoint,"name":"write","arguments":{},"cwd":".","side_effect":true});
        let pending = crate::runs::submit_bound_mcp(
            &db,
            "waiting-mcp",
            "implementer",
            "task",
            &mcp_id,
            mcp_args.clone(),
        )
        .unwrap();
        let approval = pending["approval_id"].as_str().unwrap().to_owned();
        let mut mcp_checkpoint = checkpoint.clone();
        mcp_checkpoint["role_index"] = 1.into();
        mcp_checkpoint["pending_calls"] =
            serde_json::json!([{"call_id":"mcp","name":"mcp","arguments":mcp_args}]);
        let mut mcp_config = resume_config.clone();
        mcp_config.implementer.tools = mcp_configs[1]
            .tools
            .iter()
            .map(|v| tool_schema(v))
            .collect();
        mcp_checkpoint["config_hash"] = resume_config_identity(&spec, &mcp_config).unwrap().into();
        mcp_checkpoint["model_identity"] = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&("mock", &mcp_configs, &script, Option::<&str>::None)).unwrap()
            )
        )
        .into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert("waiting-mcp", mcp_checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        crate::runs::initialize(&db).unwrap();
        let response = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "resume-mcp".into(),
                command: domain::Command::ResumeAgent {
                    run_id: "waiting-mcp".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(response.result.is_ok(), "{:?}", response.result);
        let resumed = crate::runs::get(&db, "waiting-mcp").unwrap();
        assert_eq!(resumed["pending"]["id"], approval);
        assert_eq!(resumed["agent_attempt"], 2);
        assert_eq!(resumed["status"], "awaiting_approval");
        let _ = engine.handle(domain::Request {
            id: "cancel-mcp".into(),
            command: domain::Command::CancelRun {
                run_id: "waiting-mcp".into(),
            },
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while engine.slots.load(std::sync::atomic::Ordering::Acquire) != 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(crate::PROJECTS)
                .unwrap()
                .insert("project", project.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        std::fs::remove_file(work.join("src/pending.txt")).unwrap();
        let mut completed_run = before_resume.clone();
        completed_run["id"] = "completed-resume".into();
        completed_run["agent_active"] = true.into();
        completed_run["agent_attempt"] = 1.into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("completed-resume", completed_run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let completed_args = serde_json::json!({"path":"src/completed.txt","content":"once"});
        let completed_call = serde_json::to_string(&("task", "implementer", 0, "patch")).unwrap();
        let pending = crate::runs::submit_bound_patch(
            &db,
            "completed-resume",
            "implementer",
            "task",
            &completed_call,
            completed_args.clone(),
        )
        .unwrap();
        let approval = pending["approval_id"].as_str().unwrap().to_owned();
        let mut completed_checkpoint = checkpoint.clone();
        completed_checkpoint["role_index"] = 1.into();
        completed_checkpoint["pending_calls"] =
            serde_json::json!([{"call_id":"patch","name":"patch","arguments":completed_args}]);
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(
                    "completed-resume",
                    completed_checkpoint.to_string().as_str(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        let result = engine.handle(domain::Request {
            id: "approve-completed".into(),
            command: domain::Command::ApproveTool {
                run_id: "completed-resume".into(),
                approval_id: approval,
                approved: true,
            },
        });
        assert!(result.result.is_ok());
        let mut stopped = crate::runs::get(&db, "completed-resume").unwrap();
        stopped["agent_active"] = false.into();
        stopped["status"] = "interrupted".into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert("completed-resume", stopped.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        std::fs::write(work.join("src/completed.txt"), "preserve").unwrap();
        let result = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "resume-completed".into(),
                command: domain::Command::ResumeAgent {
                    run_id: "completed-resume".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(result.result.is_ok(), "{:?}", result.result);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while engine.slots.load(std::sync::atomic::Ordering::Acquire) != 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (_, progressed) = crate::runs::load_checkpoint(&db, "completed-resume")
            .unwrap()
            .unwrap();
        assert!(progressed["pending_calls"].as_array().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(work.join("src/completed.txt")).unwrap(),
            "preserve"
        );
        std::fs::remove_file(work.join("src/completed.txt")).unwrap();
        std::fs::write(work.join("src/a.txt"), "before").unwrap();
        let mut unsupported = project.clone();
        unsupported["approved"]["tasks"][0]["validation"] = serde_json::json!(["exec_json:{}"]);
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            projects
                .insert("project", unsupported.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let before = crate::runs::get(&db, "run").unwrap();
        let rejected = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "unsupported".into(),
                command: domain::Command::StartAgent {
                    run_id: "run".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(rejected.result.is_err());
        assert_eq!(crate::runs::get(&db, "run").unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(work.join("src/a.txt")).unwrap(),
            "before"
        );
        assert_eq!(engine.slots.load(std::sync::atomic::Ordering::Acquire), 0);
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            projects
                .insert("project", project.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        *TEST_HANDOFF_NONE.lock().unwrap() = Some("run".into());
        let response = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "start".into(),
                command: domain::Command::StartAgent {
                    run_id: "run".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(response.result.is_ok(), "{:?}", response.result);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let run = loop {
            let v = crate::runs::get(&db, "run").unwrap();
            if engine.slots.load(std::sync::atomic::Ordering::Acquire) == 0 {
                break v;
            }
            assert!(tokio::time::Instant::now() < deadline, "{v}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            engine.completions.for_run("run").is_object(),
            "expected pending completion, run: {run}"
        );
        let cursor_before = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("mock_calls"))
                .unwrap();
            let value = table.get("run:cursor").unwrap().unwrap().value();
            value
        };
        assert_eq!(cursor_before, 5);
        let calls_before = run["used_calls"].clone();
        let project_before = engine
            .handle(domain::Request {
                id: "project-before".into(),
                command: domain::Command::GetProject {
                    project_id: "project".into(),
                },
            })
            .result
            .unwrap();
        let context_change = engine.handle(domain::Request {
            id: "change-context".into(),
            command: domain::Command::SetAutonomy {
                project_id: "project".into(),
                mode: "step".into(),
            },
        });
        assert!(context_change.result.is_err());
        let project_after = engine
            .handle(domain::Request {
                id: "project-after".into(),
                command: domain::Command::GetProject {
                    project_id: "project".into(),
                },
            })
            .result
            .unwrap();
        assert_eq!(
            serde_json::to_value(project_before).unwrap(),
            serde_json::to_value(project_after).unwrap()
        );
        let rejected = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "retry-start".into(),
                command: domain::Command::StartAgent {
                    run_id: "run".into(),
                    mock_script: script.clone(),
                },
            })
            .await;
        assert!(rejected.result.is_err());
        assert_eq!(crate::runs::get(&db, "run").unwrap(), run);
        let retry = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: "run".into(),
            },
        });
        assert!(retry.result.is_ok(), "{:?}", retry.result);
        let run = crate::runs::get(&db, "run").unwrap();
        let cursor_after = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("mock_calls"))
                .unwrap();
            let value = table.get("run:cursor").unwrap().unwrap().value();
            value
        };
        assert_eq!(cursor_after, cursor_before);
        assert_eq!(run["used_calls"], calls_before);
        assert_eq!(run["agent_active"], false, "{run}");
        assert_eq!(run["status"], "completed", "{run}");
        assert_eq!(
            std::fs::read_to_string(work.join("src/a.txt")).unwrap(),
            "after"
        );
        let evidence = run["evidence"].as_array().unwrap();
        assert!(evidence.iter().any(|e| e["verification_id"]
            == "[\"task\",\"final_validation\",0]"
            && e["passed"] == true));
        let tx = db.begin_read().unwrap();
        let calls = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
            .unwrap();
        let review: serde_json::Value =
            serde_json::from_str(calls.get("run:final-review").unwrap().unwrap().value()).unwrap();
        assert_eq!(review["result"]["approved"], true);
        assert_eq!(review["result"]["binding"]["agent_attempt"], 1);
        drop(calls);
        drop(tx);
        std::fs::write(work.join("src/a.txt"), "before").unwrap();
        let mut second = before.clone();
        second["id"] = "cancel-after".into();
        let tx = db.begin_write().unwrap();
        {
            let mut runs = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            runs.insert("cancel-after", second.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        *TEST_HANDOFF_NONE.lock().unwrap() = Some("cancel-after".into());
        let response = Arc::clone(&engine)
            .handle_async(domain::Request {
                id: "start-second".into(),
                command: domain::Command::StartAgent {
                    run_id: "cancel-after".into(),
                    mock_script: script,
                },
            })
            .await;
        assert!(response.result.is_ok(), "{:?}", response.result);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let second = loop {
            let value = crate::runs::get(&db, "cancel-after").unwrap();
            if engine.slots.load(std::sync::atomic::Ordering::Acquire) == 0 {
                break value;
            }
            assert!(tokio::time::Instant::now() < deadline, "{value}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            engine.completions.for_run("cancel-after").is_object(),
            "{second}"
        );
        let cursor_before = {
            let tx = db.begin_read().unwrap();
            let t = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("mock_calls"))
                .unwrap();
            let value = t.get("cancel-after:cursor").unwrap().unwrap().value();
            value
        };
        assert_eq!(cursor_before, 5);
        let calls_before = second["used_calls"].clone();
        let cancel = engine.handle(domain::Request {
            id: "cancel".into(),
            command: domain::Command::CancelRun {
                run_id: "cancel-after".into(),
            },
        });
        assert!(cancel.result.is_ok());
        let retry = engine.handle(domain::Request {
            id: "retry-cancelled".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: "cancel-after".into(),
            },
        });
        assert!(retry.result.is_ok(), "{:?}", retry.result);
        let second = crate::runs::get(&db, "cancel-after").unwrap();
        let cursor_after = {
            let tx = db.begin_read().unwrap();
            let t = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("mock_calls"))
                .unwrap();
            let value = t.get("cancel-after:cursor").unwrap().unwrap().value();
            value
        };
        assert_eq!(cursor_after, cursor_before);
        assert_eq!(second["used_calls"], calls_before);
        assert_eq!(second["agent_active"], false, "{second}");
        assert_eq!(second["status"], "cancelled", "{second}");
        drop(db);
        drop(engine);
        git(&repo, &["worktree", "remove", "--force", target]);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(base.parent(), Some(temp.as_path()));
        assert!(base
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-start-agent-"));
        std::fs::remove_dir_all(base).unwrap();
    }
}
