use crate::Engine;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
#[cfg(all(test, windows))]
type ReadHook = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(all(test, windows))]
static TEST_AFTER_READ: std::sync::Mutex<Option<ReadHook>> = std::sync::Mutex::new(None);

// 2026-11-16: 후속 Runtime 연결 전 독립 검증 경계를 유지한다. 연결 시 dead_code 예외를 제거한다.
#[allow(dead_code)]
pub(crate) async fn verify(
    engine: Arc<Engine>,
    run_id: String,
    task_id: String,
    command: String,
    verification_id: String,
    expected_attempt: u64,
    cancel: CancellationToken,
) -> Result<domain::VerificationEvidence, String> {
    tokio::task::spawn_blocking(move || {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let _gate = engine
            .mutation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let (path, expected) = command
            .strip_prefix("read_contains:")
            .and_then(|s| s.split_once(':'))
            .ok_or("unsupported validation command")?;
        if verification_id.is_empty() {
            return Err("invalid verification id".into());
        }
        let db = engine.db.get().ok_or("database unavailable")?;
        let run = crate::runs::get(&db, &run_id)?;
        if run["agent_attempt"].as_u64() != Some(expected_attempt)
            || expected_attempt == 0
            || run["agent_active"].as_bool() != Some(true)
            || run["status"] != "awaiting_implementation"
        {
            return Err("stale run".into());
        }
        let project_id = run["project_id"].as_str().ok_or("invalid project")?;
        let project: domain::Project = {
            let tx = db.begin_read().map_err(|e| e.to_string())?;
            let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
            let row = projects
                .get(project_id)
                .map_err(|e| e.to_string())?
                .ok_or("project not found")?;
            serde_json::from_str(row.value()).map_err(|_| "invalid project")?
        };
        let spec = project
            .approved
            .as_ref()
            .ok_or("approved specification changed")?;
        if project.draft_hash != spec.hash
            || run["spec_hash"] != spec.hash
            || run["spec_version"].as_u64() != Some(spec.version)
            || run["revision"] != spec.base_revision
            || run["context_version"].as_u64() != Some(project.context_version)
            || run["scope"] != serde_json::json!(spec.scope)
            || run["agent_configs"] != serde_json::json!(project.agent_configs)
        {
            return Err("approved specification changed".into());
        }
        let task = spec
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .ok_or("task missing")?;
        if !task.validation.iter().any(|v| v == &command) {
            return Err("validation not assigned".into());
        }
        let binding = spec
            .task_bindings
            .iter()
            .find(|b| b.task_id == task_id)
            .ok_or("task binding missing")?;
        let config = project
            .agent_configs
            .iter()
            .find(|c| c.role == "implementer")
            .ok_or("implementer missing")?;
        if !config.tools.iter().any(|tool| tool == "read") {
            return Err("validation read not allowed".into());
        }
        let before = crate::runs::diff(&db, &run_id)?;
        let context = crate::tools::ToolContext {
            worktree: run["worktree"].as_str().ok_or("invalid worktree")?.into(),
            allowed_paths: spec.scope.clone(),
            owned_paths: task.owned_paths.clone(),
            allowlist: config.tools.clone(),
            timeout: std::time::Duration::from_secs(15),
            max_output_bytes: 1024 * 1024,
            side_effect_authorized: false,
        };
        let content = crate::tools::read_checked(&context, path)?;
        if !content.contains(expected) {
            return Err("content did not match".into());
        }
        #[cfg(all(test, windows))]
        {
            let hook = {
                let mut hook = TEST_AFTER_READ.lock().unwrap();
                if hook.as_ref().is_some_and(|entry| entry.0 == run_id) {
                    hook.take()
                } else {
                    None
                }
            };
            if let Some((_, reached, proceed)) = hook {
                reached.send(()).map_err(|_| "test hook failed")?;
                proceed
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|_| "test hook timed out")?;
            }
        }
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let after = crate::runs::diff(&db, &run_id)?;
        if before["worktree_hash"] != after["worktree_hash"]
            || before["current_revision"] != after["current_revision"]
        {
            return Err("worktree changed during verification".into());
        }
        let evidence = domain::VerificationEvidence {
            revision: after["current_revision"]
                .as_str()
                .ok_or("invalid revision")?
                .into(),
            spec_hash: spec.hash.clone(),
            command,
            passed: true,
            output: "content matched".into(),
            verification_id,
            worktree_hash: after["worktree_hash"]
                .as_str()
                .ok_or("invalid snapshot")?
                .into(),
            requirement_ids: binding.requirement_ids.clone(),
            acceptance_ids: binding.acceptance_ids.clone(),
        };
        crate::runs::record_verification(
            &db, &run_id, &run, &project, &evidence, &task_id, &cancel,
        )?;
        Ok(evidence)
    })
    .await
    .map_err(|_| "verification worker failed".to_string())?
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use redb::TableDefinition;
    use std::{
        fs,
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    #[derive(Debug)]
    struct FailingBackend {
        file: redb::backends::FileBackend,
        broken: Arc<std::sync::atomic::AtomicBool>,
    }
    impl redb::StorageBackend for FailingBackend {
        fn len(&self) -> std::io::Result<u64> {
            self.file.len()
        }
        fn read(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
            self.file.read(offset, length)
        }
        fn set_len(&self, length: u64) -> std::io::Result<()> {
            self.file.set_len(length)
        }
        fn sync_data(&self, eventual: bool) -> std::io::Result<()> {
            if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("fixture write failure"));
            }
            self.file.sync_data(eventual)
        }
        fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
            if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("fixture write failure"));
            }
            self.file.write(offset, data)
        }
    }

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .creation_flags(0x08000000)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn real_file_match_records_bound_evidence() {
        let root = std::env::temp_dir().join(format!("magi-verification-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::create_dir(repo.join("src")).unwrap();
        fs::write(repo.join("src/a.txt"), "actual content").unwrap();
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
        let work = root.join("work");
        let target = work
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(work.to_str().unwrap());
        git(
            &repo,
            &["worktree", "add", "--detach", "--", target, &revision],
        );
        let engine = Arc::new(Engine::open(&root.join("fixture.redb")).unwrap());
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let task = serde_json::json!({"id":"task", "dependencies":[], "owned_paths":["src"], "validation":["read_contains:src/a.txt:actual", "read_contains:src/a.txt:wrong", "read_contains:outside.txt:actual"]});
        let config = serde_json::json!({"role":"implementer","model":"fixture","tools":["read"],"limits":{}});
        let spec = serde_json::json!({"version":1,"hash":"fixture","base_revision":revision,"scope":["src"],"requirements":["req"],"acceptance":["accept"],"tasks":[task],"task_bindings":[{"task_id":"task","goal":"goal","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]});
        let project = serde_json::json!({"id":"project","name":"fixture","path":repo,"draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2,"agent_configs":[config]});
        let run = serde_json::json!({"id":run_id,"project_id":"project","spec_hash":"fixture","spec_version":1,"revision":revision,"scope":["src"],"worktree":work,"status":"awaiting_implementation","events":[],"context_version":2,"agent_active":true,"agent_attempt":1,"agent_configs":[config],"evidence":[]});
        {
            let mut table = tx.open_table(crate::PROJECTS).unwrap();
            table
                .insert("project", project.to_string().as_str())
                .unwrap();
        }
        {
            let mut table = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let replace_project = |value: &serde_json::Value| {
            let tx = db.begin_write().unwrap();
            {
                let mut projects = tx.open_table(crate::PROJECTS).unwrap();
                projects
                    .insert("project", value.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
        };
        let mut stale = project.clone();
        stale["context_version"] = 3.into();
        replace_project(&stale);
        assert!(verify(
            Arc::clone(&engine),
            run_id.clone(),
            "task".into(),
            "read_contains:src/a.txt:actual".into(),
            "stale".into(),
            1,
            CancellationToken::new()
        )
        .await
        .is_err());
        replace_project(&project);
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        *TEST_AFTER_READ.lock().unwrap() = Some((run_id.clone(), reached_tx, proceed_rx));
        let worker = tokio::spawn(verify(
            Arc::clone(&engine),
            run_id.clone(),
            "task".into(),
            "read_contains:src/a.txt:actual".into(),
            "racing".into(),
            1,
            CancellationToken::new(),
        ));
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        })
        .await
        .unwrap();
        fs::write(work.join("src/a.txt"), "changed while verifying").unwrap();
        proceed_tx.send(()).unwrap();
        assert_eq!(
            worker.await.unwrap().unwrap_err(),
            "worktree changed during verification"
        );
        fs::write(work.join("src/a.txt"), "actual content").unwrap();
        let replace_run = |value: &serde_json::Value| {
            let tx = db.begin_write().unwrap();
            {
                let mut runs = tx
                    .open_table(TableDefinition::<&str, &str>::new("runs"))
                    .unwrap();
                runs.insert(run_id.as_str(), value.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
        };
        let mut no_read_project = project.clone();
        let mut inactive_run = run.clone();
        inactive_run["agent_active"] = false.into();
        replace_run(&inactive_run);
        assert_eq!(
            verify(
                Arc::clone(&engine),
                run_id.clone(),
                "task".into(),
                "read_contains:src/a.txt:actual".into(),
                "inactive".into(),
                1,
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
            "stale run"
        );
        replace_run(&run);
        no_read_project["agent_configs"][0]["tools"] = serde_json::json!([]);
        let mut no_read_run = run.clone();
        no_read_run["agent_configs"][0]["tools"] = serde_json::json!([]);
        replace_project(&no_read_project);
        replace_run(&no_read_run);
        assert_eq!(
            verify(
                Arc::clone(&engine),
                run_id.clone(),
                "task".into(),
                "read_contains:src/a.txt:actual".into(),
                "no-read".into(),
                1,
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
            "validation read not allowed"
        );
        replace_project(&project);
        replace_run(&run);
        for (task_id, command, attempt) in [
            ("task", "exec_json:{}", 1),
            ("task", "read_contains:src/a.txt:wrong", 1),
            ("task", "read_contains:outside.txt:actual", 1),
            ("missing", "read_contains:src/a.txt:actual", 1),
            ("task", "read_contains:src/a.txt:actual", 2),
        ] {
            assert!(verify(
                Arc::clone(&engine),
                run_id.clone(),
                task_id.into(),
                command.into(),
                "invalid".into(),
                attempt,
                CancellationToken::new()
            )
            .await
            .is_err());
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(verify(
            Arc::clone(&engine),
            run_id.clone(),
            "task".into(),
            "read_contains:src/a.txt:actual".into(),
            "cancelled".into(),
            1,
            cancelled
        )
        .await
        .is_err());
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["evidence"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let evidence = verify(
            Arc::clone(&engine),
            run_id.clone(),
            "task".into(),
            "read_contains:src/a.txt:actual".into(),
            "verify-1".into(),
            1,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(evidence.passed);
        assert_eq!(evidence.requirement_ids, vec!["requirement-0"]);
        assert_eq!(evidence.acceptance_ids, vec!["acceptance-0"]);
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["evidence_contexts"]["verify-1"],
            serde_json::json!({"schema_version":1,"spec_version":1,"context_version":2,"agent_attempt":1,"task_id":"task"})
        );
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["evidence"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // 실패하는 실제 redb backend를 별도 임시 DB에 연결해 commit 실패 후 재개방 상태를 확인한다.
        let failure_path = root.join("failure.redb");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&failure_path)
            .unwrap();
        let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failure_db = redb::Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        let tx = failure_db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            projects
                .insert("project", project.to_string().as_str())
                .unwrap();
        }
        {
            let mut runs = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let expected_run = crate::runs::get(&failure_db, &run_id).unwrap();
        let expected_project: domain::Project = serde_json::from_value(project.clone()).unwrap();
        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(crate::runs::record_verification(
            &failure_db,
            &run_id,
            &expected_run,
            &expected_project,
            &evidence,
            "task",
            &CancellationToken::new()
        )
        .is_err());
        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(failure_db);
        let reopened = redb::Database::open(&failure_path).unwrap();
        assert!(crate::runs::get(&reopened, &run_id).unwrap()["evidence"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(crate::runs::get(&reopened, &run_id)
            .unwrap()
            .get("evidence_contexts")
            .is_none());
        drop(reopened);
        assert!(verify(
            Arc::clone(&engine),
            run_id.clone(),
            "task".into(),
            "read_contains:src/a.txt:actual".into(),
            "verify-1".into(),
            1,
            CancellationToken::new()
        )
        .await
        .is_err());
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["evidence"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(db);
        drop(engine);
        git(&repo, &["worktree", "remove", "--force", target]);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-verification-"));
        fs::remove_dir_all(root).unwrap();
    }
}
