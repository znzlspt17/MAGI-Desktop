use redb::{Database, ReadableTable, TableDefinition};
const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");
const COMPLETIONS: TableDefinition<&str, u8> = TableDefinition::new("completion_commits_v1");

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
