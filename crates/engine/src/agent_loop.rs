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
