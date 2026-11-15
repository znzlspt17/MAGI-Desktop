use domain::ApprovedSpec;
use std::collections::{HashMap, HashSet};

// 2026-11-14: 승인된 작업의 소유 경로와 의존성을 먼저 검증해 병렬 변경 충돌과 교착을 막는다.
pub fn validate_plan(spec: &ApprovedSpec) -> Result<Vec<String>, String> {
    if spec.hash.trim().is_empty()
        || spec.version == 0
        || spec.base_revision.trim().is_empty()
        || spec.tasks.is_empty()
    {
        return Err("approved specification and tasks required".into());
    }
    let mut ids = HashMap::new();

    for (i, task) in spec.tasks.iter().enumerate() {
        if task.id.trim().is_empty()
            || ids.insert(task.id.as_str(), i).is_some()
            || task.validation.is_empty()
            || task.validation.iter().any(|v| v.trim().is_empty())
            || task.owned_paths.is_empty()
        {
            return Err("invalid task".into());
        }
        for raw in &task.owned_paths {
            let path = raw.replace('\\', "/").to_lowercase();
            if path.is_empty()
                || path.starts_with('/')
                || path.contains(':')
                || path
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
            {
                return Err("invalid owned path".into());
            }
            if !spec.scope.iter().any(|s| {
                let s = s.replace('\\', "/").trim_end_matches('/').to_lowercase();
                !s.is_empty() && (path == s || path.starts_with(&(s + "/")))
            }) {
                return Err("owned path outside approved scope".into());
            }
        }
    }
    let mut binding_ids = HashSet::new();
    for binding in &spec.task_bindings {
        let valid_id = |id: &str, prefix: &str, len: usize| {
            id.strip_prefix(prefix)
                .and_then(|v| v.parse::<usize>().ok())
                .is_some_and(|i| i < len && id == format!("{prefix}{i}"))
        };
        if !ids.contains_key(binding.task_id.as_str())
            || !binding_ids.insert(binding.task_id.as_str())
            || binding.goal.trim().is_empty()
            || binding.requirement_ids.is_empty()
            || binding.acceptance_ids.is_empty()
            || binding
                .requirement_ids
                .iter()
                .any(|id| !valid_id(id, "requirement-", spec.requirements.len()))
            || binding
                .acceptance_ids
                .iter()
                .any(|id| !valid_id(id, "acceptance-", spec.acceptance.len()))
        {
            return Err("invalid task binding".into());
        }
    }
    if binding_ids.len() != spec.tasks.len() {
        return Err("missing task binding".into());
    }
    let mut done = HashSet::new();
    let mut ordered = Vec::new();
    while ordered.len() < spec.tasks.len() {
        let mut progress = false;
        for task in &spec.tasks {
            if done.contains(task.id.as_str()) {
                continue;
            }
            if task
                .dependencies
                .iter()
                .any(|d| !ids.contains_key(d.as_str()))
            {
                return Err("missing dependency".into());
            }
            if task.dependencies.iter().all(|d| done.contains(d.as_str())) {
                done.insert(task.id.as_str());
                ordered.push(task.id.clone());
                progress = true;
            }
        }
        if !progress {
            return Err("dependency cycle".into());
        }
    }
    Ok(ordered)
}

// 2026-11-15: 동시 요청의 예약량을 실제 사용량과 분리해 중복 예산 사용을 막는다.
#[derive(Default)]
struct TokenBudget {
    used: u64,
    reserved: u64,
}

#[derive(Clone)]
pub struct RoleConfig {
    pub name: String,
    pub tools: Vec<serde_json::Value>,
    pub max_rounds: usize,
}
#[derive(Clone)]
pub struct LoopConfig {
    pub coordinator: RoleConfig,
    pub implementer: RoleConfig,
    pub reviewer: RoleConfig,
    pub max_tokens: u64,
    pub max_parallel_tasks: usize,
    pub timeout: std::time::Duration,
}
pub trait ModelDriver: Send + Sync {
    fn generate(
        &self,
        role: &str,
        input: serde_json::Value,
        tools: Vec<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        remaining_tokens: u64,
    ) -> impl std::future::Future<Output = Result<provider::Generation, String>> + Send;
}
pub trait ToolDriver: Send + Sync {
    fn progress(&self, _stage: &str, _task_id: &str, _role: &str) {}
    fn fresh(
        &self,
        spec: &ApprovedSpec,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    fn execute(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        name: &str,
        call_id: &str,
        arguments: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send;
    fn verify(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = Result<domain::VerificationEvidence, String>> + Send;
}

// 2026-11-14: 각 역할의 호출 이력과 도구 결과를 유지하고 검증 실패 시 완료를 막는다.
pub async fn run_loop<M: ModelDriver, T: ToolDriver>(
    model: &M,
    runtime: &T,
    spec: &ApprovedSpec,
    config: &LoopConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    use futures_util::{stream::FuturesUnordered, StreamExt};
    let ordered = validate_plan(spec)?;
    if config.max_tokens == 0
        || config.timeout.is_zero()
        || !(1..=8).contains(&config.max_parallel_tasks)
    {
        return Err("execution limits required".into());
    }
    if [&config.coordinator, &config.implementer, &config.reviewer]
        .iter()
        .any(|r| r.name.trim().is_empty() || r.max_rounds == 0)
    {
        return Err("invalid role configuration".into());
    }
    let task_by_id: HashMap<_, _> = spec.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let work_cancel = cancel.child_token();
    let work = async {
        let budget = std::sync::Mutex::new(TokenBudget::default());
        let seen_calls = std::sync::Mutex::new(HashSet::new());
        let mut done = HashSet::<String>::new();
        while done.len() < ordered.len() {
            let mut selected: Vec<&domain::TaskSpec> = Vec::new();
            for id in &ordered {
                let task = task_by_id[id.as_str()];
                if done.contains(id) || !task.dependencies.iter().all(|d| done.contains(d)) {
                    continue;
                }
                let overlaps = selected.iter().any(|other| {
                    task.owned_paths.iter().any(|a| {
                        other.owned_paths.iter().any(|b| {
                            let a = a.replace('\\', "/").to_lowercase();
                            let b = b.replace('\\', "/").to_lowercase();
                            a == b || a.starts_with(&(b.clone() + "/")) || b.starts_with(&(a + "/"))
                        })
                    })
                });
                if !overlaps {
                    selected.push(task);
                }
                if selected.len() == config.max_parallel_tasks {
                    break;
                }
            }
            if selected.is_empty() {
                return Err("task scheduling stalled".into());
            }
            let mut batch = FuturesUnordered::new();
            for task in selected {
                let budget = &budget;
                let seen_calls = &seen_calls;
                let work_cancel = work_cancel.clone();
                batch.push(async move {
            let mut verification_evidence = Vec::<domain::VerificationEvidence>::new();
            for (kind, role) in [
                ("coordinator", &config.coordinator),
                ("implementer", &config.implementer),
                ("reviewer", &config.reviewer),
            ] {
                runtime.progress("role_started", &task.id, kind);
                let role_request = serde_json::json!({
                    "role": kind,
                    "review_output": "reviewer must return JSON with boolean approved",
                    "approved_hash": spec.hash,
                    "approved_version": spec.version,
                    "base_revision": spec.base_revision,
                    "requirements": spec.requirements,
                    "constraints": spec.constraints,
                    "acceptance": spec.acceptance,
                    "task": task,
                    "binding": spec.task_bindings.iter().find(|binding| binding.task_id == task.id),
                    "verification_evidence": verification_evidence,
                });
                let mut input = vec![serde_json::json!({
                    "role": "user",
                    "content": role_request.to_string(),
                })];
                let mut finished = false;
                for round in 0..role.max_rounds {
                    if work_cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    runtime.fresh(spec).await?;
                    // 2026-11-14: 병렬 요청은 입력 상한과 출력 한도를 먼저 예약해 비용 한도 중복 사용을 막는다.
                    let input_bound = (serde_json::to_vec(&input).map_err(|_| "invalid input")?.len()
                        + serde_json::to_vec(&role.tools).map_err(|_| "invalid tools")?.len()
                        + 64) as u64;
                    let reservation = {
                        let mut budget = budget.lock().map_err(|_| "budget unavailable")?;
                        let available = config.max_tokens.saturating_sub(budget.used).saturating_sub(budget.reserved);
                        let share = (config.max_tokens / config.max_parallel_tasks as u64).max(1);
                        let reserved = available.min(share);
                        if reserved <= input_bound {
                            return Err("token budget insufficient for input".into());
                        }
                        budget.reserved += reserved;
                        reserved
                    };
                    let output_limit = reservation - input_bound;
                    let result = model.generate(
                        &role.name,
                        serde_json::Value::Array(input.clone()),
                        role.tools.clone(),
                        work_cancel.clone(),
                        output_limit,
                    ).await;
                    let generation = match result {
                        Ok(generation) => generation,
                        Err(error) => {
                            let mut budget = budget.lock().map_err(|_| "budget unavailable")?;
                            budget.reserved -= reservation;
                            budget.used = budget.used.saturating_add(reservation);
                            return Err(error);
                        }
                    };
                    let tokens = generation.usage.as_ref()
                        .and_then(|usage| usage.get("total_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .ok_or("provider token usage missing")?;
                    {
                        let mut budget = budget.lock().map_err(|_| "budget unavailable")?;
                        budget.reserved -= reservation;
                        if tokens > reservation {
                            return Err("provider exceeded reserved token budget".into());
                        }
                        budget.used = budget.used.checked_add(tokens)
                            .filter(|used| *used <= config.max_tokens)
                            .ok_or("token limit exceeded")?;
                    }
                    input.extend(generation.output);
                    if generation.tool_calls.is_empty() {
                        if kind == "reviewer"
                            && serde_json::from_str::<serde_json::Value>(&generation.text)
                                .ok()
                                .and_then(|v| {
                                    v.get("approved").and_then(serde_json::Value::as_bool)
                                })
                                != Some(true)
                        {
                            return Err("review rejected or invalid".into());
                        }
                        finished = true;
                        break;
                    }
                    for call in generation.tool_calls {
                        if kind == "reviewer" && !matches!(call.name.as_str(), "read" | "search") {
                            return Err("reviewer tool forbidden".into());
                        }
                        if !role.tools.iter().any(|t| {
                            t.get("name").and_then(serde_json::Value::as_str)
                                == Some(call.name.as_str())
                        }) {
                            return Err("tool not allowed".into());
                        }
                        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                            .map_err(|_| "invalid tool arguments")?;
                        let logical_id=serde_json::to_string(&(&task.id,kind,round,&call.call_id)).map_err(|_|"invalid call ID")?;
                        if !arguments.is_object()
                            || call.call_id.is_empty()
                            || !seen_calls.lock().map_err(|_|"call state unavailable")?.insert(logical_id.clone())
                        {
                            return Err("invalid or duplicate tool call".into());
                        }
                        runtime.fresh(spec).await?;
                        runtime.progress("tool_started", &task.id, kind);
                        let result = runtime
                            .execute(
                                kind,
                                task,
                                &call.name,
                                &logical_id,
                                arguments,
                                work_cancel.clone(),
                            )
                            .await?;
                        input.push(serde_json::json!({"type":"function_call_output","call_id":call.call_id,"output":result.to_string()}));
                    }
                }
                if !finished {
                    return Err("role round limit exceeded".into());
                }
                runtime.progress("role_completed", &task.id, kind);
                if kind == "implementer" {
                    for command in &task.validation {
                        runtime.fresh(spec).await?;
                        let evidence = runtime.verify(task, command, work_cancel.clone()).await?;
                        let binding = spec
                            .task_bindings
                            .iter()
                            .find(|b| b.task_id == task.id)
                            .ok_or("missing task binding")?;
                        if !evidence.passed
                            || evidence.command != *command
                            || evidence.spec_hash != spec.hash
                            || evidence.revision.trim().is_empty()
                            || evidence.worktree_hash.trim().is_empty()
                            || evidence.requirement_ids != binding.requirement_ids
                            || evidence.acceptance_ids != binding.acceptance_ids
                        {
                            return Err("validation failed or evidence invalid".into());
                        }
                        verification_evidence.push(evidence);
                        runtime.progress("validation_passed", &task.id, kind);
                    }
                }
            }
                    Ok::<String,String>(task.id.clone())
                });
            }
            while let Some(result) = batch.next().await {
                match result {
                    Ok(id) => {
                        done.insert(id);
                    }
                    Err(e) => {
                        work_cancel.cancel();
                        return Err(e);
                    }
                }
            }
        }
        runtime.fresh(spec).await?;
        Ok(())
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = tokio::time::timeout(config.timeout, work) => {
            result.map_err(|_| "execution timed out".to_owned()).and_then(|r| r)
        }
    };
    work_cancel.cancel();
    result
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CheckpointCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LoopCheckpoint {
    pub schema_version: u32,
    pub revision: u64,
    pub spec_hash: String,
    pub spec_version: u64,
    pub model_identity: String,
    pub config_hash: String,
    pub task_index: usize,
    pub role_index: usize,
    pub round: usize,
    pub input: Vec<serde_json::Value>,
    pub pending_calls: Vec<CheckpointCall>,
    pub pending_index: usize,
    pub evidence: Vec<domain::VerificationEvidence>,
    pub completed_tasks: Vec<String>,
    pub used_calls: Vec<String>,
    pub tokens_used: u64,
    pub model_inflight: bool,
    pub role_complete: bool,
    pub validation_index: usize,
    pub reserved_tokens: u64,
    #[serde(default)]
    pub task_states: std::collections::BTreeMap<String, Box<LoopCheckpoint>>,
}
/// 2026-11-16: revision 비교와 저장은 원자적으로 수행해야 이전 실행의 진행 상태를 덮어쓰지 않는다.
pub trait CheckpointStore: Send + Sync {
    fn load(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<LoopCheckpoint>, String>> + Send;
    fn save(
        &self,
        expected_revision: u64,
        checkpoint: LoopCheckpoint,
    ) -> impl std::future::Future<Output = Result<u64, String>> + Send;
}
/// 2026-11-16: 같은 logical ID로 재호출될 수 있으므로 완료 결과를 재사용해야 한다.
/// 결과가 불명확한 부작용은 driver가 재실행하지 않고 오류로 돌려줘야 한다.
pub trait DurableToolDriver: ToolDriver {
    fn verify_durable(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        verification_id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = Result<domain::VerificationEvidence, String>> + Send;
    fn execute_durable(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        name: &str,
        call_id: &str,
        arguments: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send;
}

// 2026-11-14: 대화와 호출 커서를 부작용보다 먼저 저장하고 완료 결과는 durable 호출 기록에서 재사용한다.
// 2026-11-14: 재개 가능한 루프의 모델, 저장소 및 격리된 작업 필터를 명시적으로 전달한다.
#[allow(clippy::too_many_arguments)]
async fn run_checkpointed_serial<M: ModelDriver, T: DurableToolDriver, S: CheckpointStore>(
    model: &M,
    runtime: &T,
    store: &S,
    spec: &ApprovedSpec,
    config: &LoopConfig,
    model_identity: &str,
    cancel: tokio_util::sync::CancellationToken,
    task_filter: Option<&str>,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut ordered = validate_plan(spec)?;
    if let Some(id) = task_filter {
        ordered.retain(|candidate| candidate == id);
        if ordered.is_empty() {
            return Err("task not found".into());
        }
    }
    if config.max_parallel_tasks != 1
        || config.max_tokens == 0
        || config.timeout.is_zero()
        || model_identity.trim().is_empty()
    {
        return Err("invalid durable execution configuration".into());
    }
    let roles = [
        ("coordinator", &config.coordinator),
        ("implementer", &config.implementer),
        ("reviewer", &config.reviewer),
    ];
    if roles
        .iter()
        .any(|(_, r)| r.name.trim().is_empty() || r.max_rounds == 0)
    {
        return Err("invalid role configuration".into());
    }
    let settings = serde_json::json!({
        "roles": roles.iter().map(|(kind, role)| serde_json::json!({
            "kind": kind, "name": role.name, "tools": role.tools, "rounds": role.max_rounds,
        })).collect::<Vec<_>>(),
        "tokens": config.max_tokens,
        "parallel": config.max_parallel_tasks,
        "timeout_ms": config.timeout.as_millis().to_string(),
    });
    let config_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&settings).map_err(|_| "invalid configuration")?)
    );
    let mut state = match store.load().await? {
        Some(state) => state,
        None => LoopCheckpoint {
            schema_version: 1,
            revision: 0,
            spec_hash: spec.hash.clone(),
            spec_version: spec.version,
            model_identity: model_identity.into(),
            config_hash: config_hash.clone(),
            task_index: 0,
            role_index: 0,
            round: 0,
            input: vec![],
            pending_calls: vec![],
            pending_index: 0,
            evidence: vec![],
            completed_tasks: vec![],
            used_calls: vec![],
            tokens_used: 0,
            model_inflight: false,
            role_complete: false,
            validation_index: 0,
            reserved_tokens: 0,
            task_states: std::collections::BTreeMap::new(),
        },
    };
    if state.schema_version != 1
        || state.spec_hash != spec.hash
        || state.spec_version != spec.version
        || state.model_identity != model_identity
        || state.config_hash != config_hash
        || state.task_index > ordered.len()
        || state.role_index > 2
        || state.pending_index > state.pending_calls.len()
        || state.tokens_used > config.max_tokens
        || state.completed_tasks != ordered[..state.task_index]
    {
        return Err("checkpoint does not match run configuration".into());
    }
    async fn persist<S: CheckpointStore>(
        store: &S,
        state: &mut LoopCheckpoint,
    ) -> Result<(), String> {
        if serde_json::to_vec(state)
            .map_err(|_| "invalid checkpoint")?
            .len()
            > 2 * 1024 * 1024
        {
            return Err("checkpoint exceeds size limit".into());
        }
        let expected = state.revision;
        let revision = store.save(expected, state.clone()).await?;
        if revision
            != expected
                .checked_add(1)
                .ok_or("checkpoint revision overflow")?
        {
            return Err("invalid checkpoint revision".into());
        }
        state.revision = revision;
        Ok(())
    }
    if state.revision == 0 {
        persist(store, &mut state).await?;
    }
    let work_cancel = cancel.child_token();
    let work = async {
        if state.model_inflight {
            state.model_inflight = false;
            state.reserved_tokens = 0;
            persist(store, &mut state).await?;
        }
        while state.task_index < ordered.len() {
            if work_cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            runtime.fresh(spec).await?;
            let task = spec
                .tasks
                .iter()
                .find(|t| t.id == ordered[state.task_index])
                .ok_or("checkpoint task missing")?;
            let (kind, role) = roles[state.role_index];
            let binding = spec
                .task_bindings
                .iter()
                .find(|b| b.task_id == task.id)
                .ok_or("task binding missing")?;
            if state.input.is_empty() {
                let role_request = serde_json::json!({
                    "role": kind,
                    "review_output": "reviewer must return JSON with boolean approved",
                    "approved_hash": spec.hash,
                    "approved_version": spec.version,
                    "base_revision": spec.base_revision,
                    "requirements": spec.requirements,
                    "constraints": spec.constraints,
                    "acceptance": spec.acceptance,
                    "task": task,
                    "binding": binding,
                    "verification_evidence": state.evidence,
                });
                state.input.push(serde_json::json!({
                    "role": "user",
                    "content": role_request.to_string(),
                }));
                runtime.progress("role_started", &task.id, kind);
                persist(store, &mut state).await?;
            }
            if state.pending_index < state.pending_calls.len() {
                let call = state.pending_calls[state.pending_index].clone();
                if call.call_id.is_empty()
                    || !call.arguments.is_object()
                    || (kind == "reviewer" && !matches!(call.name.as_str(), "read" | "search"))
                    || !role.tools.iter().any(|t| {
                        t.get("name").and_then(serde_json::Value::as_str)
                            == Some(call.name.as_str())
                    })
                {
                    return Err("invalid saved tool call".into());
                }
                let logical_id =
                    serde_json::to_string(&(&task.id, kind, state.round, &call.call_id))
                        .map_err(|_| "invalid call ID")?;
                runtime.fresh(spec).await?;
                runtime.progress("tool_started", &task.id, kind);
                let output = runtime
                    .execute_durable(
                        kind,
                        task,
                        &call.name,
                        &logical_id,
                        call.arguments,
                        work_cancel.clone(),
                    )
                    .await?;
                state.input.push(serde_json::json!({"type":"function_call_output","call_id":call.call_id,"output":output.to_string()}));
                state.used_calls.push(logical_id);
                state.pending_index += 1;
                if state.pending_index == state.pending_calls.len() {
                    state.pending_calls.clear();
                    state.pending_index = 0;
                }
                persist(store, &mut state).await?;
                continue;
            }
            if !state.role_complete {
                if state.round >= role.max_rounds {
                    return Err("role round limit exceeded".into());
                }
                let input_bound = (serde_json::to_vec(&state.input)
                    .map_err(|_| "invalid input")?
                    .len()
                    + serde_json::to_vec(&role.tools)
                        .map_err(|_| "invalid tools")?
                        .len()
                    + 64) as u64;
                let reservation = config.max_tokens.saturating_sub(state.tokens_used);
                if reservation <= input_bound {
                    return Err("token budget insufficient for input".into());
                }
                state.tokens_used += reservation;
                state.reserved_tokens = reservation;
                state.model_inflight = true;
                persist(store, &mut state).await?;
                let generation = model
                    .generate(
                        &role.name,
                        serde_json::Value::Array(state.input.clone()),
                        role.tools.clone(),
                        work_cancel.clone(),
                        reservation - input_bound,
                    )
                    .await?;
                let usage = generation
                    .usage
                    .as_ref()
                    .and_then(|u| u.get("total_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("provider token usage missing")?;
                if usage > reservation {
                    return Err("provider exceeded reserved token budget".into());
                }
                state.tokens_used = state.tokens_used - reservation + usage;
                state.reserved_tokens = 0;
                state.model_inflight = false;
                state.round += 1;
                state.input.extend(generation.output);
                if generation.tool_calls.is_empty() {
                    if kind == "reviewer"
                        && serde_json::from_str::<serde_json::Value>(&generation.text)
                            .ok()
                            .and_then(|v| v.get("approved").and_then(serde_json::Value::as_bool))
                            != Some(true)
                    {
                        return Err("review rejected or invalid".into());
                    }
                    state.role_complete = true;
                } else {
                    let mut ids = HashSet::new();
                    for call in generation.tool_calls {
                        if kind == "reviewer" && !matches!(call.name.as_str(), "read" | "search") {
                            return Err("reviewer tool forbidden".into());
                        }
                        if !role.tools.iter().any(|t| {
                            t.get("name").and_then(serde_json::Value::as_str)
                                == Some(call.name.as_str())
                        }) || call.call_id.is_empty()
                            || !ids.insert(call.call_id.clone())
                        {
                            return Err("invalid tool call".into());
                        }
                        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                            .map_err(|_| "invalid tool arguments")?;
                        if !arguments.is_object() {
                            return Err("invalid tool arguments".into());
                        }
                        state.pending_calls.push(CheckpointCall {
                            call_id: call.call_id,
                            name: call.name,
                            arguments,
                        });
                    }
                }
                persist(store, &mut state).await?;
                continue;
            }
            if kind == "implementer" && state.validation_index < task.validation.len() {
                let command = &task.validation[state.validation_index];
                let verification_id =
                    serde_json::to_string(&(&task.id, "validation", state.validation_index))
                        .map_err(|_| "invalid verification ID")?;
                runtime.fresh(spec).await?;
                let evidence = runtime
                    .verify_durable(task, command, &verification_id, work_cancel.clone())
                    .await?;
                if !evidence.passed
                    || evidence.command != *command
                    || evidence.spec_hash != spec.hash
                    || evidence.revision.trim().is_empty()
                    || evidence.worktree_hash.trim().is_empty()
                    || evidence.requirement_ids != binding.requirement_ids
                    || evidence.acceptance_ids != binding.acceptance_ids
                {
                    return Err("validation failed or evidence invalid".into());
                }
                state.evidence.push(evidence);
                state.validation_index += 1;
                runtime.progress("validation_passed", &task.id, kind);
                persist(store, &mut state).await?;
                continue;
            }
            runtime.progress("role_completed", &task.id, kind);
            state.role_index += 1;
            state.round = 0;
            state.input.clear();
            state.role_complete = false;
            state.validation_index = 0;
            if state.role_index == 3 {
                state.completed_tasks.push(task.id.clone());
                state.task_index += 1;
                state.role_index = 0;
                state.evidence.clear();
            }
            persist(store, &mut state).await?;
        }
        runtime.fresh(spec).await?;
        Ok(())
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = tokio::time::timeout(config.timeout, work) => {
            result.map_err(|_| "execution timed out".to_owned()).and_then(|r| r)
        }
    };
    work_cancel.cancel();
    result
}

// 2026-11-14: 작업별 체크포인트를 CAS로 병합해 병렬 모델 응답이 서로의 대화 상태를 덮어쓰지 않게 한다.
struct TaskCheckpointStore<'a, S: CheckpointStore> {
    parent: &'a S,
    task_id: String,
    spec_hash: String,
    spec_version: u64,
    identity: String,
    config_hash: String,
    max_tokens: u64,
}
impl<S: CheckpointStore> TaskCheckpointStore<'_, S> {
    fn check(&self, s: &LoopCheckpoint) -> Result<(), String> {
        if s.schema_version != 2
            || s.spec_hash != self.spec_hash
            || s.spec_version != self.spec_version
            || s.model_identity != self.identity
            || s.config_hash != self.config_hash
        {
            return Err("parallel checkpoint does not match run configuration".into());
        }
        Ok(())
    }
}
impl<S: CheckpointStore> CheckpointStore for TaskCheckpointStore<'_, S> {
    async fn load(&self) -> Result<Option<LoopCheckpoint>, String> {
        let root = self
            .parent
            .load()
            .await?
            .ok_or("parallel checkpoint missing")?;
        self.check(&root)?;
        Ok(root.task_states.get(&self.task_id).map(|s| (**s).clone()))
    }
    async fn save(&self, expected_revision: u64, mut child: LoopCheckpoint) -> Result<u64, String> {
        for _ in 0..64 {
            let mut root = self
                .parent
                .load()
                .await?
                .ok_or("parallel checkpoint missing")?;
            self.check(&root)?;
            if root
                .task_states
                .get(&self.task_id)
                .map_or(0, |s| s.revision)
                != expected_revision
            {
                return Err("stale task checkpoint".into());
            }
            let next = expected_revision
                .checked_add(1)
                .ok_or("checkpoint revision overflow")?;
            child.revision = next;
            root.task_states
                .insert(self.task_id.clone(), Box::new(child.clone()));
            root.tokens_used = root
                .task_states
                .values()
                .try_fold(0u64, |sum, s| sum.checked_add(s.tokens_used))
                .ok_or("token budget exceeded")?;
            if root.tokens_used > self.max_tokens {
                return Err("token budget exceeded".into());
            }
            if serde_json::to_vec(&root)
                .map_err(|_| "invalid checkpoint")?
                .len()
                > 2 * 1024 * 1024
            {
                return Err("checkpoint exceeds size limit".into());
            }
            let revision = root.revision;
            match self.parent.save(revision, root).await {
                Ok(n) if n == revision + 1 => return Ok(next),
                Ok(_) => return Err("invalid checkpoint revision".into()),
                Err(error) => {
                    let latest = self
                        .parent
                        .load()
                        .await?
                        .ok_or("parallel checkpoint missing")?;
                    self.check(&latest)?;
                    if latest.revision == revision {
                        return Err(error);
                    }
                }
            }
        }
        Err("checkpoint contention limit reached".into())
    }
}

pub async fn run_checkpointed_loop<M: ModelDriver, T: DurableToolDriver, S: CheckpointStore>(
    model: &M,
    runtime: &T,
    store: &S,
    spec: &ApprovedSpec,
    config: &LoopConfig,
    model_identity: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    use futures_util::{stream::FuturesUnordered, StreamExt};
    use sha2::{Digest, Sha256};
    if config.max_parallel_tasks == 1 {
        return run_checkpointed_serial(
            model,
            runtime,
            store,
            spec,
            config,
            model_identity,
            cancel,
            None,
        )
        .await;
    }
    let ordered = validate_plan(spec)?;
    if !(2..=8).contains(&config.max_parallel_tasks)
        || model_identity.trim().is_empty()
        || config.timeout.is_zero()
    {
        return Err("invalid durable execution configuration".into());
    }
    // 각 모델 요청 예산을 체크포인트 CAS에 예약하고 전체 사용량을 원자적으로 제한한다.
    let per_task = config.max_tokens / (config.max_parallel_tasks as u64);
    if per_task == 0 {
        return Err("token budget insufficient".into());
    }
    let roles = [
        ("coordinator", &config.coordinator),
        ("implementer", &config.implementer),
        ("reviewer", &config.reviewer),
    ];
    let settings = serde_json::json!({
        "roles": roles.iter().map(|(kind, role)| serde_json::json!({
            "kind": kind, "name": role.name, "tools": role.tools, "rounds": role.max_rounds,
        })).collect::<Vec<_>>(),
        "tokens": config.max_tokens,
        "parallel": config.max_parallel_tasks,
        "timeout_ms": config.timeout.as_millis().to_string(),
        "task_order": ordered,
    });
    let config_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&settings).map_err(|_| "invalid configuration")?)
    );
    let root = match store.load().await? {
        Some(root) => root,
        None => {
            let root = LoopCheckpoint {
                schema_version: 2,
                revision: 0,
                spec_hash: spec.hash.clone(),
                spec_version: spec.version,
                model_identity: model_identity.into(),
                config_hash: config_hash.clone(),
                task_index: 0,
                role_index: 0,
                round: 0,
                input: vec![],
                pending_calls: vec![],
                pending_index: 0,
                evidence: vec![],
                completed_tasks: vec![],
                used_calls: vec![],
                tokens_used: 0,
                model_inflight: false,
                role_complete: false,
                validation_index: 0,
                reserved_tokens: 0,
                task_states: std::collections::BTreeMap::new(),
            };
            let revision = store.save(0, root.clone()).await?;
            if revision != 1 {
                return Err("invalid checkpoint revision".into());
            }
            let mut root = root;
            root.revision = revision;
            root
        }
    };
    if root.schema_version != 2
        || root.spec_hash != spec.hash
        || root.spec_version != spec.version
        || root.model_identity != model_identity
        || root.config_hash != config_hash
        || root.task_states.keys().any(|id| !ordered.contains(id))
    {
        return Err("checkpoint does not match run configuration".into());
    }
    let child_cancel = cancel.child_token();
    let work = async {
        loop {
            runtime.fresh(spec).await?;
            let root = store.load().await?.ok_or("parallel checkpoint missing")?;
            if root.schema_version != 2
                || root.spec_hash != spec.hash
                || root.spec_version != spec.version
                || root.model_identity != model_identity
                || root.config_hash != config_hash
                || root.tokens_used > config.max_tokens
            {
                return Err("parallel checkpoint changed".into());
            }
            let done: HashSet<String> = root
                .task_states
                .iter()
                .filter(|(id, s)| {
                    s.task_index == 1 && s.completed_tasks.as_slice() == [id.as_str()]
                })
                .map(|(id, _)| id.clone())
                .collect();
            if done.len() == ordered.len() {
                return Ok(());
            }
            let mut selected: Vec<&domain::TaskSpec> = vec![];
            for id in &ordered {
                let task = spec
                    .tasks
                    .iter()
                    .find(|t| &t.id == id)
                    .ok_or("task missing")?;
                if done.contains(id) || !task.dependencies.iter().all(|d| done.contains(d)) {
                    continue;
                }
                let overlap = selected.iter().any(|other| {
                    task.owned_paths.iter().any(|a| {
                        other.owned_paths.iter().any(|b| {
                            let a = a.replace('\\', "/").to_lowercase();
                            let b = b.replace('\\', "/").to_lowercase();
                            a == b || a.starts_with(&(b.clone() + "/")) || b.starts_with(&(a + "/"))
                        })
                    })
                });
                if !overlap {
                    selected.push(task);
                }
                if selected.len() == config.max_parallel_tasks {
                    break;
                }
            }
            if selected.is_empty() {
                return Err("task scheduling stalled".into());
            }
            let mut batch = FuturesUnordered::new();
            for task in selected {
                let mut child_config = config.clone();
                child_config.max_parallel_tasks = 1;
                child_config.max_tokens = per_task;
                let child_store = TaskCheckpointStore {
                    parent: store,
                    task_id: task.id.clone(),
                    spec_hash: spec.hash.clone(),
                    spec_version: spec.version,
                    identity: model_identity.into(),
                    config_hash: config_hash.clone(),
                    max_tokens: config.max_tokens,
                };
                let token = child_cancel.clone();
                let id = task.id.clone();
                batch.push(async move {
                    run_checkpointed_serial(
                        model,
                        runtime,
                        &child_store,
                        spec,
                        &child_config,
                        model_identity,
                        token,
                        Some(&id),
                    )
                    .await
                });
            }
            while let Some(result) = batch.next().await {
                if let Err(error) = result {
                    child_cancel.cancel();
                    return Err(error);
                }
            }
        }
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = tokio::time::timeout(config.timeout, work) => {
            result.map_err(|_| "execution timed out".to_owned()).and_then(|r| r)
        }
    };
    child_cancel.cancel();
    result
}
