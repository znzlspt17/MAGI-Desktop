//! 승인된 실행 시도의 내구 체크포인트 연결.

use crate::{
    agent_loop::{CheckpointStore, LoopCheckpoint},
    runs, Engine, PROJECTS,
};
use redb::Database;
use std::sync::Arc;

#[derive(Clone)]
struct RunIdentity {
    run_id: String,
    spec_hash: String,
    spec_version: u64,
    context_version: u64,
    attempt: u64,
}

// 2026-11-16: 같은 gate 아래 현재 승인과 시도를 확인한 뒤에만 쓰기를 수행한다.
fn check_identity(db: &Database, identity: &RunIdentity) -> Result<(), String> {
    let run = runs::get(db, &identity.run_id)?;
    let project_id = run["project_id"].as_str().ok_or("invalid run project")?;
    if run["agent_active"].as_bool() != Some(true)
        || run["agent_attempt"].as_u64() != Some(identity.attempt)
        || run["spec_hash"].as_str() != Some(identity.spec_hash.as_str())
        || run["spec_version"].as_u64() != Some(identity.spec_version)
        || run["context_version"].as_u64() != Some(identity.context_version)
        || run["status"].as_str().is_none()
        || matches!(
            run["status"].as_str(),
            Some("cancelled" | "cancelling" | "uncertain" | "failed" | "completed" | "interrupted")
        )
    {
        return Err("run or approved specification changed".into());
    }
    let tx = db.begin_read().map_err(|_| "storage unavailable")?;
    let table = tx.open_table(PROJECTS).map_err(|_| "storage unavailable")?;
    let entry = table
        .get(project_id)
        .map_err(|_| "storage unavailable")?
        .ok_or("project not found")?;
    let project: domain::Project =
        serde_json::from_str(entry.value()).map_err(|_| "invalid project")?;
    if project.draft_hash != identity.spec_hash
        || project.context_version != identity.context_version
        || project
            .approved
            .as_ref()
            .map(|s| (s.hash.as_str(), s.version))
            != Some((identity.spec_hash.as_str(), identity.spec_version))
    {
        return Err("run or approved specification changed".into());
    }
    Ok(())
}

// 2026-11-16: 에이전트의 실행 세대에 귀속된 checkpoint를 보존한다.
pub(crate) struct Store {
    engine: Arc<Engine>,
    identity: RunIdentity,
}

impl CheckpointStore for Store {
    async fn load(&self) -> Result<Option<LoopCheckpoint>, String> {
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            check_identity(&db, &identity)?;
            runs::load_checkpoint(&db, &identity.run_id)?
                .map(|(_, v)| {
                    serde_json::from_value(v).map_err(|_| "invalid saved checkpoint".to_owned())
                })
                .transpose()
        })
        .await
        .map_err(|_| "checkpoint worker failed".to_owned())?
    }
    async fn save(
        &self,
        expected_revision: u64,
        checkpoint: LoopCheckpoint,
    ) -> Result<u64, String> {
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            check_identity(&db, &identity)?;
            if checkpoint.spec_hash != identity.spec_hash
                || checkpoint.spec_version != identity.spec_version
            {
                return Err("checkpoint does not match approved specification".into());
            }
            let value = serde_json::to_value(checkpoint).map_err(|_| "invalid checkpoint")?;
            runs::save_checkpoint(&db, &identity.run_id, expected_revision, value)
        })
        .await
        .map_err(|_| "checkpoint worker failed".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::TableDefinition;
    use serde_json::json;

    fn checkpoint() -> LoopCheckpoint {
        serde_json::from_value(json!({
            "schema_version":1,"revision":0,"spec_hash":"approved","spec_version":1,
            "model_identity":"fixture","config_hash":"config","task_index":0,"role_index":0,
            "round":0,"input":[],"pending_calls":[],"pending_index":0,"evidence":[],
            "completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,
            "role_complete":false,"validation_index":0,"reserved_tokens":0
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn checkpoint_save_rejects_old_attempt_and_preserves_existing_state() {
        let base = std::env::temp_dir().join(format!("magi-bridge-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let engine = Arc::new(Engine::open(&base.join("fixture.redb")).unwrap());
        let spec = domain::ApprovedSpec {
            version: 1,
            hash: "approved".into(),
            base_revision: "fixture".into(),
            scope: vec!["src".into()],
            requirements: vec![],
            acceptance: vec![],
            accepted_optional: vec![],
            markdown: String::new(),
            source_links: vec![],
            constraints: vec![],
            tasks: vec![],
            reused_decisions: vec![],
            task_bindings: vec![],
        };
        let project = domain::Project {
            id: "project".into(),
            name: "fixture".into(),
            path: base.to_string_lossy().into_owned(),
            draft: domain::SpecDraft::default(),
            draft_hash: "approved".into(),
            assessment: None,
            approved: Some(spec),
            approval_version: 1,
            interview: vec![],
            model_settings: None,
            decisions: vec![],
            approval_history: vec![],
            context_version: 3,
            agent_configs: vec![],
            autonomy_mode: String::new(),
        };
        let run = json!({"id":"run","project_id":"project","agent_active":true,"agent_attempt":1,"spec_hash":"approved","spec_version":1,"context_version":3,"status":"awaiting_implementation"});
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(PROJECTS).unwrap();
            projects
                .insert("project", serde_json::to_string(&project).unwrap().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            runs.insert("run", run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let store = Store {
            engine: Arc::clone(&engine),
            identity: RunIdentity {
                run_id: "run".into(),
                spec_hash: "approved".into(),
                spec_version: 1,
                context_version: 3,
                attempt: 1,
            },
        };
        assert_eq!(store.save(0, checkpoint()).await.unwrap(), 1);
        let mut wrong = checkpoint();
        wrong.spec_hash = "different-approval".into();
        assert!(
            store.save(1, wrong).await.is_err(),
            "foreign checkpoint must be rejected"
        );
        assert_eq!(runs::load_checkpoint(&db, "run").unwrap().unwrap().0, 1);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let mut changed = run.clone();
            changed["agent_attempt"] = json!(2);
            table.insert("run", changed.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(
            store.save(1, checkpoint()).await.unwrap_err(),
            "run or approved specification changed"
        );
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let mut changed = run.clone();
            changed["status"] = serde_json::Value::Null;
            table.insert("run", changed.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        assert!(
            store.save(1, checkpoint()).await.is_err(),
            "missing run status must not save"
        );
        let saved = runs::load_checkpoint(&db, "run").unwrap().unwrap();
        assert_eq!(saved.0, 1);
        drop(store);
        drop(db);
        drop(engine);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(base.parent(), Some(temp.as_path()));
        assert!(base
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-bridge-store-"));
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(all(test, windows))]
type ApprovalHook = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(all(test, windows))]
static TEST_AUTO_APPROVAL_HOOK: std::sync::Mutex<Option<ApprovalHook>> =
    std::sync::Mutex::new(None);
// 2026-11-16: StartAgent 연결 시 이 진입점으로 승인과 실행 세대를 고정한다.
pub(crate) struct Runtime {
    engine: Arc<Engine>,
    identity: RunIdentity,
    spec: domain::ApprovedSpec,
}

// 2026-11-16: 현재 승인과 실행 세대에서만 도구와 검증을 실행한다.
impl Runtime {
    pub(crate) fn new(
        engine: Arc<Engine>,
        run_id: String,
        spec: domain::ApprovedSpec,
        context_version: u64,
        expected_attempt: u64,
    ) -> Self {
        Self {
            engine,
            identity: RunIdentity {
                run_id,
                spec_hash: spec.hash.clone(),
                spec_version: spec.version,
                context_version,
                attempt: expected_attempt,
            },
            spec,
        }
    }

    async fn snapshot(&self) -> Result<serde_json::Value, String> {
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            check_identity(&db, &identity)?;
            runs::get(&db, &identity.run_id)
        })
        .await
        .map_err(|_| "runtime worker failed".to_owned())?
    }

    fn context(
        &self,
        run: &serde_json::Value,
        task: &domain::TaskSpec,
        role: &str,
    ) -> Result<crate::tools::ToolContext, String> {
        let configured: Vec<domain::AgentConfig> =
            serde_json::from_value(run["agent_configs"].clone())
                .map_err(|_| "invalid role configuration")?;
        let role = configured
            .iter()
            .find(|a| a.role == role)
            .ok_or("role not configured")?;
        let approved = self
            .spec
            .tasks
            .iter()
            .find(|t| t.id == task.id)
            .ok_or("task not approved")?;
        if serde_json::to_value(approved).map_err(|_| "invalid task")?
            != serde_json::to_value(task).map_err(|_| "invalid task")?
        {
            return Err("task changed".into());
        }
        Ok(crate::tools::ToolContext {
            worktree: run["worktree"].as_str().ok_or("invalid worktree")?.into(),
            allowed_paths: self.spec.scope.clone(),
            owned_paths: task.owned_paths.clone(),
            allowlist: role.tools.clone(),
            timeout: std::time::Duration::from_secs(30),
            max_output_bytes: 1024 * 1024,
            side_effect_authorized: false,
        })
    }

    async fn read_or_search(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        name: &str,
        call_id: &str,
        args: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        use sha2::{Digest, Sha256};
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let run = self.snapshot().await?;
        if run["status"] != "awaiting_implementation" {
            return Err("run not ready".into());
        }
        let ctx = self.context(&run, task, role)?;
        if !ctx.allowlist.iter().any(|v| v == name) {
            return Err("tool not allowed for role".into());
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(role, &task.id, name, &args))
                    .map_err(|_| "invalid tool request")?
            )
        );
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        let key = call_id.to_owned();
        let record = tokio::task::spawn_blocking(move || {
            let _guard = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            check_identity(&db, &identity)?;
            runs::claim_call(&db, &identity.run_id, &key, &digest)
        })
        .await
        .map_err(|_| "runtime worker failed".to_owned())??;
        if record["status"] == "completed" {
            return Ok(record["result"].clone());
        }
        if record["status"] != "planned" {
            return Err("previous tool outcome uncertain".into());
        }
        let value = crate::tools::execute_tool(ctx, name, args, cancel.clone())
            .await?
            .content;
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        let key = call_id.to_owned();
        let saved = value.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let db = engine.db.get().ok_or("storage unavailable")?;
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            check_identity(&db, &identity)?;
            runs::complete_call(&db, &identity.run_id, &key, saved)
        })
        .await
        .map_err(|_| "runtime worker failed".to_owned())??;
        Ok(value)
    }

    async fn patch(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        call_id: &str,
        args: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let mut approved_once = false;
        loop {
            let engine = Arc::clone(&self.engine);
            let identity = self.identity.clone();
            let name = role.to_owned();
            let task_id = task.id.clone();
            let key = call_id.to_owned();
            let input = args.clone();
            let check = cancel.clone();
            let response = tokio::task::spawn_blocking(move || {
                let _guard = engine
                    .mutation_gate
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if check.is_cancelled() {
                    return Err("cancelled".into());
                }
                let db = engine.db.get().ok_or("storage unavailable")?;
                check_identity(&db, &identity)?;
                runs::submit_bound_patch(&db, &identity.run_id, &name, &task_id, &key, input)
            })
            .await
            .map_err(|_| "runtime worker failed".to_owned())??;
            match response["status"].as_str() {
                Some("completed") => return Ok(response["result"].clone()),
                Some("waiting") => {
                    let run = self.snapshot().await?;
                    if run["autonomy_mode"] == "approved_scope" && !approved_once {
                        let approval_id = response["approval_id"]
                            .as_str()
                            .ok_or("approval missing")?
                            .to_owned();
                        let engine = Arc::clone(&self.engine);
                        let run_id = self.identity.run_id.clone();
                        let identity = self.identity.clone();
                        let approval_cancel = cancel.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            #[cfg(all(test, windows))]
                            {
                                let hook = {
                                    let mut hook = TEST_AUTO_APPROVAL_HOOK.lock().unwrap();
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
                            // 2026-11-16: gate 대기 중 취소나 시도 변경이 생기면 승인 효과를 시작하지 않는다.
                            let _guard = engine
                                .mutation_gate
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if approval_cancel.is_cancelled() {
                                return Err("cancelled".into());
                            }
                            let db = engine.db.get().ok_or("storage unavailable")?;
                            check_identity(&db, &identity)?;
                            if approval_cancel.is_cancelled() {
                                return Err("cancelled".into());
                            }
                            engine.execute(domain::Command::ApproveTool {
                                run_id,
                                approval_id,
                                approved: true,
                            })
                        })
                        .await
                        .map_err(|_| "approval worker failed".to_owned())?;
                        result?;
                        approved_once = true;
                    } else {
                        tokio::select! {_=cancel.cancelled()=>return Err("cancelled".into()),_=tokio::time::sleep(std::time::Duration::from_millis(50))=>{}}
                    }
                }
                _ => return Err("invalid patch response".into()),
            }
        }
    }

    async fn reusable_evidence(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<domain::VerificationEvidence>, String> {
        let engine = Arc::clone(&self.engine);
        let identity = self.identity.clone();
        let task = task.clone();
        let command = command.to_owned();
        let id = id.to_owned();
        let spec = self.spec.clone();
        tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let _guard = engine
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let db = engine.db.get().ok_or("storage unavailable")?;
            check_identity(&db, &identity)?;
            let run = runs::get(&db, &identity.run_id)?;
            if run["status"] != "awaiting_implementation" {
                return Err("run not ready".into());
            }
            let Some((row, context)) = crate::evidence::resolve(&run, &id, identity.attempt)?
            else {
                return Ok(None);
            };
            let evidence: domain::VerificationEvidence =
                serde_json::from_value(row.clone()).map_err(|_| "invalid saved evidence")?;
            let approved = spec
                .tasks
                .iter()
                .find(|t| t.id == task.id)
                .ok_or("task not approved")?;
            if serde_json::to_value(approved).map_err(|_| "invalid task")?
                != serde_json::to_value(&task).map_err(|_| "invalid task")?
                || !task.validation.iter().any(|v| v == &command)
            {
                return Err("task changed".into());
            }
            let binding = spec
                .task_bindings
                .iter()
                .find(|b| b.task_id == task.id)
                .ok_or("task binding missing")?;
            if context["schema_version"] != 1
                || context["spec_version"] != identity.spec_version
                || context["context_version"] != identity.context_version
                || context["agent_attempt"] != identity.attempt
                || context["task_id"] != task.id
                || !evidence.passed
                || evidence.spec_hash != identity.spec_hash
                || evidence.command != command
                || evidence.requirement_ids != binding.requirement_ids
                || evidence.acceptance_ids != binding.acceptance_ids
            {
                return Err("saved evidence does not match run".into());
            }
            let diff = runs::diff(&db, &identity.run_id)?;
            if evidence.revision != diff["current_revision"].as_str().unwrap_or("")
                || evidence.worktree_hash != diff["worktree_hash"].as_str().unwrap_or("")
            {
                return Err("saved evidence is stale".into());
            }
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            Ok(Some(evidence))
        })
        .await
        .map_err(|_| "verification worker failed".to_owned())?
    }

    async fn verify_with_id(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<domain::VerificationEvidence, String> {
        if id.is_empty() {
            return Err("invalid verification id".into());
        }
        if let Some(evidence) = self
            .reusable_evidence(task, command, id, cancel.clone())
            .await?
        {
            return Ok(evidence);
        }
        let result = crate::verification::verify(
            Arc::clone(&self.engine),
            self.identity.run_id.clone(),
            task.id.clone(),
            command.to_owned(),
            id.to_owned(),
            self.identity.attempt,
            cancel.clone(),
        )
        .await;
        if matches!(&result,Err(error) if error=="duplicate verification id") {
            return self
                .reusable_evidence(task, command, id, cancel)
                .await?
                .ok_or("saved evidence missing".into());
        }
        result
    }
}

impl Store {
    pub(crate) fn new(
        engine: Arc<Engine>,
        run_id: String,
        spec: domain::ApprovedSpec,
        context_version: u64,
        expected_attempt: u64,
    ) -> Self {
        Self {
            engine,
            identity: RunIdentity {
                run_id,
                spec_hash: spec.hash,
                spec_version: spec.version,
                context_version,
                attempt: expected_attempt,
            },
        }
    }
}

impl crate::agent_loop::ToolDriver for Runtime {
    async fn fresh(&self, spec: &domain::ApprovedSpec) -> Result<(), String> {
        if serde_json::to_value(spec).map_err(|_| "invalid spec")?
            != serde_json::to_value(&self.spec).map_err(|_| "invalid spec")?
        {
            return Err("approved specification changed".into());
        }
        self.snapshot().await.map(|_| ())
    }
    async fn execute(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        name: &str,
        call_id: &str,
        args: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        crate::agent_loop::DurableToolDriver::execute_durable(
            self, role, task, name, call_id, args, cancel,
        )
        .await
    }
    async fn verify(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<domain::VerificationEvidence, String> {
        self.verify_with_id(task, command, &uuid::Uuid::new_v4().to_string(), cancel)
            .await
    }
}

impl crate::agent_loop::DurableToolDriver for Runtime {
    async fn execute_durable(
        &self,
        role: &str,
        task: &domain::TaskSpec,
        name: &str,
        call_id: &str,
        args: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        if !["read", "search", "patch"].contains(&name) {
            return Err("unsupported durable tool".into());
        }
        if call_id.is_empty() {
            return Err("invalid call id".into());
        }
        if name == "patch" {
            self.patch(role, task, call_id, args, cancel).await
        } else {
            self.read_or_search(role, task, name, call_id, args, cancel)
                .await
        }
    }
    async fn verify_durable(
        &self,
        task: &domain::TaskSpec,
        command: &str,
        verification_id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<domain::VerificationEvidence, String> {
        self.verify_with_id(task, command, verification_id, cancel)
            .await
    }
}

#[cfg(all(test, windows))]
mod bridge_integration_tests {
    use super::*;
    use crate::agent_loop::{self, DurableToolDriver, LoopConfig, ModelDriver, RoleConfig};
    use redb::ReadableTable;
    use serde_json::{json, Value};
    use std::{
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio_util::sync::CancellationToken;

    fn git(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .creation_flags(0x08000000)
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    struct Fixture {
        root: PathBuf,
        work: PathBuf,
        engine: Arc<Engine>,
        run_id: String,
        spec: domain::ApprovedSpec,
    }
    impl Fixture {
        fn new(mode: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("magi-bridge-integration-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let repo = root.join("repo");
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(repo.join("src/a.txt"), "before").unwrap();
            git(&repo, &["init", "-q"]);
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
            let rev = git(&repo, &["rev-parse", "HEAD"]);
            let work = root.join("work");
            let target = work
                .to_str()
                .unwrap()
                .strip_prefix(r"\\?\")
                .unwrap_or(work.to_str().unwrap());
            git(&repo, &["worktree", "add", "--detach", "--", target, &rev]);
            let task = domain::TaskSpec {
                id: "task".into(),
                dependencies: vec![],
                owned_paths: vec!["src".into()],
                validation: vec!["read_contains:src/a.txt:after".into()],
            };
            let spec = domain::ApprovedSpec {
                version: 1,
                hash: "fixture".into(),
                base_revision: rev.clone(),
                scope: vec!["src".into()],
                requirements: vec!["req".into()],
                acceptance: vec!["accept".into()],
                accepted_optional: vec![],
                markdown: String::new(),
                source_links: vec![],
                constraints: vec![],
                tasks: vec![task],
                reused_decisions: vec![],
                task_bindings: vec![domain::TaskBinding {
                    task_id: "task".into(),
                    goal: "goal".into(),
                    requirement_ids: vec!["requirement-0".into()],
                    acceptance_ids: vec!["acceptance-0".into()],
                }],
            };
            let roles = vec!["coordinator", "implementer", "reviewer"]
                .into_iter()
                .map(|role| domain::AgentConfig {
                    role: role.into(),
                    model: "fixture".into(),
                    tools: if role == "implementer" {
                        vec!["read".into(), "patch".into()]
                    } else {
                        vec!["read".into()]
                    },
                    limits: json!({}),
                })
                .collect::<Vec<_>>();
            let project = domain::Project {
                id: "project".into(),
                name: "fixture".into(),
                path: repo.to_string_lossy().into_owned(),
                draft: domain::SpecDraft::default(),
                draft_hash: "fixture".into(),
                assessment: None,
                approved: Some(spec.clone()),
                approval_version: 1,
                interview: vec![],
                model_settings: None,
                decisions: vec![],
                approval_history: vec![],
                context_version: 2,
                agent_configs: roles.clone(),
                autonomy_mode: mode.into(),
            };
            let run_id = uuid::Uuid::new_v4().to_string();
            let run = json!({"id":run_id,"project_id":"project","spec_hash":"fixture","spec_version":1,"revision":rev,"scope":["src"],"worktree":work,"status":"awaiting_implementation","events":[],"context_version":2,"agent_active":true,"agent_attempt":1,"agent_configs":roles,"autonomy_mode":mode,"evidence":[]});
            let engine = Arc::new(Engine::open(&root.join("state.redb")).unwrap());
            let db = engine.db.get().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut projects = tx.open_table(PROJECTS).unwrap();
                projects
                    .insert("project", serde_json::to_string(&project).unwrap().as_str())
                    .unwrap();
                let mut runs = tx
                    .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                    .unwrap();
                runs.insert(run_id.as_str(), run.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
            drop(db);
            Self {
                root,
                work,
                engine,
                run_id,
                spec,
            }
        }
        fn runtime(&self) -> Runtime {
            Runtime::new(
                Arc::clone(&self.engine),
                self.run_id.clone(),
                self.spec.clone(),
                2,
                1,
            )
        }
        fn store(&self) -> Store {
            Store::new(
                Arc::clone(&self.engine),
                self.run_id.clone(),
                self.spec.clone(),
                2,
                1,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let temp = std::env::temp_dir().canonicalize().unwrap();
            assert_eq!(self.root.parent(), Some(temp.as_path()));
            assert!(self
                .root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("magi-bridge-integration-"));
            if let Some(engine) = Arc::get_mut(&mut self.engine) {
                engine.db = crate::storage::StorageSlot::empty(self.root.join("closed.redb"));
            }
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
    struct Model(AtomicUsize);

    #[tokio::test]
    async fn completed_patch_before_checkpoint_advance_is_reused_after_resume() {
        let mut fixture = Fixture::new("step_by_step");
        let db = fixture.engine.db.get().unwrap();
        let logical_id = serde_json::to_string(&("task", "implementer", 0, "patch-1")).unwrap();
        let args = json!({"path":"src/new.txt","content":"fixture"});
        let pending = runs::submit_bound_patch(
            &db,
            &fixture.run_id,
            "implementer",
            "task",
            &logical_id,
            args.clone(),
        )
        .unwrap();
        let approval = pending["approval_id"].as_str().unwrap().to_owned();
        let run = runs::get(&db, &fixture.run_id).unwrap();
        let checkpoint = json!({"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":"model","config_hash":"config","task_index":0,"role_index":1,"round":0,"input":[],"pending_calls":[{"call_id":"patch-1","name":"patch","arguments":args}],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert(fixture.run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(fixture.run_id.as_str(), checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(!fixture.work.join("src/new.txt").exists());
        let mut inactive = run.clone();
        inactive["agent_active"] = false.into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert(fixture.run_id.as_str(), inactive.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let denied = fixture
            .engine
            .handle(domain::Request {
                id: "approve-inactive".into(),
                command: domain::Command::ApproveTool {
                    run_id: fixture.run_id.clone(),
                    approval_id: approval.clone(),
                    approved: true,
                },
            })
            .result;
        assert!(denied.is_err());
        assert!(!fixture.work.join("src/new.txt").exists());
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert(fixture.run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let result = fixture
            .engine
            .handle(domain::Request {
                id: "approve-active".into(),
                command: domain::Command::ApproveTool {
                    run_id: fixture.run_id.clone(),
                    approval_id: approval,
                    approved: true,
                },
            })
            .result;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/new.txt")).unwrap(),
            "fixture"
        );
        drop(db);
        Arc::get_mut(&mut fixture.engine).unwrap().db =
            crate::storage::StorageSlot::empty(fixture.root.join("closed.redb"));
        fixture.engine = Arc::new(Engine::open(&fixture.root.join("state.redb")).unwrap());
        let db = fixture.engine.db.get().unwrap();
        let before_resume = runs::get(&db, &fixture.run_id).unwrap();
        assert_eq!(before_resume["agent_active"], false);
        assert_eq!(before_resume["status"], "interrupted");
        let mut mismatch = checkpoint.clone();
        mismatch["pending_calls"][0]["arguments"]["content"] = "other".into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(fixture.run_id.as_str(), mismatch.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(
            runs::begin_resume(&db, &fixture.run_id, &before_resume, "model", "config").is_err()
        );
        assert_eq!(runs::get(&db, &fixture.run_id).unwrap(), before_resume);
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(fixture.run_id.as_str(), checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let key = format!("{}:{logical_id}", fixture.run_id);
        let original_record: Value = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap()
        };
        for (field, value) in [
            ("approval_id", json!("different")),
            ("role", json!("reviewer")),
            ("origin_agent_attempt", json!(2)),
            ("status", json!("uncertain")),
        ] {
            let mut altered = original_record.clone();
            altered[field] = value;
            let tx = db.begin_write().unwrap();
            {
                tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                    .unwrap()
                    .insert(key.as_str(), altered.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
            assert!(
                runs::begin_resume(&db, &fixture.run_id, &before_resume, "model", "config")
                    .is_err()
            );
        }
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap()
                .insert(key.as_str(), original_record.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let resumed =
            runs::begin_resume(&db, &fixture.run_id, &before_resume, "model", "config").unwrap();
        assert_eq!(resumed["agent_attempt"], 2);
        let before_record: Value = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap()
        };
        assert_eq!(before_record["origin_agent_attempt"], 1);
        std::fs::write(fixture.work.join("src/new.txt"), "preserve").unwrap();
        let runtime = Runtime::new(
            Arc::clone(&fixture.engine),
            fixture.run_id.clone(),
            fixture.spec.clone(),
            2,
            2,
        );
        let result = runtime
            .execute_durable(
                "implementer",
                &fixture.spec.tasks[0],
                "patch",
                &logical_id,
                args,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/new.txt")).unwrap(),
            "preserve"
        );
        let after_record: Value = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap()
        };
        assert_eq!(after_record, before_record);
    }

    #[tokio::test]
    async fn resume_waiting_patch_keeps_original_approval_and_runs_only_after_user_accepts() {
        let fixture = Fixture::new("step_by_step");
        let db = fixture.engine.db.get().unwrap();
        let logical_id = serde_json::to_string(&("task", "implementer", 0, "patch-1")).unwrap();
        let arguments = json!({"path":"src/new.txt","content":"fixture"});
        let pending = runs::submit_bound_patch(
            &db,
            &fixture.run_id,
            "implementer",
            "task",
            &logical_id,
            arguments.clone(),
        )
        .unwrap();
        let approval = pending["approval_id"].as_str().unwrap().to_owned();
        let mut saved = runs::get(&db, &fixture.run_id).unwrap();
        saved["agent_active"] = false.into();
        let checkpoint = json!({"schema_version":1,"revision":1,"spec_hash":"fixture","spec_version":1,"model_identity":"model","config_hash":"config","task_index":0,"role_index":1,"round":0,"input":[],"pending_calls":[{"call_id":"patch-1","name":"patch","arguments":arguments}],"pending_index":0,"evidence":[],"completed_tasks":[],"used_calls":[],"tokens_used":0,"model_inflight":false,"role_complete":false,"validation_index":0,"reserved_tokens":0});
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert(fixture.run_id.as_str(), saved.to_string().as_str())
                .unwrap();
        }
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(fixture.run_id.as_str(), checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let saved = runs::get(&db, &fixture.run_id).unwrap();
        let set_run = |value: &Value| {
            let tx = db.begin_write().unwrap();
            {
                tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                    .unwrap()
                    .insert(fixture.run_id.as_str(), value.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
        };
        for field in ["id", "call_id", "arguments"] {
            let mut changed = saved.clone();
            changed["pending"][field] = json!("mismatch");
            set_run(&changed);
            assert!(runs::begin_resume(&db, &fixture.run_id, &changed, "model", "config").is_err());
        }
        let mut unknown = saved.clone();
        unknown["resources_unknown"] = true.into();
        set_run(&unknown);
        assert!(runs::begin_resume(&db, &fixture.run_id, &unknown, "model", "config").is_err());
        set_run(&saved);
        let key = format!("{}:{logical_id}", fixture.run_id);
        let original_record: Value = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap()
        };
        for invalid in ["planned", "uncertain", "completed"] {
            let mut altered = original_record.clone();
            altered["status"] = invalid.into();
            let tx = db.begin_write().unwrap();
            {
                tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                    .unwrap()
                    .insert(key.as_str(), altered.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
            assert!(runs::begin_resume(&db, &fixture.run_id, &saved, "model", "config").is_err());
        }
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap()
                .insert(key.as_str(), original_record.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let mut wrong_checkpoint = checkpoint.clone();
        wrong_checkpoint["pending_calls"][0]["arguments"]["content"] = "other".into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(
                    fixture.run_id.as_str(),
                    wrong_checkpoint.to_string().as_str(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(runs::begin_resume(&db, &fixture.run_id, &saved, "model", "config").is_err());
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("checkpoints"))
                .unwrap()
                .insert(fixture.run_id.as_str(), checkpoint.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                let id = fixture.run_id.clone();
                let expected = saved.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    runs::begin_resume(&db, &id, &expected, "model", "config")
                })
            })
            .collect();
        barrier.wait();
        let mut results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter_map(Result::ok);
        let resumed = results.next().expect("resume claim missing");
        assert!(results.next().is_none());
        assert_eq!(resumed["pending"]["id"], approval);
        assert_eq!(resumed["agent_attempt"], 2);
        assert!(!fixture.work.join("src/new.txt").exists());
        let runtime = Runtime::new(
            Arc::clone(&fixture.engine),
            fixture.run_id.clone(),
            fixture.spec.clone(),
            2,
            2,
        );
        let work = runtime.execute_durable(
            "implementer",
            &fixture.spec.tasks[0],
            "patch",
            &logical_id,
            arguments,
            CancellationToken::new(),
        );
        let approve = async {
            tokio::task::yield_now().await;
            assert!(!fixture.work.join("src/new.txt").exists());
            fixture
                .engine
                .handle(domain::Request {
                    id: "approve".into(),
                    command: domain::Command::ApproveTool {
                        run_id: fixture.run_id.clone(),
                        approval_id: approval,
                        approved: true,
                    },
                })
                .result
                .unwrap();
        };
        let (result, _) = tokio::join!(work, approve);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/new.txt")).unwrap(),
            "fixture"
        );
        let after = runs::get(&db, &fixture.run_id).unwrap();
        let record: Value = {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            serde_json::from_str(
                table
                    .get(format!("{}:{logical_id}", fixture.run_id).as_str())
                    .unwrap()
                    .unwrap()
                    .value(),
            )
            .unwrap()
        };
        assert_eq!(record["origin_agent_attempt"], 1);
        assert_eq!(record["effect_agent_attempt"], 2);
        assert!(after["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "tool_started" && event["attempt"] == 2));
        let mut interrupted = after.clone();
        interrupted["agent_active"] = false.into();
        interrupted["status"] = "interrupted".into();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap()
                .insert(fixture.run_id.as_str(), interrupted.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let interrupted = runs::get(&db, &fixture.run_id).unwrap();
        let next =
            runs::begin_resume(&db, &fixture.run_id, &interrupted, "model", "config").unwrap();
        assert_eq!(next["agent_attempt"], 3);
        std::fs::write(fixture.work.join("src/new.txt"), "preserve").unwrap();
        let runtime = Runtime::new(
            Arc::clone(&fixture.engine),
            fixture.run_id.clone(),
            fixture.spec.clone(),
            2,
            3,
        );
        runtime
            .execute_durable(
                "implementer",
                &fixture.spec.tasks[0],
                "patch",
                &logical_id,
                json!({"path":"src/new.txt","content":"fixture"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/new.txt")).unwrap(),
            "preserve"
        );
    }
    impl ModelDriver for Model {
        async fn generate(
            &self,
            role: &str,
            _: Value,
            _: Vec<Value>,
            _: CancellationToken,
            _: u64,
        ) -> Result<provider::Generation, String> {
            if role == "implementer" && self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(provider::Generation {
                    text: String::new(),
                    output: vec![],
                    usage: Some(json!({"total_tokens":1})),
                    tool_calls: vec![provider::ToolCall {
                        call_id: "patch-1".into(),
                        name: "patch".into(),
                        arguments: r#"{"path":"src/a.txt","content":"after"}"#.into(),
                    }],
                });
            }
            Ok(provider::Generation {
                text: if role == "reviewer" {
                    r#"{"approved":true}"#.into()
                } else {
                    "done".into()
                },
                output: vec![],
                usage: Some(json!({"total_tokens":1})),
                tool_calls: vec![],
            })
        }
    }
    fn config() -> LoopConfig {
        let role = |name: &str, tools: Vec<Value>| RoleConfig {
            name: name.into(),
            tools,
            max_rounds: 3,
        };
        LoopConfig {
            coordinator: role("coordinator", vec![]),
            implementer: role("implementer", vec![json!({"name":"patch"})]),
            reviewer: role("reviewer", vec![]),
            max_tokens: 100000,
            max_parallel_tasks: 1,
            timeout: Duration::from_secs(20),
        }
    }
    #[tokio::test]
    async fn checkpoint_loop_applies_patch_once_and_reuses_real_evidence() {
        let fixture = Fixture::new("approved_scope");
        let runtime = fixture.runtime();
        let store = fixture.store();
        agent_loop::run_checkpointed_loop(
            &Model(AtomicUsize::new(0)),
            &runtime,
            &store,
            &fixture.spec,
            &config(),
            "fixture-model",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            "after"
        );
        let run = runs::get(&fixture.engine.db.get().unwrap(), &fixture.run_id).unwrap();
        let entries = run["evidence"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let id = entries[0]["verification_id"].as_str().unwrap();
        let evidence = runtime
            .verify_durable(
                &fixture.spec.tasks[0],
                &fixture.spec.tasks[0].validation[0],
                id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(evidence.passed);
        let refreshed = runs::get(&fixture.engine.db.get().unwrap(), &fixture.run_id).unwrap();
        assert_eq!(refreshed["evidence"].as_array().unwrap().len(), 1);
        let call_id = {
            let db = fixture.engine.db.get().unwrap();
            let tx = db.begin_read().unwrap();
            let calls = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            let prefix = format!("{}:", fixture.run_id);
            calls
                .iter()
                .unwrap()
                .find_map(|row| {
                    let (key, value) = row.unwrap();
                    let record: Value = serde_json::from_str(value.value()).unwrap();
                    (record["role"] == "implementer"
                        && record["task_id"] == "task"
                        && record["status"] == "completed")
                        .then(|| key.value().strip_prefix(&prefix).unwrap().to_owned())
                })
                .unwrap()
        };
        std::fs::write(fixture.work.join("src/a.txt"), "later").unwrap();
        let replay = runtime
            .execute_durable(
                "implementer",
                &fixture.spec.tasks[0],
                "patch",
                &call_id,
                json!({"path":"src/a.txt","content":"after"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(replay["status"], "applied");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            "later"
        );
    }
    #[tokio::test]
    async fn denied_patch_and_unsupported_calls_do_not_modify_worktree() {
        let fixture = Fixture::new("step_by_step");
        let runtime = fixture.runtime();
        let token = CancellationToken::new();
        for name in ["exec", "mcp", "git", "knowledge"] {
            assert_eq!(
                runtime
                    .execute_durable(
                        "implementer",
                        &fixture.spec.tasks[0],
                        name,
                        "unsupported",
                        json!({}),
                        token.clone()
                    )
                    .await
                    .unwrap_err(),
                "unsupported durable tool"
            );
        }
        let task = fixture.spec.tasks[0].clone();
        let work = runtime.execute_durable(
            "implementer",
            &task,
            "patch",
            "denied-call",
            json!({"path":"src/a.txt","content":"after"}),
            token.clone(),
        );
        let deny = async {
            loop {
                let run = runs::get(&fixture.engine.db.get().unwrap(), &fixture.run_id).unwrap();
                if let Some(id) = run["pending"]["id"].as_str() {
                    fixture
                        .engine
                        .handle(domain::Request {
                            id: "deny".into(),
                            command: domain::Command::ApproveTool {
                                run_id: fixture.run_id.clone(),
                                approval_id: id.into(),
                                approved: false,
                            },
                        })
                        .result
                        .unwrap();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (result, _) =
            tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(work, deny) })
                .await
                .unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            "before"
        );
    }

    #[tokio::test]
    async fn saved_evidence_from_other_attempt_is_never_reused() {
        let fixture = Fixture::new("approved_scope");
        let runtime = fixture.runtime();
        std::fs::write(fixture.work.join("src/a.txt"), "after").unwrap();
        let task = &fixture.spec.tasks[0];
        let command = &task.validation[0];
        runtime
            .verify_durable(task, command, "verification-1", CancellationToken::new())
            .await
            .unwrap();
        let db = fixture.engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let mut run: Value = {
                let v = table.get(fixture.run_id.as_str()).unwrap().unwrap();
                serde_json::from_str(v.value()).unwrap()
            };
            run["evidence_contexts_by_attempt"]["1"]["verification-1"]["agent_attempt"] = json!(2);
            table
                .insert(fixture.run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(runtime
            .verify_durable(task, command, "verification-1", CancellationToken::new())
            .await
            .is_err());
        let saved = runs::get(&db, &fixture.run_id).unwrap();
        assert_eq!(saved["evidence"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelled_request_and_unavailable_storage_never_launch_effects() {
        let mut fixture = Fixture::new("approved_scope");
        let runtime = fixture.runtime();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            runtime
                .execute_durable(
                    "implementer",
                    &fixture.spec.tasks[0],
                    "patch",
                    "cancelled",
                    json!({"path":"src/a.txt","content":"after"}),
                    cancel
                )
                .await
                .unwrap_err(),
            "cancelled"
        );
        drop(runtime);
        Arc::get_mut(&mut fixture.engine).unwrap().db =
            crate::storage::StorageSlot::empty(fixture.root.join("missing.redb"));
        let runtime = fixture.runtime();
        assert_eq!(
            runtime
                .execute_durable(
                    "implementer",
                    &fixture.spec.tasks[0],
                    "patch",
                    "storage",
                    json!({"path":"src/a.txt","content":"after"}),
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            "storage unavailable"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            "before"
        );
    }
    #[tokio::test]
    async fn uncertain_patch_record_is_not_reexecuted() {
        use sha2::{Digest, Sha256};
        let fixture = Fixture::new("approved_scope");
        let runtime = fixture.runtime();
        let args = json!({"path":"src/a.txt","content":"after"});
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&("implementer", "task", "patch", &args)).unwrap())
        );
        let db = fixture.engine.db.get().unwrap();
        runs::claim_call(&db, &fixture.run_id, "uncertain-call", &hash).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut calls = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("calls"))
                .unwrap();
            let key = format!("{}:uncertain-call", fixture.run_id);
            let mut record: Value = {
                let row = calls.get(key.as_str()).unwrap().unwrap();
                serde_json::from_str(row.value()).unwrap()
            };
            record["status"] = "uncertain".into();
            calls
                .insert(key.as_str(), record.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let error = runtime
            .execute_durable(
                "implementer",
                &fixture.spec.tasks[0],
                "patch",
                "uncertain-call",
                args,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, "previous tool outcome uncertain");
        assert_eq!(
            std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            "before"
        );
    }
    #[tokio::test]
    async fn queued_auto_approval_rechecks_cancel_and_attempt_before_file_effect() {
        let mut outcomes = Vec::new();
        for change_attempt in [false, true] {
            let fixture = Fixture::new("approved_scope");
            let runtime = fixture.runtime();
            let token = CancellationToken::new();
            let (reached_tx, reached_rx) = std::sync::mpsc::channel();
            let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
            *TEST_AUTO_APPROVAL_HOOK.lock().unwrap() =
                Some((fixture.run_id.clone(), reached_tx, proceed_rx));
            let task = &fixture.spec.tasks[0];
            let work = runtime.execute_durable(
                "implementer",
                task,
                "patch",
                "race-call",
                json!({"path":"src/a.txt","content":"after"}),
                token.clone(),
            );
            let controller = async {
                tokio::task::spawn_blocking(move || {
                    reached_rx.recv_timeout(Duration::from_secs(10)).unwrap()
                })
                .await
                .unwrap();
                if change_attempt {
                    let engine = Arc::clone(&fixture.engine);
                    let id = fixture.run_id.clone();
                    tokio::task::spawn_blocking(move || {
                        let _gate = engine.mutation_gate.lock().unwrap();
                        let db = engine.db.get().unwrap();
                        let tx = db.begin_write().unwrap();
                        {
                            let mut table = tx
                                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                                .unwrap();
                            let mut run: Value = {
                                let row = table.get(id.as_str()).unwrap().unwrap();
                                serde_json::from_str(row.value()).unwrap()
                            };
                            run["agent_attempt"] = json!(2);
                            table.insert(id.as_str(), run.to_string().as_str()).unwrap();
                        }
                        tx.commit().unwrap();
                    })
                    .await
                    .unwrap();
                } else {
                    token.cancel()
                }
                proceed_tx.send(()).unwrap();
            };
            let (result, _) = tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(work, controller)
            })
            .await
            .unwrap();
            outcomes.push((
                change_attempt,
                result.is_err(),
                std::fs::read_to_string(fixture.work.join("src/a.txt")).unwrap(),
            ));
        }
        assert!(
            outcomes
                .iter()
                .all(|(_, error, content)| *error && content == "before"),
            "outcomes: {outcomes:?}"
        );
    }
}
