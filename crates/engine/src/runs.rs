use redb::{Database, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};
const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");
const COMPLETIONS: TableDefinition<&str, u8> = TableDefinition::new("completion_commits_v1");
fn checkpoint_has_inflight(value: &serde_json::Value) -> bool {
    value["model_inflight"] != false
        || value["task_states"]
            .as_object()
            .is_some_and(|states| states.values().any(checkpoint_has_inflight))
}

// 2026-11-16: 미소모 승인만 체크포인트 커서·인자·ledger와 결박해 재개한다.
fn waiting_checkpoint_call(
    run: &serde_json::Value,
    saved: &crate::agent_loop::LoopCheckpoint,
    spec: &domain::ApprovedSpec,
) -> Result<(String, String, String, String, String), String> {
    use sha2::{Digest, Sha256};
    if !((run["status"] == "awaiting_approval" && run["pending"]["authorized"] == false)
        || (run["status"] == "approved_mcp"
            && run["pending"]["tool"] == "mcp"
            && run["pending"]["authorized"] == true)
        || (run["status"] == "approved_exec"
            && run["pending"]["tool"] == "exec"
            && run["pending"]["authorized"] == true))
        || !["patch", "mcp", "exec"].contains(&run["pending"]["tool"].as_str().unwrap_or(""))
    {
        return Err("pending approval cannot resume safely".into());
    }
    let expected = run["pending"]["call_id"]
        .as_str()
        .ok_or("pending call missing")?;
    let approval = run["pending"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("pending approval missing")?;
    let ordered = crate::agent_loop::validate_plan(spec)?;
    let states: Vec<(String, &crate::agent_loop::LoopCheckpoint)> = if saved.schema_version == 1 {
        vec![(
            ordered
                .get(saved.task_index)
                .ok_or("checkpoint task missing")?
                .clone(),
            saved,
        )]
    } else {
        saved
            .task_states
            .iter()
            .map(|(id, state)| (id.clone(), state.as_ref()))
            .collect()
    };
    let mut found = None;
    for (task, state) in states {
        let Some(call) = state.pending_calls.get(state.pending_index) else {
            continue;
        };
        let role = match state.role_index {
            0 => "coordinator",
            1 => "implementer",
            2 => "reviewer",
            _ => return Err("invalid checkpoint role".into()),
        };
        let logical = serde_json::to_string(&(&task, role, state.round, &call.call_id))
            .map_err(|_| "invalid call identity")?;
        if logical != expected {
            continue;
        }
        if found.is_some()
            || call.name != run["pending"]["tool"]
            || call.arguments != run["pending"]["arguments"]
        {
            return Err("pending call mismatch".into());
        }
        let identity = serde_json::to_vec(&(role, &task, call.name.as_str(), &call.arguments))
            .map_err(|_| "invalid call identity")?;
        found = Some((
            logical,
            approval.to_owned(),
            format!("{:x}", Sha256::digest(identity)),
            role.to_owned(),
            task,
        ));
    }
    found.ok_or("pending checkpoint mismatch".into())
}

#[cfg(test)]
#[test]
fn waiting_checkpoint_accepts_exact_mcp_call() {
    let spec: domain::ApprovedSpec = serde_json::from_value(serde_json::json!({"version":1,"hash":"fixture","base_revision":"rev","scope":["src"],"requirements":["r"],"acceptance":["a"],"tasks":[{"id":"task","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/a:x"]}],"task_bindings":[{"task_id":"task","goal":"goal","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]})).unwrap();
    let args = serde_json::json!({"endpoint":"http://127.0.0.1:1/mcp","name":"write","arguments":{},"cwd":".","side_effect":true});
    let logical = serde_json::to_string(&("task", "implementer", 0, "mcp-1")).unwrap();
    let run = serde_json::json!({"status":"awaiting_approval","pending":{"tool":"mcp","authorized":false,"id":"approval","call_id":logical,"arguments":args}});
    let checkpoint:crate::agent_loop::LoopCheckpoint=serde_json::from_value(serde_json::json!({"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":"model","config_hash":"config","task_index":0,"role_index":1,"round":0,"input":[],"pending_calls":[{"call_id":"mcp-1","name":"mcp","arguments":args}],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0})).unwrap();
    assert!(waiting_checkpoint_call(&run, &checkpoint, &spec).is_ok());
    let mut approved = run.clone();
    approved["status"] = "approved_mcp".into();
    approved["pending"]["authorized"] = true.into();
    assert!(waiting_checkpoint_call(&approved, &checkpoint, &spec).is_ok());
    let args = serde_json::json!({"program":"fixture.exe","args":[],"cwd":"src"});
    let mut exec_run = run.clone();
    exec_run["pending"]["tool"] = "exec".into();
    exec_run["pending"]["arguments"] = args.clone();
    let mut exec_checkpoint = serde_json::to_value(&checkpoint).unwrap();
    exec_checkpoint["pending_calls"][0]["name"] = "exec".into();
    exec_checkpoint["pending_calls"][0]["arguments"] = args;
    let exec_checkpoint: crate::agent_loop::LoopCheckpoint =
        serde_json::from_value(exec_checkpoint).unwrap();
    assert!(waiting_checkpoint_call(&exec_run, &exec_checkpoint, &spec).is_ok());
    exec_run["status"] = "approved_exec".into();
    exec_run["pending"]["authorized"] = true.into();
    assert!(waiting_checkpoint_call(&exec_run, &exec_checkpoint, &spec).is_ok());
}

#[cfg(test)]
#[test]
fn completed_exec_checkpoint_requires_real_attempt_provenance() {
    let root = std::env::temp_dir().join(format!("magi-exec-provenance-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let db = Database::create(root.join("state.redb")).unwrap();
    let spec:domain::ApprovedSpec=serde_json::from_value(serde_json::json!({"version":1,"hash":"fixture","base_revision":"rev","scope":["src"],"requirements":["r"],"acceptance":["a"],"tasks":[{"id":"task","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/a:x"]}],"task_bindings":[{"task_id":"task","goal":"goal","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]})).unwrap();
    let args = serde_json::json!({"program":"fixture.exe","args":[],"cwd":"src"});
    let logical = serde_json::to_string(&("task", "implementer", 0, "exec-1")).unwrap();
    let run = serde_json::json!({"used_calls":[logical],"agent_attempt":2,"events":[]});
    let checkpoint:crate::agent_loop::LoopCheckpoint=serde_json::from_value(serde_json::json!({"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":"model","config_hash":"config","task_index":0,"role_index":1,"round":0,"input":[],"pending_calls":[{"call_id":"exec-1","name":"exec","arguments":args}],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0})).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut calls = tx.open_table(CALLS).unwrap();
        calls
            .insert(
                format!("run:{logical}").as_str(),
                r#"{"status":"completed","result":{"exit_code":0}}"#,
            )
            .unwrap();
    }
    tx.commit().unwrap();
    let tx = db.begin_write().unwrap();
    assert!(validate_completed_checkpoint_calls(&tx, &run, &checkpoint, &spec, "run").is_err());
    drop(tx);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// 2026-11-16: 커서 기록 전 완료된 효과는 실제 ledger와 동일할 때만 결과를 재사용한다.
fn validate_completed_checkpoint_calls(
    tx: &redb::WriteTransaction,
    run: &serde_json::Value,
    saved: &crate::agent_loop::LoopCheckpoint,
    spec: &domain::ApprovedSpec,
    run_id: &str,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let ordered = crate::agent_loop::validate_plan(spec)?;
    let states: Vec<(String, &crate::agent_loop::LoopCheckpoint)> = if saved.schema_version == 1 {
        match ordered.get(saved.task_index) {
            Some(id) => vec![(id.clone(), saved)],
            None => vec![],
        }
    } else {
        saved
            .task_states
            .iter()
            .map(|(id, state)| (id.clone(), state.as_ref()))
            .collect()
    };
    let calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
    for (task, state) in states {
        let Some(call) = state.pending_calls.get(state.pending_index) else {
            continue;
        };
        if call.name != "patch" && call.name != "exec" {
            continue;
        }
        let role = match state.role_index {
            0 => "coordinator",
            1 => "implementer",
            2 => "reviewer",
            _ => return Err("invalid checkpoint role".into()),
        };
        let logical = serde_json::to_string(&(&task, role, state.round, &call.call_id))
            .map_err(|_| "invalid call identity")?;
        let key = format!("{run_id}:{logical}");
        let Some(value) = calls.get(key.as_str()).map_err(|e| e.to_string())? else {
            if run["used_calls"]
                .as_array()
                .is_none_or(|ids| ids.iter().any(|id| id == &logical))
            {
                return Err("previous tool outcome uncertain".into());
            }
            continue;
        };
        if run["used_calls"]
            .as_array()
            .ok_or("invalid calls")?
            .iter()
            .filter(|id| id.as_str() == Some(logical.as_str()))
            .count()
            != 1
        {
            return Err("completed checkpoint call mismatch".into());
        }
        let record: serde_json::Value =
            serde_json::from_str(value.value()).map_err(|_| "invalid call record")?;
        let identity = serde_json::to_vec(&(role, &task, call.name.as_str(), &call.arguments))
            .map_err(|_| "invalid call identity")?;
        let origin = record["origin_agent_attempt"]
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or("call origin unavailable")?;
        let effect_attempt = record["effect_agent_attempt"]
            .as_u64()
            .filter(|v| *v >= origin)
            .ok_or("call effect attempt unavailable")?;
        let approval = record["approval_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("call approval unavailable")?;
        let events = run["events"].as_array().ok_or("invalid run events")?;
        for kind in ["tool_started", "tool_complete"] {
            if events
                .iter()
                .filter(|event| {
                    event["kind"] == kind
                        && event["attempt"].as_u64() == Some(effect_attempt)
                        && event["approval_id"] == approval
                        && event["call_id"] == logical
                })
                .count()
                != 1
            {
                return Err("completed call provenance unavailable".into());
            }
        }
        if record["status"] != "completed"
            || (call.name == "patch" && record["result"]["status"] != "applied")
            || (call.name == "exec"
                && (!record["result"]["content"].is_object()
                    || !record["result"]["exit_code"].is_number()))
            || record["hash"] != format!("{:x}", Sha256::digest(identity))
            || record["role"] != role
            || record["task_id"] != task
            || run["agent_attempt"]
                .as_u64()
                .is_none_or(|attempt| effect_attempt > attempt)
        {
            return Err("completed checkpoint call mismatch".into());
        }
    }
    Ok(())
}
// 2026-11-16: 시작 호출이 연결되기 전 CAS 경계를 독립적으로 검증한다.
pub(crate) fn begin_agent(
    db: &Database,
    id: &str,
    expected: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    begin_agent_inner(db, id, expected, None)
}

// 2026-11-16: 재개 선점은 체크포인트와 모든 호출 기록을 같은 트랜잭션에서 검사한다.
pub(crate) fn begin_resume(
    db: &Database,
    id: &str,
    expected: &serde_json::Value,
    model: &str,
    config: &str,
) -> Result<serde_json::Value, String> {
    begin_agent_inner(db, id, expected, Some((model, config)))
}

fn begin_agent_inner(
    db: &Database,
    id: &str,
    expected: &serde_json::Value,
    resume: Option<(&str, &str)>,
) -> Result<serde_json::Value, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let mut run: serde_json::Value = {
        let table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let row = table
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or("run not found")?;
        serde_json::from_str(row.value()).map_err(|_| "invalid run")?
    };
    normalize_run(&mut run)?;
    if &run != expected
        || (run["status"] != "awaiting_implementation"
            && !(resume.is_some()
                && (run["status"] == "interrupted"
                    || run["status"] == "awaiting_approval"
                    || run["status"] == "approved_mcp"
                    || run["status"] == "approved_exec")))
        || run["agent_active"] != false
        || (resume.is_some() && run["resources_unknown"] == true)
        || (!run["pending"].is_null()
            && !(resume.is_some()
                && (run["status"] == "awaiting_approval"
                    || run["status"] == "approved_mcp"
                    || run["status"] == "approved_exec")))
    {
        return Err("run not ready for agent".into());
    }
    if resume.is_none()
        && !run["used_calls"]
            .as_array()
            .ok_or("invalid calls")?
            .is_empty()
    {
        return Err("previous calls require resume".into());
    }
    let project: domain::Project = {
        let table = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
        let row = table
            .get(run["project_id"].as_str().ok_or("invalid project")?)
            .map_err(|e| e.to_string())?
            .ok_or("project not found")?;
        serde_json::from_str(row.value()).map_err(|_| "invalid project")?
    };
    let spec = project.approved.as_ref().ok_or("approval required")?;
    if project.draft_hash != spec.hash
        || run["spec_hash"] != spec.hash
        || run["spec_version"].as_u64() != Some(spec.version)
        || run["context_version"].as_u64() != Some(project.context_version)
        || run["revision"] != spec.base_revision
        || run["scope"] != serde_json::json!(spec.scope)
        || run["agent_configs"] != serde_json::json!(project.agent_configs)
    {
        return Err("approved specification changed".into());
    }
    crate::agent_loop::validate_plan(spec)?;
    if let Some((model, config)) = resume {
        let table = tx.open_table(CHECKPOINTS).map_err(|e| e.to_string())?;
        let value = table
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or("checkpoint unavailable")?;
        let checkpoint: serde_json::Value =
            serde_json::from_str(value.value()).map_err(|_| "invalid saved checkpoint")?;
        let saved: crate::agent_loop::LoopCheckpoint =
            serde_json::from_value(checkpoint.clone()).map_err(|_| "invalid saved checkpoint")?;
        if ![1, 2].contains(&saved.schema_version)
            || saved.revision == 0
            || saved.role_index > 2
            || saved.pending_index > saved.pending_calls.len()
        {
            return Err("invalid saved checkpoint".into());
        }
        if checkpoint["spec_hash"] != run["spec_hash"]
            || checkpoint["spec_version"] != run["spec_version"]
            || checkpoint["model_identity"] != model
            || checkpoint["config_hash"] != config
            || checkpoint_has_inflight(&checkpoint)
        {
            return Err("checkpoint does not match run configuration".into());
        }
        for item in run["evidence"].as_array().ok_or("invalid evidence")? {
            let id = item["verification_id"]
                .as_str()
                .ok_or("invalid verification id")?;
            let (attempt, _) = crate::evidence::context(&run, item)?;
            crate::evidence::resolve(&run, id, attempt)?.ok_or("invalid evidence")?;
        }
        let waiting = if run["status"] == "awaiting_approval"
            || run["status"] == "approved_mcp"
            || run["status"] == "approved_exec"
        {
            let waiting = waiting_checkpoint_call(&run, &saved, spec)?;
            if run["pending"]["tool"] == "mcp" {
                validate_bound_mcp(&tx, &run, &waiting.3, &waiting.4, false)?;
            } else if run["pending"]["tool"] == "exec" {
                validate_bound_exec(
                    &tx,
                    &run,
                    &waiting.3,
                    &waiting.4,
                    &run["pending"]["arguments"],
                    false,
                )?;
            } else {
                validate_bound_patch(
                    &tx,
                    &run,
                    &waiting.3,
                    &waiting.4,
                    &run["pending"]["arguments"],
                    false,
                )?;
            }
            if run["used_calls"]
                .as_array()
                .ok_or("invalid calls")?
                .iter()
                .filter(|id| id.as_str() == Some(waiting.0.as_str()))
                .count()
                != 1
            {
                return Err("pending call mismatch".into());
            }
            Some(waiting)
        } else {
            None
        };
        if waiting.is_none() {
            validate_completed_checkpoint_calls(&tx, &run, &saved, spec, id)?;
        }
        let calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let prefix = format!("{id}:");
        for item in calls.iter().map_err(|e| e.to_string())? {
            let (key, value) = item.map_err(|e| e.to_string())?;
            if key.value().starts_with(&prefix) {
                let record: serde_json::Value =
                    serde_json::from_str(value.value()).map_err(|_| "invalid call record")?;
                if let Some((logical, approval, hash, role, task)) = &waiting {
                    if key.value() == format!("{id}:{logical}") {
                        if record["status"] != "waiting"
                            || record["approval_id"] != *approval
                            || record["hash"] != *hash
                            || record["role"] != *role
                            || record["task_id"] != *task
                        {
                            return Err("pending call mismatch".into());
                        }
                        if run["pending"]["tool"] == "mcp" {
                            validate_mcp_stamp(&tx, &run, &record)?;
                        }
                        continue;
                    }
                }
                if record["status"] != "completed" || record.get("result").is_none() {
                    return Err("previous tool outcome uncertain".into());
                }
            }
        }
        for call_id in run["used_calls"].as_array().ok_or("invalid calls")? {
            let call_id = call_id.as_str().ok_or("invalid call identity")?;
            let key = format!("{id}:{call_id}");
            let value = calls
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .ok_or("previous tool outcome uncertain")?;
            let record: serde_json::Value =
                serde_json::from_str(value.value()).map_err(|_| "invalid call record")?;
            if let Some((logical, approval, hash, role, task)) = &waiting {
                if call_id == logical {
                    if record["status"] != "waiting"
                        || record["approval_id"] != *approval
                        || record["hash"] != *hash
                        || record["role"] != *role
                        || record["task_id"] != *task
                    {
                        return Err("pending call mismatch".into());
                    }
                    continue;
                }
            }
            if record["status"] != "completed" || record.get("result").is_none() {
                return Err("previous tool outcome uncertain".into());
            }
        }
        if waiting.is_none() {
            run["status"] = "awaiting_implementation".into();
        }
    }
    let attempt = run["agent_attempt"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .ok_or("agent attempt overflow")?;
    run["agent_attempt"] = attempt.into();
    run["agent_active"] = true.into();
    let events = run["events"].as_array_mut().ok_or("invalid run events")?;
    let sequence = events.last().map_or(Ok(1), |event| {
        event["sequence"]
            .as_u64()
            .and_then(|v| v.checked_add(1))
            .ok_or("invalid event sequence")
    })?;
    let (kind, message) = if resume.is_some() {
        ("agent_resumed", "저장된 checkpoint에서 재개합니다.")
    } else {
        ("agent_started", "설정된 에이전트 작업을 시작합니다.")
    };
    events.push(serde_json::json!({"sequence":sequence,"run_id":id,"attempt":attempt,"kind":kind,"message":message}));
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(run)
}
// 2026-11-16: 검증 중 stale 상태나 취소가 확인되면 근거를 같은 transaction에 남기지 않는다.
pub(crate) fn record_verification(
    db: &Database,
    id: &str,
    expected_run: &serde_json::Value,
    expected_project: &domain::Project,
    evidence: &domain::VerificationEvidence,
    task_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
        let row = projects
            .get(expected_project.id.as_str())
            .map_err(|e| e.to_string())?
            .ok_or("project not found")?;
        let project: domain::Project =
            serde_json::from_str(row.value()).map_err(|_| "invalid project")?;
        if serde_json::to_value(project).map_err(|_| "invalid project")?
            != serde_json::to_value(expected_project).map_err(|_| "invalid project")?
        {
            return Err("project changed".into());
        }
    }
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let row = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(row.value()).map_err(|_| "invalid run")?
        };
        normalize_run(&mut run)?;
        if &run != expected_run {
            return Err("run changed".into());
        }
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let attempt = run["agent_attempt"]
            .as_u64()
            .ok_or("invalid agent attempt")?;
        if crate::evidence::resolve(&run, &evidence.verification_id, attempt)?.is_some() {
            return Err("duplicate verification id".into());
        }
        let mut row = serde_json::to_value(evidence).map_err(|_| "invalid evidence")?;
        row["agent_attempt"] = attempt.into();
        run["evidence"]
            .as_array_mut()
            .ok_or("invalid evidence")?
            .push(row);
        let context = serde_json::json!({
            "schema_version": 1,
            "spec_version": run["spec_version"].as_u64().ok_or("invalid spec version")?,
            "context_version": run["context_version"].as_u64().ok_or("invalid context version")?,
            "agent_attempt": run["agent_attempt"].as_u64().ok_or("invalid agent attempt")?,
            "task_id": task_id,
        });
        let contexts = run
            .as_object_mut()
            .ok_or("invalid run")?
            .entry("evidence_contexts_by_attempt")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or("invalid evidence contexts")?;
        let contexts = contexts
            .entry(attempt.to_string())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or("invalid evidence contexts")?;
        if contexts.contains_key(&evidence.verification_id) {
            return Err("duplicate verification id".into());
        }
        contexts.insert(evidence.verification_id.clone(), context);
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}
#[cfg(all(test, windows))]
struct DiffTestHook {
    run_id: String,
    reached: std::sync::mpsc::Sender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}
#[cfg(all(test, windows))]
static TEST_DIFF_HOOK: std::sync::Mutex<Option<DiffTestHook>> = std::sync::Mutex::new(None);
const CHECKPOINTS: TableDefinition<&str, &str> = TableDefinition::new("checkpoints");
const CALLS: TableDefinition<&str, &str> = TableDefinition::new("calls");
const MOCK_CALLS: TableDefinition<&str, u64> = TableDefinition::new("mock_calls");
// 2026-11-16: 같은 mock 입력의 재개는 이전 script 인덱스를 재사용한다.
pub(crate) fn mock_index(db: &Database, run: &str, fingerprint: &str) -> Result<u64, String> {
    let key = format!("{run}:request:{fingerprint}");
    let cursor = format!("{run}:cursor");
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let index = {
        let mut table = tx.open_table(MOCK_CALLS).map_err(|e| e.to_string())?;
        let found = table
            .get(key.as_str())
            .map_err(|e| e.to_string())?
            .map(|v| v.value());
        if let Some(index) = found {
            index
        } else {
            let index = table
                .get(cursor.as_str())
                .map_err(|e| e.to_string())?
                .map_or(0, |v| v.value());
            let next = index.checked_add(1).ok_or("mock cursor exhausted")?;
            table
                .insert(key.as_str(), index)
                .map_err(|e| e.to_string())?;
            table
                .insert(cursor.as_str(), next)
                .map_err(|e| e.to_string())?;
            index
        }
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(index)
}
const MAX_DURABLE_RECORD_BYTES: usize = 2 * 1024 * 1024;

fn append_event(
    run: &mut serde_json::Value,
    id: &str,
    kind: &str,
    message: &str,
) -> Result<(), String> {
    let attempt = run["agent_attempt"]
        .as_u64()
        .filter(|attempt| *attempt > 0)
        .unwrap_or(1);
    let events = run["events"].as_array_mut().ok_or("invalid run events")?;
    let sequence = events.last().map_or(Ok(1), |event| {
        event["sequence"]
            .as_u64()
            .and_then(|v| v.checked_add(1))
            .ok_or("invalid event sequence")
    })?;
    events.push(serde_json::json!({"sequence":sequence,"run_id":id,"attempt":attempt,"kind":kind,"message":message}));
    Ok(())
}

// 2026-11-15: 완료 기록을 먼저 반영한 후 외부 작업 결과가 불명확한 실행만 중단 상태로 표시한다.
pub(crate) fn initialize(db: &Database) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let mut interrupted_calls = Vec::new();
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut changes = Vec::new();
        for item in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = item.map_err(|e| e.to_string())?;
            let mut run: serde_json::Value =
                serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
            let status = run["status"]
                .as_str()
                .ok_or("invalid run status")?
                .to_owned();
            let mut modified = false;
            if run.get("resources_unknown").is_none() && status == "uncertain" {
                run["resources_unknown"] = true.into();
                modified = true;
            }
            if ["executing", "cancelling"].contains(&status.as_str())
                && run["pending"]["tool"] == "patch"
            {
                if let Some(call_id) = run["pending"]["call_id"].as_str() {
                    interrupted_calls.push((
                        key.value().to_owned(),
                        call_id.to_owned(),
                        run["pending"]["id"].clone(),
                    ));
                }
            }
            if ["preparing", "cancelling", "executing", "generating"].contains(&status.as_str()) {
                if status != "generating" {
                    run["resources_unknown"] = true.into();
                }
                run["status"] = "uncertain".into();
                append_event(
                    &mut run,
                    key.value(),
                    "uncertain",
                    "엔진 중단으로 외부 작업 결과를 확인해야 합니다.",
                )?;
                modified = true;
            }
            if run["agent_active"].as_bool() == Some(true) {
                run["agent_active"] = false.into();
                let current = run["status"].as_str().ok_or("invalid run status")?;
                if ![
                    "awaiting_approval",
                    "approved_exec",
                    "approved_mcp",
                    "uncertain",
                ]
                .contains(&current)
                {
                    run["status"] = "interrupted".into();
                }
                append_event(
                    &mut run,
                    key.value(),
                    "interrupted",
                    "에이전트가 중단되어 자동 재생하지 않았습니다.",
                )?;
                modified = true;
            }
            if modified {
                changes.push((
                    key.value().to_owned(),
                    serde_json::to_string(&run).map_err(|e| e.to_string())?,
                ));
            }
        }
        for (id, json) in changes {
            table
                .insert(id.as_str(), json.as_str())
                .map_err(|e| e.to_string())?;
        }
    }
    // 2026-11-16: 중단된 patch 효과는 재실행하지 않도록 Run과 호출 결과를 함께 불명확 상태로 둔다.
    if !interrupted_calls.is_empty() {
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        for (run_id, call_id, approval_id) in interrupted_calls {
            let key = format!("{run_id}:{call_id}");
            let existing = {
                calls
                    .get(key.as_str())
                    .map_err(|e| e.to_string())?
                    .map(|value| value.value().to_owned())
            };
            let mut call: serde_json::Value =
                serde_json::from_str(&existing.ok_or("interrupted call not found")?)
                    .map_err(|e| e.to_string())?;
            if call["status"] == "waiting" && call["approval_id"] == approval_id {
                call["status"] = "uncertain".into();
                calls
                    .insert(key.as_str(), call.to_string().as_str())
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    tx.open_table(CHECKPOINTS).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}
#[cfg(test)]
pub(crate) static TEST_GIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) static TEST_FAIL_AFTER_GIT: std::sync::Mutex<
    Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static TEST_GIT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE_GIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static MAX_GIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn serialized_worktree(
    gate: &std::sync::Mutex<()>,
    repo: &std::path::Path,
    revision: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        TEST_GIT_CALLS.fetch_add(1, Ordering::SeqCst);
        let current = ACTIVE_GIT.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_GIT.fetch_max(current, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    let result = crate::worktree::create(repo, revision, target);
    #[cfg(test)]
    ACTIVE_GIT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    result
}
// 2026-11-15: 승인 검증과 preparing 저장을 같은 쓰기 트랜잭션에 묶고 Git은 별도 worker에서 직렬화한다.
pub(crate) fn start(
    db: std::sync::Arc<Database>,
    root: std::path::PathBuf,
    completions: std::sync::Arc<crate::completion::CompletionManager>,
    git_gate: std::sync::Arc<std::sync::Mutex<()>>,
    slots: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    project_id: &str,
    expected: &str,
) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    slots
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| "run capacity reached")?;
    let attempt = (|| {
        let tx = db.begin_write().map_err(|e| e.to_string())?;
        let project: domain::Project = {
            let table = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
            let value = table
                .get(project_id)
                .map_err(|e| e.to_string())?
                .ok_or("project not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        let spec = project.approved.as_ref().ok_or("approval required")?;
        if spec.hash != expected || project.draft_hash != expected {
            return Err("approved specification changed".into());
        }
        let model = project
            .model_settings
            .as_ref()
            .ok_or("model not configured")?;
        if !["mock", "openai"].contains(&model.provider.as_str()) {
            return Err("selected local model is unavailable".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let target = root.join("magi-runs").join(&id);
        let run = serde_json::json!({
            "id": id, "project_id": project_id, "spec_hash": expected,
            "revision": spec.base_revision, "status": "preparing",
            "worktree": target.to_string_lossy(),
            "events": [{"sequence": 1, "run_id": id, "attempt": 1,
                "kind": "preparing", "message": "승인된 Git revision에서 작업 공간을 준비합니다."}],
            "spec_version": spec.version, "scope": spec.scope,
            "pending": null, "report": null, "context_version": project.context_version,
            "agent_configs": project.agent_configs, "autonomy_mode": project.autonomy_mode,
            "model_provider": model.provider, "model_capabilities": model.capabilities,
            "agent_active": false, "agent_attempt": 0, "resources_unknown": false,
            "used_calls": [], "evidence": []
        });
        {
            let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
            let json = run.to_string();
            table
                .insert(id.as_str(), json.as_str())
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        let worker_id = id.clone();
        let pid = project_id.to_owned();
        let repo = std::path::PathBuf::from(project.path);
        let revision = spec.base_revision.clone();
        let worker_slots = std::sync::Arc::clone(&slots);
        std::thread::spawn(move || {
            let result = serialized_worktree(&git_gate, &repo, &revision, &target);
            #[cfg(test)]
            if let Some(flag) = TEST_FAIL_AFTER_GIT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                flag.store(true, Ordering::SeqCst);
            }
            completions.register(&db, &worker_id, &pid, result);
            worker_slots.fetch_sub(1, Ordering::AcqRel);
        });
        Ok(id)
    })();
    if attempt.is_err() {
        slots.fetch_sub(1, Ordering::AcqRel);
    }
    attempt
}
// 2026-11-15: Git 작업 중 취소 요청은 종료 확인이 아니므로 cancelling으로만 기록한다.
// 2026-11-16: 생성 시점의 초안과 문맥을 같은 쓰기 트랜잭션에서 확인한다.
pub(crate) fn begin_generation(
    db: &Database,
    id: &str,
    project_id: &str,
    expected: &str,
    context_version: u64,
) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
        let value = projects
            .get(project_id)
            .map_err(|e| e.to_string())?
            .ok_or("project not found")?;
        let project: domain::Project =
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
        if project.draft_hash != expected || project.context_version != context_version {
            return Err("draft or context changed".into());
        }
    }
    let run = serde_json::json!({"id":id,"project_id":project_id,"spec_hash":expected,"revision":"","status":"generating","worktree":"","events":[{"sequence":1,"run_id":id,"attempt":1,"kind":"generating","message":"명세 생성 요청을 시작했습니다."}],"spec_version":0,"scope":[],"pending":null,"report":null,"context_version":context_version,"agent_configs":[],"autonomy_mode":"","model_provider":"","model_capabilities":null,"agent_active":false,"agent_attempt":0,"resources_unknown":false,"used_calls":[],"evidence":[]});
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let json = run.to_string();
        table.insert(id, json.as_str()).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}
pub(crate) fn cancel(db: &Database, id: &str) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        let next = match run["status"].as_str() {
            Some("executing") => Some(("cancelling", "외부 작업 취소를 요청했습니다.")),
            Some("awaiting_approval" | "approved_mcp" | "approved_exec") => {
                Some(("cancelled", "대기 중인 승인을 취소했습니다."))
            }
            Some("generating") => Some(("cancelled", "작업을 취소했습니다.")),
            Some("preparing") => Some(("cancelling", "취소를 요청했습니다.")),
            Some("awaiting_implementation") => Some(("cancelled", "작업을 취소했습니다.")),
            _ => None,
        };
        if let Some((state, message)) = next {
            if ["awaiting_approval", "approved_mcp", "approved_exec"]
                .contains(&run["status"].as_str().unwrap_or(""))
            {
                run["pending"] = serde_json::Value::Null;
            }
            run["status"] = state.into();
            let events = run["events"].as_array_mut().ok_or("invalid run events")?;
            let seq = events.last().map_or(Ok(1), |event| {
                event["sequence"]
                    .as_u64()
                    .and_then(|v| v.checked_add(1))
                    .ok_or("invalid event sequence")
            })?;
            events.push(serde_json::json!({"sequence":seq,"run_id":id,"attempt":1,"kind":state,"message":message}));
            let json = run.to_string();
            table.insert(id, json.as_str()).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}
// 2026-11-15: 이전 저장 형식의 선택 필드는 응답에서 기본값을 채우되 원본 필드는 보존한다.
fn normalize_run(run: &mut serde_json::Value) -> Result<(), String> {
    let map = run.as_object_mut().ok_or("invalid run")?;
    for (key, value) in [
        ("spec_version", serde_json::json!(0)),
        ("scope", serde_json::json!([])),
        ("pending", serde_json::Value::Null),
        ("report", serde_json::Value::Null),
        ("context_version", serde_json::json!(0)),
        ("agent_configs", serde_json::json!([])),
        ("autonomy_mode", serde_json::json!("")),
        ("model_provider", serde_json::json!("")),
        ("model_capabilities", serde_json::Value::Null),
        ("agent_active", serde_json::json!(false)),
        ("agent_attempt", serde_json::json!(0)),
        ("resources_unknown", serde_json::json!(false)),
        ("used_calls", serde_json::json!([])),
        ("evidence", serde_json::json!([])),
    ] {
        map.entry(key.to_owned()).or_insert(value);
    }
    Ok(())
}
pub(crate) fn list(
    db: &Database,
    project_id: &str,
    offset: usize,
    limit: usize,
) -> Result<serde_json::Value, String> {
    if !(1..=100).contains(&limit) || offset.checked_add(limit).is_none() {
        return Err("invalid run page".into());
    }
    let tx = db.begin_read().map_err(|e| e.to_string())?;
    let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
    if projects
        .get(project_id)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("project not found".into());
    }
    let table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
    let mut total = 0usize;
    let mut page = Vec::new();
    for item in table.iter().map_err(|e| e.to_string())? {
        let (_, value) = item.map_err(|e| e.to_string())?;
        let mut run: serde_json::Value =
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
        normalize_run(&mut run)?;
        if run["project_id"].as_str() != Some(project_id) {
            continue;
        }
        if total >= offset && page.len() < limit {
            page.push(serde_json::json!({
                "id": run["id"], "project_id": run["project_id"], "status": run["status"],
                "spec_hash": run["spec_hash"], "spec_version": run["spec_version"],
                "agent_active": run["agent_active"], "resources_unknown": run["resources_unknown"],
                "has_pending_approval": !run["pending"].is_null()
            }));
        }
        total += 1;
    }
    Ok(serde_json::json!({"runs":page,"total":total,"offset":offset,"limit":limit}))
}
pub(crate) fn get(db: &Database, id: &str) -> Result<serde_json::Value, String> {
    let tx = db.begin_read().map_err(|e| e.to_string())?;
    let table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
    let value = table
        .get(id)
        .map_err(|e| e.to_string())?
        .ok_or("run not found")?;
    let mut run = serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
    normalize_run(&mut run)?;
    Ok(run)
}
// 2026-11-15: 결과와 중복 방지 marker를 하나의 트랜잭션에 저장한다.
pub(crate) fn finish_worktree(
    db: &Database,
    id: &str,
    result: &Result<(), String>,
    completion_id: &str,
) -> Result<bool, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let previous = {
        let table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        let value = table
            .get(completion_id)
            .map_err(|e| e.to_string())?
            .map(|value| value.value());
        value
    };
    if let Some(value) = previous {
        // 2026-11-16: 이전 commit의 성공 여부가 불명확한 경우에도 marker를 다시 내구화한 뒤 반환한다.
        {
            let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
            table
                .insert(completion_id, value)
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(value != 0);
    }
    let applied = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        let transition = match run.get("status").and_then(|v| v.as_str()) {
            Some("cancelling") => Some((
                "uncertain",
                "uncertain",
                "취소 중 작업 공간 변경 여부를 확인해야 합니다.".to_owned(),
            )),
            Some("preparing") => match result {
                Ok(()) => Some((
                    "awaiting_implementation",
                    "worktree_ready",
                    "작업 공간이 준비되었습니다. 구현과 검증은 아직 수행되지 않았습니다."
                        .to_owned(),
                )),
                Err(message) => Some(("uncertain", "uncertain", message.clone())),
            },
            Some(_) => None,
            None => return Err("invalid run status".into()),
        };
        if let Some((status, kind, message)) = transition {
            run["status"] = status.into();
            let events = run
                .get_mut("events")
                .and_then(|v| v.as_array_mut())
                .ok_or("invalid run events")?;
            let sequence = events.last().map_or(Ok(1), |event| {
                event
                    .get("sequence")
                    .and_then(|n| n.as_u64())
                    .and_then(|n| n.checked_add(1))
                    .ok_or("invalid event sequence")
            })?;
            events.push(serde_json::json!({"sequence":sequence,"run_id":id,"attempt":1,"kind":kind,"message":message}));
            let encoded = serde_json::to_string(&run).map_err(|e| e.to_string())?;
            table
                .insert(id, encoded.as_str())
                .map_err(|e| e.to_string())?;
            true
        } else {
            false
        }
    };
    {
        let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        table
            .insert(completion_id, u8::from(applied))
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(applied)
}
// 2026-11-16: 평가 보고서와 프로젝트 초안 및 완료 marker를 같은 트랜잭션에 기록한다.
pub(crate) fn finish_generation(
    db: &Database,
    id: &str,
    result: Result<provider::sdd::SpecResult, String>,
    completion_id: &str,
) -> Result<bool, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let previous = {
        let table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        let value = table
            .get(completion_id)
            .map_err(|e| e.to_string())?
            .map(|v| v.value());
        value
    };
    if let Some(value) = previous {
        {
            let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
            table
                .insert(completion_id, value)
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(value != 0);
    }
    let mut run: serde_json::Value = {
        let table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let value = table
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or("run not found")?;
        serde_json::from_str(value.value()).map_err(|e| e.to_string())?
    };
    if run["status"].as_str() != Some("generating") {
        {
            let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
            table.insert(completion_id, 0).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(false);
    }
    let mut applied = true;
    match result {
        Ok(report) => {
            let draft = report.draft.clone();
            run["report"] = serde_json::to_value(report).map_err(|e| e.to_string())?;
            let project_id = run["project_id"]
                .as_str()
                .ok_or("invalid run project")?
                .to_owned();
            let mut table = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
            let mut project: domain::Project = {
                let value = table
                    .get(project_id.as_str())
                    .map_err(|e| e.to_string())?
                    .ok_or("project not found")?;
                serde_json::from_str(value.value()).map_err(|e| e.to_string())?
            };
            if run["spec_hash"].as_str() != Some(project.draft_hash.as_str())
                || run["context_version"].as_u64() != Some(project.context_version)
            {
                applied = false;
                run["status"] = "stale".into();
                append_event(
                    &mut run,
                    id,
                    "stale",
                    "생성 중 초안 또는 입력이 변경되어 결과를 적용하지 않았습니다.",
                )?;
            } else {
                project.draft_hash = crate::draft_hash(&draft)?;
                project.assessment = Some(domain::AssessmentRef {
                    draft_hash: project.draft_hash.clone(),
                    context_version: project.context_version,
                });
                project.draft = draft;
                project.approved = None;
                let json = serde_json::to_string(&project).map_err(|e| e.to_string())?;
                table
                    .insert(project_id.as_str(), json.as_str())
                    .map_err(|e| e.to_string())?;
                run["status"] = "generated".into();
                append_event(
                    &mut run,
                    id,
                    "generated",
                    "명세 초안을 생성했습니다. 사용자 검토가 필요합니다.",
                )?;
            }
        }
        Err(_) => {
            run["status"] = "failed".into();
            append_event(&mut run, id, "failed", "명세 생성 요청이 실패했습니다.")?;
        }
    }
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let json = serde_json::to_string(&run).map_err(|e| e.to_string())?;
        table.insert(id, json.as_str()).map_err(|e| e.to_string())?;
    }
    {
        let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        table
            .insert(completion_id, u8::from(applied))
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(applied)
}
// 2026-11-16: 재개 연결 전에도 원본 저장 형식과 CAS 계약을 보존한다.
pub(crate) fn save_checkpoint(
    db: &Database,
    id: &str,
    expected: u64,
    mut value: serde_json::Value,
) -> Result<u64, String> {
    let next = expected
        .checked_add(1)
        .ok_or("checkpoint revision overflow")?;
    let object = value.as_object_mut().ok_or("invalid checkpoint")?;
    object.insert("revision".into(), serde_json::json!(next));
    let json = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    if json.len() > MAX_DURABLE_RECORD_BYTES {
        return Err("checkpoint too large".into());
    }
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(CHECKPOINTS).map_err(|e| e.to_string())?;
        let current = {
            let existing = table.get(id).map_err(|e| e.to_string())?;
            match existing {
                Some(v) => serde_json::from_str::<serde_json::Value>(v.value())
                    .map_err(|e| e.to_string())?["revision"]
                    .as_u64()
                    .ok_or("invalid checkpoint revision")?,
                None => 0,
            }
        };
        if current != expected {
            return Err("checkpoint revision conflict".into());
        }
        table.insert(id, json.as_str()).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(next)
}
pub(crate) fn load_checkpoint(
    db: &Database,
    id: &str,
) -> Result<Option<(u64, serde_json::Value)>, String> {
    let tx = db.begin_read().map_err(|e| e.to_string())?;
    let table = tx.open_table(CHECKPOINTS).map_err(|e| e.to_string())?;
    let value = table.get(id).map_err(|e| e.to_string())?;
    match value {
        None => Ok(None),
        Some(v) => {
            let value: serde_json::Value =
                serde_json::from_str(v.value()).map_err(|e| e.to_string())?;
            let rev = value["revision"]
                .as_u64()
                .ok_or("invalid checkpoint revision")?;
            Ok(Some((rev, value)))
        }
    }
}

pub(crate) fn claim_call(
    db: &Database,
    run: &str,
    id: &str,
    hash: &str,
) -> Result<serde_json::Value, String> {
    if run.len() > 128 || id.is_empty() || id.len() > 1024 || hash.len() != 64 {
        return Err("invalid call identity".into());
    }
    let key = format!("{run}:{id}");
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let result;
    {
        let mut table = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let existing = {
            table
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .map(|v| v.value().to_owned())
        };
        result = if let Some(json) = existing {
            let mut record: serde_json::Value =
                serde_json::from_str(&json).map_err(|e| e.to_string())?;
            if record["hash"] != hash {
                return Err("call identity conflict".into());
            }
            record["fresh"] = serde_json::json!(false);
            record
        } else {
            let record = serde_json::json!({"status":"planned","hash":hash});
            let json = serde_json::to_string(&record).map_err(|e| e.to_string())?;
            table
                .insert(key.as_str(), json.as_str())
                .map_err(|e| e.to_string())?;
            let mut visible = record;
            visible["fresh"] = serde_json::json!(true);
            visible
        };
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}
pub(crate) fn complete_call(
    db: &Database,
    run: &str,
    id: &str,
    result: serde_json::Value,
) -> Result<(), String> {
    let key = format!("{run}:{id}");
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let mut record: serde_json::Value = {
            let v = table
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .ok_or("call not found")?;
            serde_json::from_str(v.value()).map_err(|e| e.to_string())?
        };
        if record["status"] != "planned" {
            return Err("call not pending".into());
        }
        record["status"] = serde_json::json!("completed");
        record["result"] = result;
        let json = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        if json.len() > MAX_DURABLE_RECORD_BYTES {
            return Err("call result too large".into());
        }
        table
            .insert(key.as_str(), json.as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "checkpoint_storage_tests.rs"]
mod checkpoint_storage_tests;
#[cfg(test)]
#[path = "durable_patch_tests.rs"]
mod durable_patch_tests;

#[cfg(test)]
// 2026-11-16: 병렬 테스트의 서로 다른 실행이 장애 주입 등록을 덮거나 해제하지 않게 한다.
pub(crate) static TEST_FAIL_AFTER_PATCH: std::sync::LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
#[cfg(test)]
pub(crate) static TEST_FAIL_AFTER_TRUNCATE: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
type PatchReadBarrier = (
    String,
    std::sync::Arc<std::sync::Barrier>,
    std::sync::Arc<std::sync::Barrier>,
);
#[cfg(test)]
static TEST_PAUSE_AFTER_PATCH_READ: std::sync::Mutex<Option<PatchReadBarrier>> =
    std::sync::Mutex::new(None);
// 2026-11-16: 논리 호출과 승인 요청을 같은 트랜잭션에 기록해 중단 후 파일 효과를 추정해 재실행하지 않는다.
pub(crate) fn submit_bound_patch(
    db: &Database,
    id: &str,
    role: &str,
    task_id: &str,
    call_id: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    if id.len() > 128 || call_id.is_empty() || call_id.len() > 1024 {
        return Err("invalid call identity".into());
    }
    let path = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("path required")?;
    let content = arguments
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("content required")?;
    if content.len() > 1024 * 1024 {
        return Err("content too large".into());
    }
    let identity =
        serde_json::to_vec(&(role, task_id, "patch", &arguments)).map_err(|e| e.to_string())?;
    let hash = format!("{:x}", Sha256::digest(identity));
    let key = format!("{id}:{call_id}");
    let snapshot = get(db, id)?;
    let already_exists = {
        let tx = db.begin_read().map_err(|e| e.to_string())?;
        match tx.open_table(CALLS) {
            Ok(table) => table
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.to_string()),
        }
    };
    let before_hash = if already_exists {
        None
    } else {
        if snapshot["status"] != "awaiting_implementation" || !snapshot["pending"].is_null() {
            return Err("run not ready for call".into());
        }
        let run: PatchRun = serde_json::from_value(snapshot.clone()).map_err(|e| e.to_string())?;
        let (path, missing) = target_path(&run, path)?;
        Some(if missing {
            "missing".into()
        } else {
            file_hash(&run, &path)?
        })
    };
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let response = {
        let mut runs = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = runs
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        // 2026-11-16: 이전 Run 형식의 선택 필드는 기존 조회와 동일한 기본값으로 처리한다.
        normalize_run(&mut run)?;
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let existing = {
            calls
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .map(|value| value.value().to_owned())
        };
        if let Some(existing) = existing {
            let record: serde_json::Value =
                serde_json::from_str(&existing).map_err(|e| e.to_string())?;
            if record["hash"] != hash {
                return Err("call identity conflict".into());
            }
            // 2026-11-16: 중단된 호출은 활동이 끝나도 같은 identity와 문맥에서 결과 불명을 알려준다.
            let require_active = !(record["status"] == "uncertain" && run["status"] == "uncertain");
            validate_bound_patch(&tx, &run, role, task_id, &arguments, require_active)?;
            match record["status"].as_str() {
                Some("completed") => {
                    serde_json::json!({"status":"completed","result":record["result"]})
                }
                Some("waiting")
                    if run["status"] == "awaiting_approval"
                        && run["pending"]["call_id"].as_str() == Some(call_id)
                        && run["pending"]["id"] == record["approval_id"] =>
                {
                    serde_json::json!({"status":"waiting","approval_id":record["approval_id"]})
                }
                Some("waiting")
                    if ["executing", "cancelling", "uncertain"]
                        .contains(&run["status"].as_str().unwrap_or("")) =>
                {
                    return Err("previous tool outcome uncertain".into())
                }
                Some("waiting") => return Err("approval is no longer pending".into()),
                Some("uncertain") => return Err("previous tool outcome uncertain".into()),
                Some("planned") => return Err("call is not ready for approval".into()),
                _ => return Err("invalid call state".into()),
            }
        } else {
            validate_bound_patch(&tx, &run, role, task_id, &arguments, true)?;
            if run["status"] != "awaiting_implementation"
                || !run["pending"].is_null()
                || [
                    "project_id",
                    "spec_hash",
                    "spec_version",
                    "context_version",
                    "worktree",
                    "scope",
                    "agent_configs",
                ]
                .iter()
                .any(|k| run[*k] != snapshot[*k])
            {
                return Err("run not ready for call".into());
            }
            let before = before_hash.ok_or("call changed during request")?;
            if run["used_calls"]
                .as_array()
                .is_none_or(|items| items.iter().any(|v| v.as_str() == Some(call_id)))
            {
                return Err("run not ready for call".into());
            }
            let approval = uuid::Uuid::new_v4().to_string();
            let record = serde_json::json!({"status":"waiting","hash":hash,"approval_id":approval,"role":role,"task_id":task_id,"origin_agent_attempt":run["agent_attempt"]});
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .map_err(|e| e.to_string())?;
            run["used_calls"]
                .as_array_mut()
                .unwrap()
                .push(call_id.into());
            run["pending"] = serde_json::json!({"id":approval,"tool":"patch","arguments":arguments,"before_hash":before,"call_id":call_id,"authorized":false});
            run["status"] = "awaiting_approval".into();
            append_event(
                &mut run,
                id,
                "approval_required",
                "도구 요청에 사용자 승인이 필요합니다.",
            )?;
            runs.insert(id, run.to_string().as_str())
                .map_err(|e| e.to_string())?;
            serde_json::json!({"status":"waiting","approval_id":approval})
        }
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(response)
}

// 2026-11-16: 승인본·문맥·역할과 작업 소유권을 DB 쓰기 트랜잭션의 현재 상태로 검증한다.
fn validate_bound_patch(
    tx: &redb::WriteTransaction,
    run: &serde_json::Value,
    role: &str,
    task_id: &str,
    arguments: &serde_json::Value,
    require_active: bool,
) -> Result<(), String> {
    if require_active && run["agent_active"] != true {
        return Err("run not ready for call".into());
    }
    let configs = run["agent_configs"]
        .as_array()
        .ok_or("role not configured")?;
    let config = configs
        .iter()
        .find(|v| v["role"].as_str() == Some(role))
        .ok_or("role not configured")?;
    if config["tools"]
        .as_array()
        .is_none_or(|tools| !tools.iter().any(|v| v == "patch"))
    {
        return Err("tool not allowed for role".into());
    }
    let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
    let project_id = run["project_id"].as_str().ok_or("invalid run project")?;
    let value = projects
        .get(project_id)
        .map_err(|e| e.to_string())?
        .ok_or("project not found")?;
    let project: domain::Project =
        serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
    let spec = project
        .approved
        .as_ref()
        .ok_or("approved specification changed")?;
    if run["spec_hash"].as_str() != Some(spec.hash.as_str())
        || run["spec_version"].as_u64() != Some(spec.version)
        || run["context_version"].as_u64() != Some(project.context_version)
        || project.draft_hash != spec.hash
        || run["scope"] != serde_json::to_value(&spec.scope).map_err(|e| e.to_string())?
    {
        return Err("approved specification changed".into());
    }
    let task = spec
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .ok_or("task not found")?;
    let path = arguments["path"]
        .as_str()
        .ok_or("path required")?
        .replace('\\', "/")
        .to_lowercase();
    if !task.owned_paths.iter().any(|owned| {
        let owned = owned
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        !owned.is_empty() && (path == owned || path.starts_with(&(owned + "/")))
    }) {
        return Err("path outside task ownership".into());
    }
    Ok(())
}
// 2026-11-16: 파일 읽기와 승인 대기 전환 사이의 취소·새 요청은 같은 트랜잭션에서 다시 확인한다.
pub(crate) fn submit_patch(
    db: &Database,
    id: &str,
    tool: String,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if tool != "patch" {
        return Err("unsupported tool".into());
    }
    let path = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("path required")?;
    let content = arguments
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("content required")?;
    if content.len() > 1024 * 1024 {
        return Err("content too large".into());
    }
    let snapshot = get(db, id)?;
    if snapshot["status"] != "awaiting_implementation" || !snapshot["pending"].is_null() {
        return Err("run not ready for tool approval".into());
    }
    let run: PatchRun = serde_json::from_value(snapshot.clone()).map_err(|e| e.to_string())?;
    let (full_path, missing) = target_path(&run, path)?;
    let before_hash = if missing {
        "missing".to_owned()
    } else {
        file_hash(&run, &full_path)?
    };
    #[cfg(test)]
    if let Some((_, ready, release)) = TEST_PAUSE_AFTER_PATCH_READ
        .lock()
        .unwrap()
        .clone()
        .filter(|(run_id, _, _)| run_id == id)
    {
        ready.wait();
        release.wait();
    }
    let approval_id = uuid::Uuid::new_v4().to_string();
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut current: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if current["status"] != "awaiting_implementation"
            || !current["pending"].is_null()
            || [
                "project_id",
                "spec_hash",
                "spec_version",
                "worktree",
                "scope",
            ]
            .iter()
            .any(|key| current[*key] != snapshot[*key])
        {
            return Err("run not ready for tool approval".into());
        }
        current["pending"] = serde_json::json!({"id":approval_id,"tool":tool,"arguments":arguments,"before_hash":before_hash,"call_id":null,"authorized":false});
        current["status"] = "awaiting_approval".into();
        append_event(
            &mut current,
            id,
            "approval_required",
            "파일 변경 승인이 필요합니다.",
        )?;
        table
            .insert(id, current.to_string().as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"approval_id":approval_id}))
}

// 2026-11-16: 현재 승인본과 요청 ID를 원자적으로 확인하고 executing을 내구 기록한 후에만 파일을 변경한다.
pub(crate) fn approve_patch(
    db: &Database,
    id: &str,
    approval: &str,
    approved: bool,
) -> Result<serde_json::Value, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let (run, pending) = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut current: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if current["status"] != "awaiting_approval" || current["pending"].is_null() {
            return Err("no pending approval".into());
        }
        let pending = current["pending"].clone();
        if pending["id"].as_str() != Some(approval) {
            return Err("approval does not match pending request".into());
        }
        if pending["tool"] != "patch"
            && pending["tool"] != "mcp"
            && !(pending["tool"] == "exec" && pending["call_id"].is_string())
        {
            return Err("unsupported tool".into());
        }
        if approved {
            let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
            let project_id = current["project_id"]
                .as_str()
                .ok_or("invalid run project")?;
            let value = projects
                .get(project_id)
                .map_err(|e| e.to_string())?
                .ok_or("project not found")?;
            let project: domain::Project =
                serde_json::from_str(value.value()).map_err(|e| e.to_string())?;
            let spec = project
                .approved
                .as_ref()
                .ok_or("approved specification changed")?;
            if current["spec_hash"].as_str() != Some(spec.hash.as_str())
                || current["spec_version"].as_u64() != Some(spec.version)
                || project.draft_hash != spec.hash
                || current["scope"]
                    != serde_json::to_value(&spec.scope).map_err(|e| e.to_string())?
            {
                return Err("approved specification changed".into());
            }
            if let Some(call_id) = pending["call_id"].as_str() {
                use sha2::{Digest, Sha256};
                let key = format!("{id}:{call_id}");
                let record: serde_json::Value = {
                    let calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
                    let value = calls
                        .get(key.as_str())
                        .map_err(|e| e.to_string())?
                        .ok_or("call not found")?;
                    serde_json::from_str(value.value()).map_err(|e| e.to_string())?
                };
                let role = record["role"].as_str().ok_or("invalid call identity")?;
                let task = record["task_id"].as_str().ok_or("invalid call identity")?;
                let tool = pending["tool"].as_str().ok_or("invalid tool")?;
                let identity = serde_json::to_vec(&(role, task, tool, &pending["arguments"]))
                    .map_err(|e| e.to_string())?;
                if record["status"] != "waiting"
                    || record["approval_id"].as_str() != Some(approval)
                    || record["hash"] != format!("{:x}", Sha256::digest(identity))
                {
                    return Err("call identity conflict".into());
                }
                drop(value);
                drop(projects);
                if tool == "mcp" {
                    validate_bound_mcp(&tx, &current, role, task, true)?;
                    validate_mcp_stamp(&tx, &current, &record)?;
                } else if tool == "exec" {
                    validate_bound_exec(&tx, &current, role, task, &pending["arguments"], true)?;
                } else {
                    validate_bound_patch(&tx, &current, role, task, &pending["arguments"], true)?;
                    let mut claimed = record;
                    claimed["effect_agent_attempt"] = current["agent_attempt"].clone();
                    let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
                    calls
                        .insert(key.as_str(), claimed.to_string().as_str())
                        .map_err(|e| e.to_string())?;
                }
            }
            if pending["tool"] == "mcp" || pending["tool"] == "exec" {
                current["pending"]["authorized"] = true.into();
                current["status"] = if pending["tool"] == "mcp" {
                    "approved_mcp"
                } else {
                    "approved_exec"
                }
                .into();
                append_event(
                    &mut current,
                    id,
                    "tool_approved",
                    if pending["tool"] == "mcp" {
                        "MCP 요청을 승인했습니다."
                    } else {
                        "명령 실행을 승인했습니다."
                    },
                )?;
            } else {
                current["status"] = "executing".into();
                append_event(
                    &mut current,
                    id,
                    "tool_started",
                    "승인된 파일 변경을 시작합니다.",
                )?;
                if let Some(call_id) = pending["call_id"].as_str() {
                    let event = current["events"]
                        .as_array_mut()
                        .and_then(|events| events.last_mut())
                        .ok_or("invalid run events")?;
                    event["approval_id"] = approval.into();
                    event["call_id"] = call_id.into();
                }
            }
        } else {
            current["pending"] = serde_json::Value::Null;
            current["status"] = "awaiting_implementation".into();
            append_event(
                &mut current,
                id,
                "tool_denied",
                "도구 요청이 거절되었습니다.",
            )?;
        }
        table
            .insert(id, current.to_string().as_str())
            .map_err(|e| e.to_string())?;
        (current, pending)
    };
    tx.commit().map_err(|e| e.to_string())?;
    if !approved {
        return Ok(serde_json::json!({"status":"denied"}));
    }
    if pending["tool"] == "mcp" || pending["tool"] == "exec" {
        return Ok(serde_json::json!({"status":run["status"]}));
    }
    let run: PatchRun = serde_json::from_value(run).map_err(|e| e.to_string())?;
    let result = (|| {
        let path = pending["arguments"]["path"]
            .as_str()
            .ok_or("invalid path")?;
        let content = pending["arguments"]["content"]
            .as_str()
            .ok_or("invalid content")?;
        let before = pending["before_hash"].as_str().ok_or("invalid file hash")?;
        let (path, missing) = target_path(&run, path)?;
        if before == "missing" {
            if missing {
                create_checked(&run, &path, content)
            } else {
                Err("file changed since approval".into())
            }
        } else if missing {
            Err("file changed since approval".into())
        } else {
            write_checked(&run, &path, content, before)
        }
    })();
    #[cfg(test)]
    if let Some(flag) = TEST_FAIL_AFTER_PATCH.lock().unwrap().get(id) {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    finish_patch(db, id, approval, &result)?;
    result?;
    Ok(serde_json::json!({"status":"applied"}))
}

// 2026-11-16: 파일 효과 뒤 저장이 실패하면 executing을 남겨 재시작 때 불명확 상태로 복구한다.
fn finish_patch(
    db: &Database,
    id: &str,
    approval: &str,
    result: &Result<(), String>,
) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if !["executing", "cancelling"].contains(&run["status"].as_str().unwrap_or(""))
            || run["pending"]["id"].as_str() != Some(approval)
        {
            return Err("patch execution state changed".into());
        }
        let call_id = run["pending"]["call_id"].as_str().map(str::to_owned);
        let cancelled = run["status"] == "cancelling";
        run["pending"] = serde_json::Value::Null;
        let (state, kind, message) = if result.is_ok() && !cancelled {
            (
                "awaiting_implementation",
                "tool_complete",
                "승인된 파일 변경을 적용했습니다.",
            )
        } else if result
            .as_ref()
            .err()
            .is_some_and(|e| e == "file changed since approval")
            && !cancelled
        {
            (
                "awaiting_implementation",
                "stale_tool",
                "승인 후 파일이 변경되어 요청을 적용하지 않았습니다.",
            )
        } else {
            (
                "uncertain",
                "uncertain",
                "파일 변경 결과를 확인해야 합니다.",
            )
        };
        run["status"] = state.into();
        append_event(&mut run, id, kind, message)?;
        if let Some(call_id) = &call_id {
            let event = run["events"]
                .as_array_mut()
                .and_then(|events| events.last_mut())
                .ok_or("invalid run events")?;
            event["approval_id"] = approval.into();
            event["call_id"] = call_id.clone().into();
        }
        if let Some(call_id) = call_id {
            let key = format!("{id}:{call_id}");
            let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
            let mut record: serde_json::Value = {
                let value = calls
                    .get(key.as_str())
                    .map_err(|e| e.to_string())?
                    .ok_or("call not found")?;
                serde_json::from_str(value.value()).map_err(|e| e.to_string())?
            };
            if record["status"] != "waiting" || record["approval_id"].as_str() != Some(approval) {
                return Err("call identity conflict".into());
            }
            if result.is_ok() && !cancelled {
                record["status"] = "completed".into();
                record["result"] = serde_json::json!({"status":"applied"});
            } else {
                record["status"] = "uncertain".into();
            }
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .map_err(|e| e.to_string())?;
        }
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}
#[derive(serde::Deserialize)]
struct PatchRun {
    worktree: String,
    scope: Vec<String>,
}
fn target_path(run: &PatchRun, name: &str) -> Result<(PathBuf, bool), String> {
    use std::path::Component;
    let rel = Path::new(name);
    if !crate::worktree::safe_relative(name)
        || rel.components().any(|c| !matches!(c, Component::Normal(_)))
        || crate::worktree::protected(&name.replace('\\', "/"))
        || name
            .split(['/', '\\'])
            .any(|p| p.eq_ignore_ascii_case(".git"))
    {
        return Err("unsafe path".into());
    }
    let normalized = name.replace('\\', "/").to_lowercase();
    if !run.scope.iter().any(|scope| {
        let s = scope
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        !s.is_empty() && (normalized == s || normalized.starts_with(&(s + "/")))
    }) {
        return Err("path outside approved scope".into());
    }
    let root = crate::tools::checked_root(Path::new(&run.worktree))?;
    let mut current = root.clone();
    let mut missing = false;
    for c in rel.components() {
        current.push(c);
        if missing {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err("links are not allowed".into());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err("reparse points are not allowed".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(_) => return Err("path unavailable".into()),
        }
    }
    if missing {
        return Ok((current, true));
    }
    let canonical = current.canonicalize().map_err(|_| "invalid path")?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return Err("path escapes worktree or is not a file".into());
    }
    Ok((canonical, false))
}
#[cfg(windows)]
fn checked_file(
    run: &PatchRun,
    path: &Path,
) -> Result<(std::fs::File, crate::tools::DirectoryPins), String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let root = crate::tools::checked_root(Path::new(&run.worktree))?;
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| "path escapes worktree")?;
    let pins =
        crate::tools::pin_directories(&root, relative.parent().ok_or("invalid parent")?, false)?;
    let pinned_path = pins
        .parent
        .join(relative.file_name().ok_or("invalid leaf")?);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&pinned_path)
        .map_err(|_| "file open failed")?;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.nNumberOfLinks != 1
        || info.dwFileAttributes & 0x400 != 0
    {
        return Err("hardlinks or reparse points are not allowed".into());
    }
    let mut buf = vec![0u16; 32768];
    let n = unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0) }
        as usize;
    if n == 0 || n >= buf.len() {
        return Err("cannot verify open file path".into());
    }
    let final_path = PathBuf::from(String::from_utf16(&buf[..n]).map_err(|_| "invalid file path")?);

    if !final_path.starts_with(&root) || final_path != path {
        return Err("open file escaped worktree".into());
    }
    Ok((file, pins))
}
#[cfg(windows)]
fn hash_open_file(file: &mut std::fs::File) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "file seek failed")?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file read failed")?;
    if bytes.len() > 1024 * 1024 {
        return Err("file too large".into());
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
#[cfg(windows)]
fn file_hash(run: &PatchRun, path: &Path) -> Result<String, String> {
    let (mut file, _pins) = checked_file(run, path)?;
    hash_open_file(&mut file)
}
#[cfg(windows)]
fn write_checked(run: &PatchRun, path: &Path, content: &str, expected: &str) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let (mut file, _pins) = checked_file(run, path)?;
    if expected.is_empty() || hash_open_file(&mut file)? != expected {
        return Err("file changed since approval".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "file seek failed")?;
    file.set_len(0).map_err(|_| "file resize failed")?;
    #[cfg(test)]
    if TEST_FAIL_AFTER_TRUNCATE.lock().unwrap().as_deref() == Some(path) {
        return Err("injected partial write failure".into());
    }
    file.write_all(content.as_bytes())
        .map_err(|_| "file update failed".to_owned())
}
#[cfg(not(windows))]
fn file_hash(_run: &PatchRun, _path: &Path) -> Result<String, String> {
    Err("file patch is supported only on Windows".into())
}
#[cfg(not(windows))]
fn write_checked(
    _run: &PatchRun,
    _path: &Path,
    _content: &str,
    _expected: &str,
) -> Result<(), String> {
    Err("file patch is supported only on Windows".into())
}

#[cfg(windows)]
fn create_checked(run: &PatchRun, path: &Path, content: &str) -> Result<(), String> {
    use std::{
        io::Write,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let root = crate::tools::checked_root(Path::new(&run.worktree))?;
    let rel = path
        .strip_prefix(&root)
        .map_err(|_| "path escapes worktree")?;
    let parent = rel.parent().ok_or("invalid parent")?;
    let pins = crate::tools::pin_directories(&root, parent, true)?;
    let pinned_path = pins.parent.join(rel.file_name().ok_or("invalid leaf")?);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&pinned_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err("file changed since approval".into())
        }
        Err(_) => return Err("file creation failed".into()),
    };
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.nNumberOfLinks != 1
        || info.dwFileAttributes & 0x400 != 0
    {
        return Err("unsafe created file".into());
    }
    let mut buf = vec![0u16; 32768];
    let n = unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0) }
        as usize;
    if n == 0 || n >= buf.len() {
        return Err("cannot verify created file".into());
    }
    let actual = PathBuf::from(String::from_utf16(&buf[..n]).map_err(|_| "invalid path")?);
    if actual != pinned_path || !actual.starts_with(&pins.root) {
        return Err("created path escaped worktree".into());
    }
    file.write_all(content.as_bytes())
        .map_err(|_| "file creation failed".into())
}
#[cfg(not(windows))]
fn create_checked(_run: &PatchRun, _path: &Path, _content: &str) -> Result<(), String> {
    Err("file patch is supported only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_identity_is_scoped_to_actual_attempt() {
        let root =
            std::env::temp_dir().join(format!("magi-evidence-attempt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let engine = crate::Engine::open(&root.join("state.redb")).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "fixture".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(p) => p,
            _ => panic!("project"),
        };
        let db = engine.db.get().unwrap();
        let evidence: domain::VerificationEvidence = serde_json::from_value(serde_json::json!({"verification_id":"verify","revision":"fixture-revision","spec_hash":"fixture-spec","command":"fixture-command","passed":true,"output":"fixture-output","worktree_hash":"fixture-snapshot","requirement_ids":[],"acceptance_ids":[]})).unwrap();
        let old = serde_json::to_value(&evidence).unwrap();
        let old_context = serde_json::json!({"schema_version":1,"spec_version":1,"context_version":0,"agent_attempt":1,"task_id":"task"});
        let run = serde_json::json!({"id":"run","project_id":project.id,"status":"awaiting_implementation","spec_version":1,"context_version":0,"agent_attempt":2,"evidence":[old],"evidence_contexts":{"verify":old_context}});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(RUNS)
                .unwrap()
                .insert("run", run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let expected = get(&db, "run").unwrap();
        record_verification(
            &db,
            "run",
            &expected,
            &project,
            &evidence,
            "task",
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let saved = get(&db, "run").unwrap();
        assert_eq!(saved["evidence"].as_array().unwrap().len(), 2);
        assert_eq!(saved["evidence"][0], old);
        assert_eq!(saved["evidence_contexts"]["verify"], old_context);
        assert!(record_verification(
            &db,
            "run",
            &saved,
            &project,
            &evidence,
            "task",
            &tokio_util::sync::CancellationToken::new()
        )
        .is_err());
        drop(db);
        drop(engine);
        let reopened = crate::Engine::open(&root.join("state.redb")).unwrap();
        let db = reopened.db.get().unwrap();
        let saved = get(&db, "run").unwrap();
        assert!(crate::evidence::resolve(&saved, "verify", 1)
            .unwrap()
            .is_some());
        assert!(crate::evidence::resolve(&saved, "verify", 2)
            .unwrap()
            .is_some());
        drop(db);
        drop(reopened);
        let _ = std::fs::remove_file(root.join("state.redb"));
        let _ = std::fs::remove_dir(root);
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_between_patch_read_and_commit_cannot_revive_run() {
        let root = std::env::temp_dir().join(format!("magi-patch-cas-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/file.txt"), "before").unwrap();
        let path = root.join("state.redb");
        let db = std::sync::Arc::new(Database::create(&path).unwrap());
        let run_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":"fixture","status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        *TEST_PAUSE_AFTER_PATCH_READ.lock().unwrap() =
            Some((run_id.clone(), ready.clone(), release.clone()));
        let worker_db = db.clone();
        let id = run_id.clone();
        let worker = std::thread::spawn(move || {
            submit_patch(
                &worker_db,
                &id,
                "patch".into(),
                serde_json::json!({"path":"src/file.txt","content":"after"}),
            )
        });
        ready.wait();
        cancel(&db, &run_id).unwrap();
        release.wait();
        assert!(worker.join().unwrap().is_err());
        *TEST_PAUSE_AFTER_PATCH_READ.lock().unwrap() = None;
        let run = get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "cancelled");
        assert!(run["pending"].is_null());
        assert_eq!(
            std::fs::read_to_string(worktree.join("src/file.txt")).unwrap(),
            "before"
        );
        drop(db);
        std::fs::remove_file(worktree.join("src/file.txt")).unwrap();
        std::fs::remove_dir(worktree.join("src")).unwrap();
        std::fs::remove_dir(worktree).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn cancelling_an_executing_patch_keeps_its_result_uncertain() {
        let root = std::env::temp_dir().join(format!("magi-patch-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            let run = serde_json::json!({"id":"run","status":"executing","pending":{"id":"approval"},"events":[]});
            table.insert("run", run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        cancel(&db, "run").unwrap();
        assert_eq!(get(&db, "run").unwrap()["status"], "cancelling");
        finish_patch(&db, "run", "approval", &Ok(())).unwrap();
        let run = get(&db, "run").unwrap();
        assert_eq!(run["status"], "uncertain");
        assert!(run["pending"].is_null());
        drop(db);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn applied_worktree_is_committed_once_and_preserves_other_run_fields() {
        let dir = std::env::temp_dir().join(format!("magi-run-finish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("state.redb");
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table.insert("run", r#"{"id":"run","status":"preparing","events":[],"agent_attempt":9,"custom":"preserve"}"#).unwrap();
        }
        tx.commit().unwrap();
        assert!(finish_worktree(&db, "run", &Ok(()), "completion").unwrap());
        assert!(finish_worktree(&db, "run", &Ok(()), "completion").unwrap());
        let tx = db.begin_read().unwrap();
        let table = tx.open_table(RUNS).unwrap();
        let stored = table.get("run").unwrap().unwrap();
        let run: serde_json::Value = serde_json::from_str(stored.value()).unwrap();
        assert_eq!(run["status"], "awaiting_implementation");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        assert_eq!(run["events"][0]["kind"], "worktree_ready");
        assert_eq!(run["agent_attempt"], 9);
        assert_eq!(run["custom"], "preserve");
        drop(stored);
        drop(table);
        drop(tx);
        drop(db);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn cancellation_becomes_uncertain_and_superseded_completion_is_marked() {
        let dir = std::env::temp_dir().join(format!("magi-run-states-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("state.redb");
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table
                .insert(
                    "cancel",
                    r#"{"id":"cancel","status":"cancelling","events":[]}"#,
                )
                .unwrap();
            table
                .insert("done", r#"{"id":"done","status":"cancelled","events":[]}"#)
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(finish_worktree(&db, "cancel", &Ok(()), "one").unwrap());
        assert!(!finish_worktree(&db, "done", &Ok(()), "two").unwrap());
        assert!(!finish_worktree(&db, "done", &Ok(()), "two").unwrap());
        let tx = db.begin_read().unwrap();
        let table = tx.open_table(RUNS).unwrap();
        let value = table.get("cancel").unwrap().unwrap();
        let run: serde_json::Value = serde_json::from_str(value.value()).unwrap();
        assert_eq!(run["status"], "uncertain");
        assert_eq!(run["events"][0]["kind"], "uncertain");
        drop(value);
        drop(table);
        drop(tx);
        drop(db);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn simultaneous_git_preparations_are_serialized_and_both_succeed() {
        let _test_gate = super::TEST_GIT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use std::os::windows::process::CommandExt;
        use std::{
            process::{Command, Stdio},
            sync::{atomic::Ordering, Arc, Barrier, Mutex},
            thread,
        };
        let root = std::env::temp_dir().join(format!("magi-git-serial-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "fixture").unwrap();
        git(&["add", "a.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        let revision = git(&["rev-parse", "HEAD"]);
        let gate = Arc::new(Mutex::new(()));
        let barrier = Arc::new(Barrier::new(3));
        super::MAX_GIT.store(0, Ordering::SeqCst);
        let mut threads = Vec::new();
        for id in ["first", "second"] {
            let repo = repo.clone();
            let revision = revision.clone();
            let gate = Arc::clone(&gate);
            let barrier = Arc::clone(&barrier);
            let target = root.join("magi-runs").join(id);
            threads.push(thread::spawn(move || {
                barrier.wait();
                super::serialized_worktree(&gate, &repo, &revision, &target)
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(super::MAX_GIT.load(Ordering::SeqCst), 1);
        assert!(root.join("magi-runs/first/a.txt").is_file());
        assert!(root.join("magi-runs/second/a.txt").is_file());
        let resolved = root.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(temp.as_path()));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-git-serial-"));
        std::fs::remove_dir_all(resolved).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn concurrent_start_requests_serialize_real_git_and_keep_both_results() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            sync::{atomic::Ordering, Arc, Barrier},
            thread,
            time::{Duration, Instant},
        };
        let _test_gate = super::TEST_GIT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("magi-start-serial-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "fixture").unwrap();
        git(&["add", "a.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        let revision = git(&["rev-parse", "HEAD"]);
        let engine = Arc::new(crate::Engine::open(&root.join("state.redb")).unwrap());
        let create = engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fake".into(),
                    path: repo.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap();
        let project = match create {
            domain::ResponseData::Project(v) => v,
            _ => panic!("project"),
        };
        {
            let db = engine.db.get().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(crate::PROJECTS).unwrap();
                let mut value = serde_json::to_value(&project).unwrap();
                value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":revision,"scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
                value["model_settings"] = serde_json::json!({"provider":"mock","model":"fake","capabilities":{"streaming":false,"tools":false,"context_tokens":null}});
                let json = value.to_string();
                t.insert(project.id.as_str(), json.as_str()).unwrap();
            }
            tx.commit().unwrap();
        }
        super::MAX_GIT.store(0, Ordering::SeqCst);
        super::TEST_GIT_CALLS.store(0, Ordering::SeqCst);
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                engine
                    .handle(domain::Request {
                        id: "start".into(),
                        command: domain::Command::StartRun {
                            project_id: project.id,
                            expected_approved_hash: project.draft_hash,
                        },
                    })
                    .result
                    .unwrap()
            }));
        }
        barrier.wait();
        let ids: Vec<_> = threads
            .into_iter()
            .map(|t| match t.join().unwrap() {
                domain::ResponseData::Run { run_id } => run_id,
                _ => panic!("run"),
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if engine.slots.load(Ordering::Acquire) == 0 {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(super::TEST_GIT_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(super::MAX_GIT.load(Ordering::SeqCst), 1);
        for id in ids {
            let response = engine
                .handle(domain::Request {
                    id: "get".into(),
                    command: domain::Command::GetRun { run_id: id },
                })
                .result
                .unwrap();
            let run = match response {
                domain::ResponseData::Data(v) => v,
                _ => panic!("run"),
            };
            assert_eq!(run["status"], "awaiting_implementation");
        }
        let held = engine.git_gate.lock().unwrap();
        let response = engine
            .handle(domain::Request {
                id: "third".into(),
                command: domain::Command::StartRun {
                    project_id: project.id.clone(),
                    expected_approved_hash: project.draft_hash.clone(),
                },
            })
            .result
            .unwrap();
        let id = match response {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        engine
            .handle(domain::Request {
                id: "cancel".into(),
                command: domain::Command::CancelRun { run_id: id.clone() },
            })
            .result
            .unwrap();
        let response = engine
            .handle(domain::Request {
                id: "get".into(),
                command: domain::Command::GetRun { run_id: id.clone() },
            })
            .result
            .unwrap();
        let run = match response {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "cancelling");
        assert_eq!(run["resources_released"], false);
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(15);
        while engine.slots.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let response = engine
            .handle(domain::Request {
                id: "get".into(),
                command: domain::Command::GetRun { run_id: id },
            })
            .result
            .unwrap();
        let run = match response {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "uncertain");
        assert_eq!(run["resources_released"], false);
        drop(engine);
        let resolved = root.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(temp.as_path()));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-start-serial-"));
        std::fs::remove_dir_all(resolved).unwrap();
    }
}

// 2026-11-16: 외부 설정과 표준 입력을 차단하고 출력과 실행 시간을 제한해 읽기 전용 Git 조회만 수행한다.
#[cfg(windows)]
fn diff_git(root: &std::path::Path, args: &[&str], limit: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut command = Command::new("git");
    command
        .creation_flags(0x08000000)
        .current_dir(root)
        .env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "NUL")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "--no-lazy-fetch",
            "--no-replace-objects",
            "-c",
            "core.hooksPath=NUL",
            "-c",
            "core.attributesFile=NUL",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cannot start Git")?;
    let stdout = child.stdout.take().ok_or("Git output unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Git output unavailable")?;
        Ok::<_, &str>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().map_err(|_| "Git output unavailable")??;
    if bytes.len() as u64 > limit {
        return Err("Git output exceeds limit".into());
    }
    if !status.is_some_and(|s| s.success()) {
        return Err(
            "Git inspection failed; --no-lazy-fetch support and local objects are required".into(),
        );
    }
    Ok(bytes)
}

// 2026-11-16: 디렉터리 탐색 전 보호 이름·링크를 거부하고 검증된 읽기 경로로 내용을 해시한다.
#[cfg(windows)]
fn diff_snapshot(
    root: &std::path::Path,
    scope: &[String],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::path::{Component, Path};
    fn inspect(root: &Path, path: &Path) -> Result<Option<std::fs::Metadata>, String> {
        let relative = path.strip_prefix(root).map_err(|_| "scope escaped")?;
        let name = relative.to_str().ok_or("invalid path")?.replace('\\', "/");
        if !crate::worktree::safe_relative(&name)
            || crate::worktree::protected(&name)
            || name
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".git"))
        {
            return Err("unsafe scope path".into());
        }
        let mut current = root.to_path_buf();
        for part in relative.components() {
            if !matches!(part, Component::Normal(_)) {
                return Err("invalid scope path".into());
            }
            current.push(part);
            let meta = match std::fs::symlink_metadata(&current) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("scope unavailable".into()),
            };
            use std::os::windows::fs::MetadataExt;
            if meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0 {
                return Err("scope contains link".into());
            }
        }
        Ok(Some(
            std::fs::symlink_metadata(path).map_err(|_| "scope unavailable")?,
        ))
    }
    fn walk(root: &Path, path: &Path, files: &mut BTreeSet<String>) -> Result<(), String> {
        let Some(meta) = inspect(root, path)? else {
            return Ok(());
        };
        if meta.is_dir() {
            let relative = path.strip_prefix(root).map_err(|_| "scope escaped")?;
            let _pins = crate::tools::pin_directories(root, relative, false)?;
            for entry in std::fs::read_dir(path).map_err(|_| "scope unavailable")? {
                let entry = entry.map_err(|_| "scope unavailable")?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(".git")
                {
                    continue;
                }
                walk(root, &entry.path(), files)?;
            }
        } else if meta.is_file() {
            let name = path
                .strip_prefix(root)
                .map_err(|_| "scope escaped")?
                .to_str()
                .ok_or("invalid path")?
                .replace('\\', "/");
            files.insert(name);
            if files.len() > 10_000 {
                return Err("scope too large".into());
            }
        } else {
            return Err("unsupported scope entry".into());
        }
        Ok(())
    }
    let mut names = BTreeSet::new();
    for item in scope {
        walk(root, &root.join(item), &mut names)?;
    }
    let context = crate::tools::ToolContext {
        worktree: root.to_path_buf(),
        allowed_paths: scope.to_vec(),
        owned_paths: scope.to_vec(),
        allowlist: vec![],
        timeout: std::time::Duration::from_secs(15),
        max_output_bytes: 1024 * 1024,
        side_effect_authorized: false,
    };
    let mut result = std::collections::BTreeMap::new();
    for name in names {
        let text = crate::tools::read_checked(&context, &name)?;
        result.insert(name, format!("{:x}", Sha256::digest(text.as_bytes())));
    }
    Ok(result)
}

// 2026-11-16: 재개 시 파일 해시 없이 저장된 실제 경로와 Git HEAD가 승인 revision과 일치하는지 확인한다.
#[cfg(windows)]
pub(crate) fn verify_resume_revision(run: &serde_json::Value) -> Result<(), String> {
    let root = std::path::Path::new(run["worktree"].as_str().ok_or("invalid worktree")?);
    let canonical = root.canonicalize().map_err(|_| "invalid worktree")?;
    if canonical != root {
        return Err("worktree path changed".into());
    }
    let _pins = crate::tools::pin_directories(root, std::path::Path::new(""), false)?;
    let head = String::from_utf8(diff_git(root, &["rev-parse", "--verify", "HEAD"], 128)?)
        .map_err(|_| "invalid Git revision")?;
    let revision = run["revision"].as_str().ok_or("invalid revision")?;
    if !head.trim().eq_ignore_ascii_case(revision) {
        return Err("worktree revision changed".into());
    }
    Ok(())
}

// 2026-11-16: 승인된 기준과 현재 HEAD 및 범위를 묶고 읽기 전후 스냅샷이 달라지면 결과를 폐기한다.
#[cfg(windows)]
pub(crate) fn diff(db: &Database, id: &str) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::path::Path;
    let run = get(db, id)?;
    let revision = run["revision"].as_str().ok_or("invalid revision")?;
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid revision".into());
    }
    let spec_hash = run["spec_hash"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("invalid spec hash")?;
    let spec_version = run["spec_version"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or("invalid spec version")?;
    let scope: Vec<String> =
        serde_json::from_value(run["scope"].clone()).map_err(|_| "invalid scope")?;
    if scope.is_empty() {
        return Err("invalid scope".into());
    }
    let root = Path::new(run["worktree"].as_str().ok_or("invalid worktree")?);
    let _pins = crate::tools::pin_directories(root, Path::new(""), false)?;
    let head = String::from_utf8(diff_git(root, &["rev-parse", "--verify", "HEAD"], 128)?)
        .map_err(|_| "invalid Git revision")?
        .trim()
        .to_owned();
    if !head.eq_ignore_ascii_case(revision) {
        return Err("worktree revision changed".into());
    }
    let before = diff_snapshot(root, &scope)?;
    #[cfg(test)]
    {
        let hook = {
            let mut hook = TEST_DIFF_HOOK.lock().unwrap();
            if hook.as_ref().is_some_and(|hook| hook.run_id == id) {
                hook.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook.reached
                .send(())
                .map_err(|_| "test synchronization failed")?;
            hook.proceed
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| "test synchronization failed")?;
        }
    }
    let tree = diff_git(
        root,
        &["ls-tree", "-r", "-z", "--full-tree", revision],
        4 * 1024 * 1024,
    )?;
    let in_scope = |name: &str| {
        let name = name.to_lowercase();
        scope.iter().any(|s| {
            let s = s.replace('\\', "/").trim_end_matches('/').to_lowercase();
            !s.is_empty() && (name == s || name.starts_with(&(s + "/")))
        })
    };
    let mut baseline = BTreeSet::new();
    for entry in tree.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let pos = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("invalid Git tree")?;
        let name = std::str::from_utf8(&entry[pos + 1..]).map_err(|_| "invalid Git path")?;
        if in_scope(name) {
            let metadata = std::str::from_utf8(&entry[..pos]).map_err(|_| "invalid Git tree")?;
            if !crate::worktree::safe_relative(name)
                || crate::worktree::protected(name)
                || name.split('/').any(|s| s.eq_ignore_ascii_case(".git"))
                || !(metadata.starts_with("100644 blob ") || metadata.starts_with("100755 blob "))
            {
                return Err("unsafe tracked path".into());
            }
            baseline.insert(name.to_owned());
            if baseline.len() > 10_000 {
                return Err("scope too large".into());
            }
        }
    }
    let context = crate::tools::ToolContext {
        worktree: root.to_path_buf(),
        allowed_paths: scope.clone(),
        owned_paths: scope.clone(),
        allowlist: vec![],
        timeout: std::time::Duration::from_secs(15),
        max_output_bytes: 1024 * 1024,
        side_effect_authorized: false,
    };
    let names: BTreeSet<_> = baseline.iter().chain(before.keys()).cloned().collect();
    let mut changes = Vec::new();
    let mut total = 0usize;
    for name in names {
        let previous = if baseline.contains(&name) {
            let object = format!("{revision}:{name}");
            let size = diff_git(root, &["cat-file", "-s", &object], 128)?;
            let size = std::str::from_utf8(&size)
                .map_err(|_| "invalid blob size")?
                .trim()
                .parse::<u64>()
                .map_err(|_| "invalid blob size")?;
            if size > 1024 * 1024 {
                return Err("baseline file too large".into());
            }
            Some(
                String::from_utf8(diff_git(
                    root,
                    &["show", "--no-ext-diff", "--no-textconv", &object],
                    1024 * 1024,
                )?)
                .map_err(|_| "binary baseline unsupported")?,
            )
        } else {
            None
        };
        let next = if let Some(expected) = before.get(&name) {
            let content = crate::tools::read_checked(&context, &name)?;
            if format!("{:x}", Sha256::digest(content.as_bytes())) != *expected {
                return Err("worktree changed during diff".into());
            }
            Some(content)
        } else {
            None
        };
        if previous != next {
            total +=
                previous.as_ref().map_or(0, String::len) + next.as_ref().map_or(0, String::len);
            if total > 4 * 1024 * 1024 {
                return Err("diff too large".into());
            }
            changes.push(serde_json::json!({"path":name,"before":previous,"after":next}));
        }
    }
    let after = diff_snapshot(root, &scope)?;
    let final_head = String::from_utf8(diff_git(root, &["rev-parse", "--verify", "HEAD"], 128)?)
        .map_err(|_| "invalid Git revision")?;
    if before != after || !final_head.trim().eq_ignore_ascii_case(&head) || get(db, id)? != run {
        return Err("worktree changed during diff".into());
    }
    let mut hash = Sha256::new();
    for (name, digest) in after {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update(digest.as_bytes());
    }
    Ok(
        serde_json::json!({"revision":revision,"current_revision":head,"spec_hash":spec_hash,"spec_version":spec_version,"scope":scope,"worktree_hash":format!("{:x}",hash.finalize()),"changes":changes}),
    )
}

#[cfg(not(windows))]
pub(crate) fn diff(_db: &Database, _id: &str) -> Result<serde_json::Value, String> {
    Err("diff is supported only on Windows".into())
}

#[cfg(all(test, windows))]
mod diff_snapshot_tests {
    use super::*;
    use std::{
        fs,
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        sync::{mpsc, Arc},
    };
    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .creation_flags(0x08000000)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    #[test]
    fn detects_mutation_between_scope_snapshots() {
        let root = std::env::temp_dir().join(format!("magi-diff-race-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/a.txt"), "before").unwrap();
        git(&root, &["add", "src/a.txt"]);
        git(
            &root,
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
        let revision = git(&root, &["rev-parse", "HEAD"]);
        let path = root.join("fixture.redb");
        let db = Arc::new(Database::create(&path).unwrap());
        let run_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            let value=serde_json::json!({"id":run_id,"revision":revision,"spec_hash":"fixture","spec_version":1,"scope":["src"],"worktree":root.to_string_lossy()}).to_string();
            table.insert(run_id.as_str(), value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let (reached_tx, reached_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        *TEST_DIFF_HOOK.lock().unwrap() = Some(DiffTestHook {
            run_id: run_id.clone(),
            reached: reached_tx,
            proceed: proceed_rx,
        });
        let worker = {
            let db = Arc::clone(&db);
            let id = run_id.clone();
            std::thread::spawn(move || diff(&db, &id))
        };
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        fs::write(root.join("src/a.txt"), "changed during lookup").unwrap();
        proceed_tx.send(()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            "worktree changed during diff"
        );
        drop(db);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-diff-race-"));
        fs::remove_dir_all(root).unwrap();
    }
}

// 2026-11-16: 승인 대상 전체를 같은 트랜잭션에 고정해 경합 중 요청이 바뀌지 않게 한다.
// 2026-11-16: 명령 호출과 승인 대기를 같은 트랜잭션에 남겨 중단된 프로세스를 추측해 재실행하지 않는다.
#[cfg(windows)]
pub(crate) fn submit_bound_exec(
    db: &Database,
    id: &str,
    role: &str,
    task: &str,
    call: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    if call.is_empty() || call.len() > 1024 || id.len() > 128 {
        return Err("invalid call identity".into());
    }
    validate_exec_args(&args)?;
    let hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(role, task, "exec", &args)).map_err(|e| e.to_string())?
        )
    );
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let result = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = serde_json::from_str(
            table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        normalize_run(&mut run)?;
        validate_bound_exec(&tx, &run, role, task, &args, true)?;
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let key = format!("{id}:{call}");
        let existing = calls
            .get(key.as_str())
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_owned());
        if let Some(existing) = existing {
            let record: serde_json::Value =
                serde_json::from_str(&existing).map_err(|e| e.to_string())?;
            if record["hash"] != hash {
                return Err("call identity conflict".into());
            }
            match record["status"].as_str() {
                Some("completed") => {
                    serde_json::json!({"status":"completed","result":record["result"]})
                }
                Some("waiting")
                    if ["awaiting_approval", "approved_exec"]
                        .contains(&run["status"].as_str().unwrap_or(""))
                        && run["pending"]["id"] == record["approval_id"]
                        && run["pending"]["call_id"] == call =>
                {
                    serde_json::json!({"status":run["status"],"approval_id":record["approval_id"]})
                }
                _ => return Err("previous tool outcome uncertain".into()),
            }
        } else {
            if run["status"] != "awaiting_implementation"
                || !run["pending"].is_null()
                || run["used_calls"]
                    .as_array()
                    .is_none_or(|v| v.iter().any(|v| v == call))
            {
                return Err("run not ready for call".into());
            }
            let approval = uuid::Uuid::new_v4().to_string();
            let record = serde_json::json!({"status":"waiting","hash":hash,"approval_id":approval,"role":role,"task_id":task,"origin_agent_attempt":run["agent_attempt"]});
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .map_err(|e| e.to_string())?;
            run["used_calls"].as_array_mut().unwrap().push(call.into());
            run["pending"] = serde_json::json!({"id":approval,"tool":"exec","arguments":args,"call_id":call,"authorized":false});
            run["status"] = "awaiting_approval".into();
            append_event(
                &mut run,
                id,
                "approval_required",
                "명령 실행에 사용자 승인이 필요합니다.",
            )?;
            table
                .insert(id, run.to_string().as_str())
                .map_err(|e| e.to_string())?;
            serde_json::json!({"status":"awaiting_approval","approval_id":approval})
        }
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

fn validate_exec_args(args: &serde_json::Value) -> Result<(), String> {
    let object = args.as_object().ok_or("invalid command")?;
    if object.len() != 3
        || args["program"]
            .as_str()
            .is_none_or(|v| v.trim().is_empty() || v.contains('\0'))
        || args["args"].as_array().is_none_or(|v| {
            v.len() > 128
                || v.iter()
                    .any(|x| x.as_str().is_none_or(|s| s.contains('\0')))
        })
        || args["cwd"].as_str().is_none_or(|v| {
            !crate::worktree::safe_relative(v) || crate::worktree::protected(&v.replace('\\', "/"))
        })
    {
        return Err("invalid command".into());
    }
    Ok(())
}

fn validate_bound_exec(
    tx: &redb::WriteTransaction,
    run: &serde_json::Value,
    role: &str,
    task: &str,
    args: &serde_json::Value,
    active: bool,
) -> Result<(), String> {
    validate_exec_args(args)?;
    if (active && run["agent_active"] != true)
        || run["agent_configs"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["role"] == role))
            .and_then(|v| v["tools"].as_array())
            .is_none_or(|v| !v.iter().any(|v| v == "exec"))
    {
        return Err("tool not allowed for active role".into());
    }
    let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
    let row = projects
        .get(run["project_id"].as_str().ok_or("invalid project")?)
        .map_err(|e| e.to_string())?
        .ok_or("project not found")?;
    let project: domain::Project = serde_json::from_str(row.value()).map_err(|e| e.to_string())?;
    let spec = project
        .approved
        .as_ref()
        .ok_or("approved specification changed")?;
    if run["spec_hash"].as_str() != Some(spec.hash.as_str())
        || run["spec_version"].as_u64() != Some(spec.version)
        || run["context_version"].as_u64() != Some(project.context_version)
        || project.draft_hash != spec.hash
        || run["scope"] != serde_json::json!(spec.scope)
    {
        return Err("approved specification changed".into());
    }
    let task = spec
        .tasks
        .iter()
        .find(|v| v.id == task)
        .ok_or("task not found")?;
    let cwd = args["cwd"]
        .as_str()
        .ok_or("invalid cwd")?
        .replace('\\', "/")
        .to_lowercase();
    for scope in [&spec.scope, &task.owned_paths] {
        if !scope.iter().any(|v| {
            let v = v.replace('\\', "/").trim_end_matches('/').to_lowercase();
            !v.is_empty() && (cwd == v || cwd.starts_with(&(v + "/")))
        }) {
            return Err("working directory outside scope".into());
        }
    }
    Ok(())
}

// 2026-11-16: 외부 프로세스 생성 전에 정확한 승인과 호출을 원자적으로 소모한다.
#[cfg(windows)]
pub(crate) fn consume_bound_exec(
    db: &Database,
    id: &str,
    approval: &str,
    call: &str,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let args = {
        let mut runs = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = serde_json::from_str(
            runs.get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        if run["status"] != "approved_exec"
            || run["pending"]["tool"] != "exec"
            || run["pending"]["id"] != approval
            || run["pending"]["call_id"] != call
            || run["pending"]["authorized"] != true
        {
            return Err("approval unavailable or already consumed".into());
        }
        let key = format!("{id}:{call}");
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let mut record: serde_json::Value = serde_json::from_str(
            calls
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .ok_or("call not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        let role = record["role"].as_str().ok_or("invalid call")?;
        let task = record["task_id"].as_str().ok_or("invalid call")?;
        let args = run["pending"]["arguments"].clone();
        let hash = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(role, task, "exec", &args)).map_err(|e| e.to_string())?
            )
        );
        if record["status"] != "waiting"
            || record["approval_id"] != approval
            || record["hash"] != hash
        {
            return Err("call identity conflict".into());
        }
        validate_bound_exec(&tx, &run, role, task, &args, true)?;
        record["status"] = "executing".into();
        record["effect_agent_attempt"] = run["agent_attempt"].clone();
        calls
            .insert(key.as_str(), record.to_string().as_str())
            .map_err(|e| e.to_string())?;
        run["status"] = "executing".into();
        append_event(&mut run, id, "tool_started", "승인된 명령을 시작합니다.")?;
        let event = run["events"]
            .as_array_mut()
            .and_then(|v| v.last_mut())
            .ok_or("invalid event")?;
        event["approval_id"] = approval.into();
        event["call_id"] = call.into();
        runs.insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
        args
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(args)
}

// 2026-11-16: 실제 결과와 호출 상태를 함께 기록하고 취소 또는 실패는 결과 불명으로 남긴다.
#[cfg(windows)]
pub(crate) fn finish_bound_exec(
    db: &Database,
    id: &str,
    approval: &str,
    call: &str,
    result: Option<serde_json::Value>,
) -> Result<bool, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let completed = {
        let mut runs = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = serde_json::from_str(
            runs.get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        if !["executing", "cancelling"].contains(&run["status"].as_str().unwrap_or(""))
            || run["pending"]["tool"] != "exec"
            || run["pending"]["id"] != approval
            || run["pending"]["call_id"] != call
        {
            return Err("command not active".into());
        }
        let complete = result.is_some() && run["status"] == "executing";
        let key = format!("{id}:{call}");
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let mut record: serde_json::Value = serde_json::from_str(
            calls
                .get(key.as_str())
                .map_err(|e| e.to_string())?
                .ok_or("call not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        if record["status"] != "executing" || record["approval_id"] != approval {
            return Err("call identity conflict".into());
        }
        record["status"] = if complete { "completed" } else { "uncertain" }.into();
        if complete {
            record["result"] = result.ok_or("result missing")?;
        }
        let encoded = record.to_string();
        if encoded.len() > MAX_DURABLE_RECORD_BYTES {
            return Err("call result too large".into());
        }
        calls
            .insert(key.as_str(), encoded.as_str())
            .map_err(|e| e.to_string())?;
        run["pending"] = serde_json::Value::Null;
        run["status"] = if complete {
            "awaiting_implementation"
        } else {
            "uncertain"
        }
        .into();
        if !complete {
            run["resources_unknown"] = true.into();
        }
        append_event(
            &mut run,
            id,
            if complete {
                "tool_complete"
            } else {
                "uncertain"
            },
            if complete {
                "명령 실행 결과를 받았습니다."
            } else {
                "명령 실행 결과를 확인해야 합니다."
            },
        )?;
        let event = run["events"]
            .as_array_mut()
            .and_then(|v| v.last_mut())
            .ok_or("invalid event")?;
        event["approval_id"] = approval.into();
        event["call_id"] = call.into();
        runs.insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
        complete
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(completed)
}

#[cfg(windows)]
pub(crate) fn submit_bound_mcp(
    db: &Database,
    id: &str,
    role: &str,
    task: &str,
    call: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    if call.is_empty() || call.len() > 1024 || id.len() > 128 {
        return Err("invalid call identity".into());
    }
    let endpoint = args["endpoint"].as_str().ok_or("invalid endpoint")?;
    integrations::mcp::validate_endpoint(endpoint)?;
    if args["name"].as_str().is_none_or(|v| v.trim().is_empty())
        || !args["arguments"].is_object()
        || args["cwd"] != "."
        || args["side_effect"] != true
    {
        return Err("invalid MCP approval request".into());
    }
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(role, task, "mcp", &args)).map_err(|e| e.to_string())?)
    );
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let result = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = serde_json::from_str(
            table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?
                .value(),
        )
        .map_err(|e| e.to_string())?;
        normalize_run(&mut run)?;
        validate_bound_mcp(&tx, &run, role, task, true)?;
        let stamp = crate::mcp_connections::endpoint_in_write(
            &tx,
            run["project_id"].as_str().ok_or("invalid project")?,
        )?;
        if stamp.0 != endpoint {
            return Err("MCP endpoint changed".into());
        }
        let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
        let key = format!("{id}:{call}");
        let existing = calls
            .get(key.as_str())
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_owned());
        if let Some(existing) = existing {
            let record: serde_json::Value =
                serde_json::from_str(&existing).map_err(|e| e.to_string())?;
            validate_mcp_stamp(&tx, &run, &record)?;
            if record["hash"] != hash {
                return Err("call identity conflict".into());
            }
            match record["status"].as_str() {
                Some("completed") => {
                    serde_json::json!({"status":"completed","result":record["result"]})
                }
                Some("waiting")
                    if ["awaiting_approval", "approved_mcp"]
                        .contains(&run["status"].as_str().unwrap_or(""))
                        && run["pending"]["id"] == record["approval_id"]
                        && run["pending"]["call_id"] == call =>
                {
                    serde_json::json!({"status":run["status"],"approval_id":record["approval_id"]})
                }
                _ => return Err("previous tool outcome uncertain".into()),
            }
        } else {
            if run["status"] != "awaiting_implementation"
                || !run["pending"].is_null()
                || run["used_calls"]
                    .as_array()
                    .is_none_or(|v| v.iter().any(|v| v == call))
            {
                return Err("run not ready for call".into());
            }
            let approval = uuid::Uuid::new_v4().to_string();
            let record = serde_json::json!({"status":"waiting","hash":hash,"approval_id":approval,"role":role,"task_id":task,"endpoint_stamp":stamp});
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .map_err(|e| e.to_string())?;
            run["used_calls"].as_array_mut().unwrap().push(call.into());
            run["pending"] = serde_json::json!({"id":approval,"tool":"mcp","arguments":args,"call_id":call,"authorized":false});
            run["status"] = "awaiting_approval".into();
            append_event(
                &mut run,
                id,
                "approval_required",
                "MCP 요청에 사용자 승인이 필요합니다.",
            )?;
            table
                .insert(id, run.to_string().as_str())
                .map_err(|e| e.to_string())?;
            serde_json::json!({"status":"awaiting_approval","approval_id":approval})
        }
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

fn validate_bound_mcp(
    tx: &redb::WriteTransaction,
    run: &serde_json::Value,
    role: &str,
    task: &str,
    require_active: bool,
) -> Result<(), String> {
    if (require_active && run["agent_active"] != true)
        || run["agent_configs"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["role"] == role))
            .and_then(|v| v["tools"].as_array())
            .is_none_or(|v| !v.iter().any(|v| v == "mcp"))
    {
        return Err("tool not allowed for active role".into());
    }
    let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
    let row = projects
        .get(run["project_id"].as_str().ok_or("invalid project")?)
        .map_err(|e| e.to_string())?
        .ok_or("project not found")?;
    let project: domain::Project = serde_json::from_str(row.value()).map_err(|e| e.to_string())?;
    let spec = project
        .approved
        .as_ref()
        .ok_or("approved specification changed")?;
    if run["spec_hash"].as_str() != Some(spec.hash.as_str())
        || run["spec_version"].as_u64() != Some(spec.version)
        || run["context_version"].as_u64() != Some(project.context_version)
        || project.draft_hash != spec.hash
        || run["scope"] != serde_json::json!(spec.scope)
        || !spec.tasks.iter().any(|v| v.id == task)
    {
        return Err("approved specification changed".into());
    }
    Ok(())
}

fn validate_mcp_stamp(
    tx: &redb::WriteTransaction,
    run: &serde_json::Value,
    record: &serde_json::Value,
) -> Result<(), String> {
    let current = crate::mcp_connections::endpoint_in_write(
        tx,
        run["project_id"].as_str().ok_or("invalid project")?,
    )?;
    if record["endpoint_stamp"] != serde_json::json!(current) {
        return Err("MCP endpoint changed".into());
    }
    Ok(())
}

pub(crate) fn request_mcp(
    db: &Database,
    id: &str,
    endpoint: String,
    name: String,
    arguments: serde_json::Value,
    cwd: String,
) -> Result<serde_json::Value, String> {
    integrations::mcp::validate_endpoint(&endpoint)?;
    if name.trim().is_empty() || !arguments.is_object() || cwd != "." {
        return Err("invalid MCP approval request".into());
    }
    let approval = uuid::Uuid::new_v4().to_string();
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if run["status"] != "awaiting_implementation"
            || !run["pending"].is_null()
            || run["agent_active"] == true
        {
            return Err("run not ready for tool approval".into());
        }
        run["pending"] = serde_json::json!({"id":approval,"tool":"mcp","arguments":{"endpoint":endpoint,"name":name,"arguments":arguments,"cwd":cwd},"call_id":null,"authorized":false});
        run["status"] = "awaiting_approval".into();
        append_event(
            &mut run,
            id,
            "approval_required",
            "MCP 요청에 사용자 승인이 필요합니다.",
        )?;
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"approval_id":approval}))
}

// 2026-11-16: 외부 요청 전에 승인을 내구적으로 소비하여 중단이나 동시 호출에서 재전송하지 않는다.
pub(crate) fn consume_mcp(
    db: &Database,
    id: &str,
    approval: &str,
) -> Result<serde_json::Value, String> {
    consume_mcp_inner(db, id, approval, None)
}

#[cfg(windows)]
pub(crate) fn consume_bound_mcp(
    db: &Database,
    id: &str,
    approval: &str,
    call: &str,
) -> Result<serde_json::Value, String> {
    consume_mcp_inner(db, id, approval, Some(call))
}

fn consume_mcp_inner(
    db: &Database,
    id: &str,
    approval: &str,
    bound: Option<&str>,
) -> Result<serde_json::Value, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let request = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if run["status"] != "approved_mcp"
            || run["pending"]["tool"] != "mcp"
            || run["pending"]["id"].as_str() != Some(approval)
            || run["pending"]["authorized"] != true
        {
            return Err("approval unavailable or already consumed".into());
        }
        if run["pending"]["call_id"].as_str() != bound {
            return Err("approval unavailable or already consumed".into());
        }
        let mut endpoint_stamp = None;
        if let Some(call) = bound {
            use sha2::{Digest, Sha256};
            let key = format!("{id}:{call}");
            let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
            let mut record: serde_json::Value = serde_json::from_str(
                calls
                    .get(key.as_str())
                    .map_err(|e| e.to_string())?
                    .ok_or("call not found")?
                    .value(),
            )
            .map_err(|e| e.to_string())?;
            let role = record["role"].as_str().ok_or("invalid call")?;
            let task = record["task_id"].as_str().ok_or("invalid call")?;
            let hash = format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(role, task, "mcp", &run["pending"]["arguments"]))
                        .map_err(|e| e.to_string())?
                )
            );
            if record["status"] != "waiting"
                || record["approval_id"] != approval
                || record["hash"] != hash
            {
                return Err("call identity conflict".into());
            }
            validate_bound_mcp(&tx, &run, role, task, true)?;
            validate_mcp_stamp(&tx, &run, &record)?;
            endpoint_stamp = Some(record["endpoint_stamp"].clone());
            record["status"] = "executing".into();
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .map_err(|e| e.to_string())?;
        }
        let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
        let row = projects
            .get(run["project_id"].as_str().ok_or("invalid project")?)
            .map_err(|e| e.to_string())?
            .ok_or("project not found")?;
        let project: domain::Project =
            serde_json::from_str(row.value()).map_err(|e| e.to_string())?;
        let spec = project
            .approved
            .as_ref()
            .ok_or("approved specification changed")?;
        if run["spec_hash"].as_str() != Some(spec.hash.as_str())
            || run["spec_version"].as_u64() != Some(spec.version)
            || project.draft_hash != spec.hash
            || run["scope"] != serde_json::json!(spec.scope)
        {
            return Err("approved specification changed".into());
        }
        let mut request = run["pending"]["arguments"].clone();
        if let Some(stamp) = endpoint_stamp {
            request["_endpoint_stamp"] = stamp;
        }
        run["status"] = "executing".into();
        append_event(
            &mut run,
            id,
            "tool_started",
            "승인된 MCP 요청을 시작합니다.",
        )?;
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
        request
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(request)
}

// 2026-11-16: 요청 실패나 취소는 외부 효과의 유무를 단정하지 않고 재실행을 막는다.
pub(crate) fn finish_mcp(
    db: &Database,
    id: &str,
    approval: &str,
    success: bool,
) -> Result<bool, String> {
    finish_mcp_inner(db, id, approval, success, None)
}

#[cfg(windows)]
pub(crate) fn finish_bound_mcp(
    db: &Database,
    id: &str,
    approval: &str,
    call: &str,
    result: Option<serde_json::Value>,
) -> Result<bool, String> {
    finish_mcp_inner(db, id, approval, result.is_some(), Some((call, result)))
}

fn finish_mcp_inner(
    db: &Database,
    id: &str,
    approval: &str,
    success: bool,
    bound: Option<(&str, Option<serde_json::Value>)>,
) -> Result<bool, String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let complete = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        if !["executing", "cancelling"].contains(&run["status"].as_str().unwrap_or(""))
            || run["pending"]["tool"] != "mcp"
            || run["pending"]["id"].as_str() != Some(approval)
        {
            return Err("MCP execution not active".into());
        }
        if run["pending"]["call_id"].as_str() != bound.as_ref().map(|(call, _)| *call) {
            return Err("MCP execution identity changed".into());
        }
        let complete = success && run["status"] == "executing";
        if let Some((call, result)) = bound {
            let key = format!("{id}:{call}");
            let mut calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
            let mut record: serde_json::Value = serde_json::from_str(
                calls
                    .get(key.as_str())
                    .map_err(|e| e.to_string())?
                    .ok_or("call not found")?
                    .value(),
            )
            .map_err(|e| e.to_string())?;
            if record["status"] != "executing" || record["approval_id"] != approval {
                return Err("call identity conflict".into());
            }
            record["status"] = if complete { "completed" } else { "uncertain" }.into();
            if complete {
                record["result"] = result.ok_or("result missing")?;
            }
            let encoded = record.to_string();
            if encoded.len() > MAX_DURABLE_RECORD_BYTES {
                return Err("call result too large".into());
            }
            calls
                .insert(key.as_str(), encoded.as_str())
                .map_err(|e| e.to_string())?;
        }
        run["pending"] = serde_json::Value::Null;
        run["status"] = if complete {
            "awaiting_implementation"
        } else {
            "uncertain"
        }
        .into();
        if !complete {
            run["resources_unknown"] = true.into();
        }
        append_event(
            &mut run,
            id,
            if complete {
                "tool_complete"
            } else {
                "uncertain"
            },
            if complete {
                "승인된 MCP 호출이 완료되었습니다."
            } else {
                "MCP 호출 결과를 확인해야 합니다."
            },
        )?;
        table
            .insert(id, run.to_string().as_str())
            .map_err(|e| e.to_string())?;
        complete
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(complete)
}
