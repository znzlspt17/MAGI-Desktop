use redb::{Database, ReadableTable, TableDefinition};
const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");
const COMPLETIONS: TableDefinition<&str, u8> = TableDefinition::new("completion_commits_v1");
#[cfg(all(test, windows))]
struct DiffTestHook {
    run_id: String,
    reached: std::sync::mpsc::Sender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}
#[cfg(all(test, windows))]
static TEST_DIFF_HOOK: std::sync::Mutex<Option<DiffTestHook>> = std::sync::Mutex::new(None);

fn append_event(
    run: &mut serde_json::Value,
    id: &str,
    kind: &str,
    message: &str,
) -> Result<(), String> {
    let events = run["events"].as_array_mut().ok_or("invalid run events")?;
    let sequence = events.last().map_or(Ok(1), |event| {
        event["sequence"]
            .as_u64()
            .and_then(|v| v.checked_add(1))
            .ok_or("invalid event sequence")
    })?;
    events.push(serde_json::json!({"sequence":sequence,"run_id":id,"attempt":1,"kind":kind,"message":message}));
    Ok(())
}

// 2026-11-15: 완료 기록을 먼저 반영한 후 외부 작업 결과가 불명확한 실행만 중단 상태로 표시한다.
pub(crate) fn initialize(db: &Database) -> Result<(), String> {
    let tx = db.begin_write().map_err(|e| e.to_string())?;
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
            Some("generating") => Some(("cancelled", "작업을 취소했습니다.")),
            Some("preparing") => Some(("cancelling", "취소를 요청했습니다.")),
            Some("awaiting_implementation") => Some(("cancelled", "작업을 취소했습니다.")),
            _ => None,
        };
        if let Some((state, message)) = next {
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
#[cfg(test)]
mod tests {
    use super::*;

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
