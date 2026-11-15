//! 로컬 프로젝트의 생성 및 조회를 제공한다.
//!
//! 실행 복구와 모델 제공자 등 나머지 엔진 기능은 아직 연결되지 않았다.

pub mod agent_loop;
mod completion;
mod runs;
mod storage;
pub mod tools;
mod worktree;

use domain::{ApprovedSpec, Command, Project, Request, Response, ResponseData, SpecDraft};
use redb::{Database, ReadableTable, TableDefinition};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SCHEMA_VERSION: u64 = 1;

pub struct Engine {
    db: storage::StorageSlot,
    run_root: std::path::PathBuf,
    git_gate: Arc<std::sync::Mutex<()>>,
    mutation_gate: Arc<std::sync::Mutex<()>>,
    slots: Arc<std::sync::atomic::AtomicUsize>,
    startup_recovery_pending: std::sync::atomic::AtomicBool,
    completions: Arc<completion::CompletionManager>,
}

impl Engine {
    pub fn open(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(err)?;
        let file_name = path.file_name().ok_or("invalid database path")?;
        let database_path = path
            .canonicalize()
            .unwrap_or_else(|_| parent.join(file_name));
        // 2026-11-15: journal이 있으면 기존 DB만 열고 schema 확인 후 진단을 보존한다.
        let mut journal_path = database_path.as_os_str().to_os_string();
        journal_path.push(".completion-journal");
        let journal_path = std::path::PathBuf::from(journal_path);
        let journal_exists = match std::fs::symlink_metadata(&journal_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(err(error)),
        };
        let mutation_gate = Arc::new(std::sync::Mutex::new(()));
        let completions = if journal_exists {
            completion::CompletionManager::load(&journal_path, Arc::clone(&mutation_gate))
                .unwrap_or_else(|_| {
                    let manager = completion::CompletionManager::empty(
                        journal_path.clone(),
                        Arc::clone(&mutation_gate),
                    );
                    manager.add_startup_issue(true);
                    manager
                })
        } else {
            completion::CompletionManager::empty(journal_path.clone(), Arc::clone(&mutation_gate))
        };
        let db = if journal_exists {
            match Database::open(&database_path) {
                Ok(db) => {
                    validate_existing_schema(&db)?;
                    completions.replay(&db, None);
                    if !completions.blocked() {
                        runs::initialize(&db)?;
                    }
                    storage::StorageSlot::new(database_path, db)
                }
                Err(_) => {
                    if !completions.blocked() {
                        completions.add_startup_issue(false);
                    }
                    storage::StorageSlot::empty(database_path)
                }
            }
        } else {
            let db = Database::create(&database_path).map_err(err)?;
            initialize_schema(&db)?;
            runs::initialize(&db)?;
            storage::StorageSlot::new(database_path, db)
        };
        let startup_pending = completions.blocked();
        Ok(Self {
            db,
            run_root: parent,
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(startup_pending),
            completions: Arc::new(completions),
        })
    }

    pub fn handle(&self, request: Request) -> Response {
        // 2026-11-15: 완료 등록과 명세 변경의 순서를 고정하되 조회와 취소는 이 잠금을 기다리지 않는다.
        let guarded = matches!(
            &request.command,
            Command::CreateProject { .. }
                | Command::StartRun { .. }
                | Command::SaveDraft { .. }
                | Command::SubmitInterview { .. }
                | Command::ConfigureModel { .. }
                | Command::ApproveSpec { .. }
        );
        let _gate = guarded.then(|| self.mutation_gate.lock().unwrap_or_else(|e| e.into_inner()));
        Response {
            id: request.id,
            result: self.execute(request.command),
        }
    }

    // 2026-11-15: redb의 동기 I/O가 비동기 실행기를 막지 않도록 전용 작업 영역에서 실행한다.
    pub async fn handle_async(self: Arc<Self>, request: Request) -> Response {
        let request_id = request.id.clone();
        tokio::task::spawn_blocking(move || self.handle(request))
            .await
            .unwrap_or_else(|_| Response {
                id: request_id,
                result: Err("engine worker failed".into()),
            })
    }

    fn execute(&self, command: Command) -> Result<ResponseData, String> {
        let read_or_control = matches!(
            &command,
            Command::Status
                | Command::ListStorageFailures
                | Command::RetryCompletionSave { .. }
                | Command::GetProject { .. }
                | Command::ListProjects
                | Command::GetRun { .. }
                | Command::ListRuns { .. }
                | Command::ListEvents { .. }
                | Command::CancelRun { .. }
        );
        let project = match &command {
            Command::StartRun { project_id, .. }
            | Command::SaveDraft { project_id, .. }
            | Command::SubmitInterview { project_id, .. }
            | Command::ConfigureModel { project_id, .. }
            | Command::ApproveSpec { project_id, .. } => Some(project_id),
            _ => None,
        };
        let run = match &command {
            Command::StartAgent { run_id, .. }
            | Command::ResumeAgent { run_id, .. }
            | Command::SubmitTool { run_id, .. }
            | Command::ApproveTool { run_id, .. } => Some(run_id),
            _ => None,
        };
        if (!read_or_control
            && (self
                .startup_recovery_pending
                .load(std::sync::atomic::Ordering::Acquire)
                || self.completions.blocks_global()))
            || project.is_some_and(|id| self.completions.blocks_project(id))
            || run.is_some_and(|id| self.completions.blocks_run(id))
        {
            return Err("완료 결과 저장을 먼저 복구해야 합니다.".into());
        }
        match command {
            Command::ListStorageFailures => Ok(ResponseData::Data(self.completions.status())),
            Command::RetryCompletionSave { run_id } => {
                if self.completions.startup_issue(&run_id) {
                    if self.completions.other_pending(&run_id) {
                        return Ok(ResponseData::Data(serde_json::json!({"state":"pending"})));
                    }
                    let db = if let Some(db) = self.db.get() {
                        Ok(db)
                    } else {
                        self.db.reopen_when_idle()
                    };
                    let saved = match db {
                        Ok(db) => validate_existing_schema(&db)
                            .and_then(|_| runs::initialize(&db))
                            .is_ok(),
                        Err(_) => false,
                    };
                    if saved {
                        self.completions.clear_startup(&run_id);
                        self.startup_recovery_pending.store(false,std::sync::atomic::Ordering::Release);
                    }
                    return Ok(ResponseData::Data(
                        serde_json::json!({"state":if saved {"saved"}else{"pending"}}),
                    ));
                }
                let mut state = if let Some(db) = self.db.get() {
                    if validate_existing_schema(&db).is_ok() {self.completions.retry(&db,&run_id)?} else {"pending"}
                } else {"pending"};
                if state == "pending" && self.completions.for_run(&run_id)["stage"] == "worktree" {
                    match self.db.reopen_when_idle() {
                        Ok(db) => {
                            if validate_existing_schema(&db).is_ok() {state=self.completions.retry(&db,&run_id)?;} else {self.completions.note_reopen_issue(&run_id,false);}
                        },
                        Err(storage::ReopenError::Busy) => {
                            self.completions.note_reopen_issue(&run_id, true)
                        }
                        Err(storage::ReopenError::OpenFailed) => {
                            self.completions.note_reopen_issue(&run_id, false)
                        }
                    }
                }
                if state=="saved" || state=="superseded" {self.finish_startup_recovery();}
                Ok(ResponseData::Data(serde_json::json!({"state":state})))
            }
            Command::CancelRun { run_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                runs::cancel(&db, &run_id)?;
                Ok(ResponseData::Empty)
            }
            Command::ListEvents { run_id, after } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let run = runs::get(&db, &run_id)?;
                let events: Vec<domain::RunEvent> =
                    serde_json::from_value(run["events"].clone()).map_err(err)?;
                Ok(ResponseData::Events(
                    events
                        .into_iter()
                        .filter(|event| event.sequence > after)
                        .collect(),
                ))
            }
            Command::ListRuns {
                project_id,
                offset,
                limit,
            } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let mut page = runs::list(&db, &project_id, offset, limit)?;
                if let Some(entries) = page["runs"].as_array_mut() {
                    for run in entries {
                        if let Some(id) = run["id"].as_str().map(str::to_owned) {
                            run["resources_released"] = resources_released(run)?.into();
                            run["persistence"] = self.completions.for_run(&id);
                        }
                    }
                }
                Ok(ResponseData::Data(page))
            }
            Command::GetRun { run_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let mut run = runs::get(&db, &run_id)?;
                let released = resources_released(&run)?;
                let object = run.as_object_mut().ok_or("invalid run")?;
                object.insert("resources_released".into(), released.into());
                object.remove("events");
                object.insert("persistence".into(), self.completions.for_run(&run_id));
                Ok(ResponseData::Data(run))
            }
            // 2026-11-15: 부분 엔진의 준비 상태가 미구현 기능까지 보장하는 것으로 보이지 않게 한다.
            Command::Status => Ok(ResponseData::Status {
                message: if self.completions.blocked() {
                    "engine storage recovery pending"
                } else {
                    "engine ready; worktree preparation and storage recovery available; generation and models unavailable"
                }.into(),
            }),
            Command::CreateProject { name, path } => {
                if name.trim().is_empty() || path.trim().is_empty() {
                    return Err("project name and path required".into());
                }
                let draft = SpecDraft::default();
                let draft_hash = draft_hash(&draft)?;
                let project = Project {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    path,
                    draft,
                    draft_hash,
                    assessment: None,
                    approved: None,
                    approval_version: 0,
                    interview: Vec::new(),
                    model_settings: None,
                    decisions: Vec::new(),
                    approval_history: Vec::new(),
                    context_version: 0,
                    agent_configs: Vec::new(),
                    autonomy_mode: "step".into(),
                };
                let db = self.db.get().ok_or("storage unavailable")?;
                let tx = db.begin_write().map_err(err)?;
                {
                    let mut table = tx.open_table(PROJECTS).map_err(err)?;
                    let json = serde_json::to_string(&project).map_err(err)?;
                    table
                        .insert(project.id.as_str(), json.as_str())
                        .map_err(err)?;
                }
                tx.commit().map_err(err)?;
                Ok(ResponseData::Project(project))
            }
            Command::GetProject { project_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let tx = db.begin_read().map_err(err)?;
                let table = tx.open_table(PROJECTS).map_err(err)?;
                let value = table
                    .get(project_id.as_str())
                    .map_err(err)?
                    .ok_or("project not found")?;
                Ok(ResponseData::Project(
                    serde_json::from_str(value.value()).map_err(err)?,
                ))
            }
            Command::ListProjects => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let tx = db.begin_read().map_err(err)?;
                let table = tx.open_table(PROJECTS).map_err(err)?;
                let mut projects = Vec::new();
                for entry in table.iter().map_err(err)? {
                    let (_, value) = entry.map_err(err)?;
                    projects.push(serde_json::from_str(value.value()).map_err(err)?);
                }
                Ok(ResponseData::Projects(projects))
            }
            Command::StartRun {
                project_id,
                expected_approved_hash,
            } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let run_id = runs::start(
                    db,
                    self.run_root.clone(),
                    Arc::clone(&self.completions),
                    Arc::clone(&self.git_gate),
                    Arc::clone(&self.slots),
                    &project_id,
                    &expected_approved_hash,
                )?;
                Ok(ResponseData::Run { run_id })
            }
            Command::ConfigureModel {
                project_id,
                provider,
                model,
                capabilities,
            } => self.update(&project_id, |project| {
                if !["mock", "openai", "local"].contains(&provider.as_str())
                    || model.trim().is_empty()
                {
                    return Err("unsupported provider or empty model".into());
                }
                // 2026-11-15: 설정 변경 후 기존 평가가 새 문맥의 평가로 오인되지 않게 한다.
                let next_context = project
                    .context_version
                    .checked_add(1)
                    .ok_or("interview context version exhausted")?;
                project.model_settings = Some(domain::ModelSettings {
                    provider,
                    model,
                    capabilities,
                });
                project.context_version = next_context;
                Ok(ResponseData::Project(project.clone()))
            }),
            Command::SubmitInterview {
                project_id,
                message,
            } => self.update(&project_id, |project| {
                if message.trim().is_empty() {
                    return Err("interview message is empty".into());
                }
                // 2026-11-15: 포화되면 입력 변화와 기존 평가의 선후 관계를 구분할 수 없다.
                if project.context_version == u64::MAX {
                    return Err("interview context version exhausted".into());
                }
                project.interview.push(message);
                project.context_version = project.context_version.saturating_add(1);
                project.approved = None;
                Ok(ResponseData::Project(project.clone()))
            }),
            Command::SaveDraft { project_id, draft } => self.update(&project_id, |project| {
                project.draft_hash = draft_hash(&draft)?;
                project.draft = draft;
                project.approved = None;
                Ok(ResponseData::Project(project.clone()))
            }),
            Command::ApproveSpec {
                project_id,
                expected_draft_hash,
                accepted_optional,
            } => self.update(&project_id, |p| {
                if p.draft_hash != expected_draft_hash {
                    return Err("draft changed since review".into());
                }
                // 2026-11-15: 기존 저장소의 포화된 문맥은 평가의 최신성을 증명하지 못한다.
                if p.context_version == u64::MAX {
                    return Err("interview context version exhausted".into());
                }
                // 2026-11-14: 다른 초안이나 입력에 대한 평가를 현재 승인 근거로 재사용하지 않는다.
                if p.assessment.as_ref().is_some_and(|a| {
                    a.draft_hash != p.draft_hash || a.context_version != p.context_version
                }) {
                    return Err("draft requires a new assessment".into());
                }
                let d = &p.draft;
                if !d.mandatory_questions.is_empty() || !d.required_failures.is_empty() {
                    return Err("mandatory questions or required checks remain unresolved".into());
                }
                if d.title.trim().is_empty()
                    || d.base_revision.trim().is_empty()
                    || d.scope.is_empty()
                    || d.requirements.is_empty()
                    || d.acceptance.is_empty()
                    || d.scope
                        .iter()
                        .chain(&d.requirements)
                        .chain(&d.acceptance)
                        .any(|v| v.trim().is_empty())
                {
                    return Err(
                        "title, base revision, scope, requirements and acceptance are required"
                            .into(),
                    );
                }
                provider::sdd::validate_draft_structure(d)
                    .map_err(|_| "incomplete specification structure")?;
                validate_tasks(&d.tasks, &d.scope)?;
                if d.optional_issues
                    .iter()
                    .any(|issue| !accepted_optional.contains(issue))
                {
                    return Err("optional issues require explicit acceptance".into());
                }
                let hash = p.draft_hash.clone();
                // 2026-11-15: 버전 소진 시 이력을 덮거나 0으로 순환하지 않는다.
                p.approval_version = p
                    .approval_version
                    .checked_add(1)
                    .ok_or("approval version exhausted")?;
                let spec = ApprovedSpec {
                    version: p.approval_version,
                    hash,
                    base_revision: d.base_revision.clone(),
                    scope: d.scope.clone(),
                    requirements: d.requirements.clone(),
                    acceptance: d.acceptance.clone(),
                    accepted_optional,
                    markdown: d.markdown.clone(),
                    source_links: d.source_links.clone(),
                    constraints: d.constraints.clone(),
                    tasks: d.tasks.clone(),
                    reused_decisions: d.reused_decisions.clone(),
                    task_bindings: d.task_bindings.clone(),
                };
                p.approval_history.push(spec.clone());
                p.approved = Some(spec.clone());
                Ok(ResponseData::Approved(spec))
            }),
            _ => Err("requested feature is not available".into()),
        }
    }

    fn finish_startup_recovery(&self) {
        use std::sync::atomic::Ordering;
        if !self.startup_recovery_pending.load(Ordering::Acquire) || self.completions.blocked() {
            return;
        }
        let Ok(_gate) = self.mutation_gate.try_lock() else {
            self.completions.add_startup_issue(false);
            return;
        };
        let initialized = self.db.get().is_some_and(|db| {
            validate_existing_schema(&db)
                .and_then(|_| runs::initialize(&db))
                .is_ok()
        });
        if initialized {
            self.startup_recovery_pending
                .store(false, Ordering::Release);
        } else {
            self.completions.add_startup_issue(false);
        }
    }
    // 2026-11-15: 원본과 같이 읽기, 변경, 저장을 하나의 트랜잭션으로 묶는다.
    fn update(
        &self,
        id: &str,
        update: impl FnOnce(&mut Project) -> Result<ResponseData, String>,
    ) -> Result<ResponseData, String> {
        let db = self.db.get().ok_or("storage unavailable")?;
        let tx = db.begin_write().map_err(err)?;
        let result;
        {
            let mut table = tx.open_table(PROJECTS).map_err(err)?;
            let mut project: Project = {
                let value = table.get(id).map_err(err)?.ok_or("project not found")?;
                serde_json::from_str(value.value()).map_err(err)?
            };
            result = update(&mut project)?;
            let json = serde_json::to_string(&project).map_err(err)?;
            table.insert(id, json.as_str()).map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(result)
    }
}

fn draft_hash(draft: &SpecDraft) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(draft).map_err(err)?)
    ))
}

fn resources_released(run: &serde_json::Value) -> Result<bool, String> {
    let status = run["status"].as_str().ok_or("invalid run status")?;
    Ok(!run["agent_active"].as_bool().unwrap_or(false)
        && !run["resources_unknown"].as_bool().unwrap_or(false)
        && [
            "awaiting_implementation",
            "awaiting_approval",
            "approved_exec",
            "approved_mcp",
            "completed",
            "failed",
            "cancelled",
            "generated",
            "stale",
            "interrupted",
        ]
        .contains(&status))
}
fn validate_existing_schema(db: &Database) -> Result<(), String> {
    let tx = db.begin_read().map_err(err)?;
    let meta = tx.open_table(META).map_err(err)?;
    if meta.get("schema_version").map_err(err)?.map(|v| v.value()) != Some(SCHEMA_VERSION) {
        return Err("unsupported storage schema version".into());
    }
    tx.open_table(PROJECTS).map_err(err)?;
    Ok(())
}
fn initialize_schema(db: &Database) -> Result<(), String> {
    let tx = db.begin_write().map_err(err)?;
    {
        let mut meta = tx.open_table(META).map_err(err)?;
        let version = meta
            .get("schema_version")
            .map_err(err)?
            .map(|value| value.value());
        match version {
            Some(SCHEMA_VERSION) => {}
            Some(_) => return Err("unsupported storage schema version".into()),
            None => {
                meta.insert("schema_version", SCHEMA_VERSION).map_err(err)?;
            }
        }
    }
    {
        tx.open_table(PROJECTS).map_err(err)?;
    }
    tx.commit().map_err(err)
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn validate_tasks(tasks: &[domain::TaskSpec], scope: &[String]) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};
    let mut by_id = HashMap::new();
    for t in tasks {
        if t.id.trim().is_empty()
            || by_id.insert(t.id.as_str(), t).is_some()
            || t.owned_paths.is_empty()
            || t.validation.is_empty()
            || t.validation.iter().any(|v| v.trim().is_empty())
        {
            return Err("invalid task definition".into());
        }
        for path in &t.owned_paths {
            let p = std::path::Path::new(path);
            if !worktree::safe_relative(path)
                || p.components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || worktree::protected(&path.replace('\\', "/"))
                || path
                    .split(['/', '\\'])
                    .any(|c| c.eq_ignore_ascii_case(".git"))
            {
                return Err("unsafe task path".into());
            }
            let normalized = path.replace('\\', "/").to_lowercase();
            if !scope.iter().any(|s| {
                let s = s.replace('\\', "/").trim_end_matches('/').to_lowercase();
                !s.is_empty() && (normalized == s || normalized.starts_with(&(s + "/")))
            }) {
                return Err("task path outside approved scope".into());
            }
        }
    }
    let mut done = HashSet::new();
    while done.len() < tasks.len() {
        let before = done.len();
        for task in tasks {
            if task.dependencies.iter().all(|d| done.contains(d.as_str())) {
                done.insert(task.id.as_str());
            }
        }
        if done.len() == before {
            return Err("task dependencies missing or cyclic".into());
        }
    }
    Ok(())
}
