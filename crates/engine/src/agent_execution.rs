// 2026-11-16: 실행 연결 전 사전 검증은 독립 테스트하며 StartAgent 성공 응답을 만들지 않는다.
#[allow(dead_code)]
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
            !["read", "search", "patch"].contains(&t.as_str())
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

#[allow(dead_code)]
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
#[allow(dead_code)]
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
    #[test]
    fn supports_only_explicit_initial_tool_and_validation_contract() {
        let (spec, configs) = fixture();
        assert!(preflight(&spec, &configs, "step", "mock", &[serde_json::json!({})]).is_ok());
        for name in ["exec", "git", "mcp", "knowledge"] {
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
    }
    #[test]
    fn stale_worker_cannot_remove_replacement_cancellation_token() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!("magi-agent-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = Arc::new(crate::Engine::open(&root.join("test.redb")).unwrap());
        let old = register_token(&engine, "run").unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = {
            let engine = Arc::clone(&engine);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                entered.wait();
                release.wait();
                release_token(&engine, "run", &old);
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
    #[test]
    fn concurrent_start_claims_only_one_attempt() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(format!("magi-agent-cas-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = crate::Engine::open(&root.join("test.redb")).unwrap();
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
}
