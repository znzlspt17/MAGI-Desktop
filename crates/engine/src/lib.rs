//! 로컬 프로젝트와 명세 생성, 실행 결과 저장 및 복구를 제공한다.

#[cfg(windows)]
mod agent_completion;
mod agent_execution;
pub mod agent_loop;
// 2026-11-16: StartAgent가 내구 runtime bridge를 통해 실행한다.
mod agent_runtime;
mod completion;
mod knowledge_text;
mod mcp_connections;
mod mcp_runtime;
mod package_export;
mod project_decisions;
mod runs;
mod storage;
pub mod tools;
mod verification;
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
    cancellations: std::sync::Mutex<
        std::collections::HashMap<String, Arc<tokio_util::sync::CancellationToken>>,
    >,
    credentials:
        std::sync::Mutex<std::collections::HashMap<String, (String, domain::SecretString)>>,
}

type PreparedGeneration = (
    String,
    provider::OpenAiProvider,
    provider::sdd::SpecInput,
    tokio_util::sync::CancellationToken,
    domain::SecretString,
    u64,
    Arc<Database>,
    String,
);
// 2026-11-16: 취소 신호와 실제 작업 반환을 분리하고 오류 경로에서도 슬롯을 반환한다.
struct GenerationLease {
    engine: Arc<Engine>,
    run_id: String,
}
impl Drop for GenerationLease {
    fn drop(&mut self) {
        self.engine
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.run_id);
        self.engine
            .slots
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
#[cfg(all(test, windows))]
static GENERATION_HANDOFF_TEST_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
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
                Ok(db) => open_existing_storage(database_path, db, &completions)?,
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
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn handle(&self, request: Request) -> Response {
        // 2026-11-15: 완료 등록과 명세 변경의 순서를 고정하되 조회와 취소는 이 잠금을 기다리지 않는다.
        let guarded = matches!(
            &request.command,
            Command::CreateProject { .. }
                | Command::StartRun { .. }
                | Command::SaveDecision { .. }
                | Command::ReviseDecision { .. }
                | Command::SelectDecision { .. }
                | Command::PutKnowledge { .. }
                | Command::SetKnowledgeEmbedding { .. }
                | Command::SubmitTool { .. }
                | Command::ConfigureMcp { .. }
                | Command::RequestMcpApproval { .. }
                | Command::ApproveTool { .. }
                | Command::SaveDraft { .. }
                | Command::SubmitInterview { .. }
                | Command::ConfigureModel { .. }
                | Command::GenerateSpec { .. }
                | Command::ConfigureAgents { .. }
                | Command::SetAutonomy { .. }
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
        if let Command::ListMcpTools { project_id } = &request.command {
            return Response {
                id: request_id,
                result: mcp_connections::list(Arc::clone(&self), project_id.clone()).await,
            };
        }
        if let Command::CallMcpApproved {
            run_id,
            approval_id,
        } = &request.command
        {
            return Response {
                id: request_id,
                result: mcp_runtime::call(Arc::clone(&self), run_id.clone(), approval_id.clone())
                    .await,
            };
        }
        #[cfg(windows)]
        if let Command::StartAgent {
            run_id,
            mock_script,
        } = &request.command
        {
            return Response {
                id: request_id,
                result: agent_execution::start(
                    Arc::clone(&self),
                    run_id.clone(),
                    mock_script.clone(),
                )
                .await
                .map(|_| ResponseData::Empty),
            };
        }
        if let Command::GenerateSpec { project_id } = &request.command {
            let engine = Arc::clone(&self);
            let project_id = project_id.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let _gate = engine
                    .mutation_gate
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if engine
                    .startup_recovery_pending
                    .load(std::sync::atomic::Ordering::Acquire)
                    || engine.completions.blocks_global()
                    || engine.completions.blocks_project(&project_id)
                {
                    return Err("완료 결과 저장을 먼저 복구해야 합니다.".into());
                }
                engine.prepare_generation(&project_id)
            })
            .await;
            match prepared {
                Ok(Ok(Some((run_id, provider, input, token, secret, context, db, project_id)))) => {
                    let worker_id = run_id.clone();
                    let engine = Arc::clone(&self);
                    tokio::spawn(async move {
                        let _lease = GenerationLease {
                            engine: Arc::clone(&engine),
                            run_id: worker_id.clone(),
                        };
                        let (events, mut rx) = tokio::sync::mpsc::channel(32);
                        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
                        let result = provider::sdd::generate_spec_with_context(
                            &provider,
                            input,
                            token,
                            events,
                            context,
                            provider::DEFAULT_MAX_OUTPUT_TOKENS,
                        )
                        .await
                        .map_err(|_| "provider failed".to_owned())
                        .and_then(|report| {
                            let json = serde_json::to_string(&report)
                                .map_err(|_| "invalid provider output")?;
                            if json.contains(&secret.0) {
                                return Err("provider output rejected".into());
                            }
                            Ok(report)
                        });
                        save_generation_result(
                            Arc::clone(&engine.completions),
                            db,
                            worker_id,
                            project_id,
                            result,
                            |manager, db, record| {
                                #[cfg(test)]
                                if GENERATION_HANDOFF_TEST_FAILURES
                                    .lock()
                                    .unwrap()
                                    .remove(record["run_id"].as_str().unwrap())
                                {
                                    panic!("fake handoff failure");
                                }
                                manager.register_generation_record(db, record)
                            },
                        )
                        .await;
                        let _ = drain.await;
                    });
                    return Response {
                        id: request_id,
                        result: Ok(ResponseData::Run { run_id }),
                    };
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    return Response {
                        id: request_id,
                        result: Err(error),
                    }
                }
                Err(_) => {
                    return Response {
                        id: request_id,
                        result: Err("engine worker failed".into()),
                    }
                }
            }
        }
        tokio::task::spawn_blocking(move || self.handle(request))
            .await
            .unwrap_or_else(|_| Response {
                id: request_id,
                result: Err("engine worker failed".into()),
            })
    }

    fn prepare_generation(&self, project_id: &str) -> Result<Option<PreparedGeneration>, String> {
        let db = self.db.get().ok_or("storage unavailable")?;
        let p: Project = {
            let tx = db.begin_read().map_err(err)?;
            let table = tx.open_table(PROJECTS).map_err(err)?;
            let value = table
                .get(project_id)
                .map_err(err)?
                .ok_or("project not found")?;
            serde_json::from_str(value.value()).map_err(err)?
        };
        let settings = p.model_settings.as_ref().ok_or("configure a model first")?;
        if settings.provider != "openai" {
            return Ok(None);
        }
        if !settings.capabilities.streaming {
            return Err("selected model does not declare streaming capability".into());
        }
        let estimated = (serde_json::to_vec(&(&p.interview, &p.draft))
            .map_err(err)?
            .len() as u64)
            .saturating_add(2048)
            .saturating_add(provider::DEFAULT_MAX_OUTPUT_TOKENS);
        if settings
            .capabilities
            .context_tokens
            .is_some_and(|limit| limit < estimated)
        {
            return Err("configured model context is too small".into());
        }
        if p.interview.is_empty()
            || p.draft.base_revision.trim().is_empty()
            || p.draft.acceptance.is_empty()
        {
            return Err("interview, base revision and acceptance rubric are required".into());
        }
        let (endpoint, secret) = self
            .credentials
            .lock()
            .map_err(|_| "credential storage unavailable")?
            .get(project_id)
            .cloned()
            .ok_or("runtime credential required")?;
        let provider = provider::OpenAiProvider::new(provider::OpenAiConfig {
            endpoint,
            model: settings.model.clone(),
            api_key: secret.0.clone(),
            max_output_bytes: 2 * 1024 * 1024,
        })
        .map_err(|_| "invalid provider configuration")?;
        // 2026-11-15: 평가 항목은 직렬화 왕복 없이 실제 계약 필드로 생성한다.
        let rubric = p
            .draft
            .acceptance
            .iter()
            .enumerate()
            .map(|(index, requirement)| provider::sdd::RubricItem {
                id: format!("criterion-{index}"),
                requirement_id: format!("requirement-{index}"),
                requirement: requirement.clone(),
                importance: "critical".into(),
                allowed_sections: [
                    "## 7. 필수 요구사항",
                    "## 20. 수용 기준",
                    "### 4.1 범위 포함",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            })
            .collect();
        // 2026-11-14: 편집된 평가본을 사용자 근거로 전달해 재평가가 변경 내용을 놓치지 않게 한다.
        let mut answers = p.interview[1..].to_vec();
        if p.assessment
            .as_ref()
            .is_some_and(|a| a.draft_hash != p.draft_hash || a.context_version != p.context_version)
        {
            answers.push(format!(
                "사용자가 편집한 명세 초안: {}",
                serde_json::to_string(&p.draft).map_err(err)?
            ));
        }
        let input = provider::sdd::SpecInput {
            request: p.interview[0].clone(),
            answers,
            decisions: p.draft.reused_decisions.clone(),
            base_revision: p.draft.base_revision.clone(),
            rubric,
            max_revisions: 2,
        };
        self.slots
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| if n < 4 { Some(n + 1) } else { None },
            )
            .map_err(|_| "run capacity reached")?;
        // 2026-11-16: Run이 조회 가능해지기 전에 토큰을 등록해 취소 누락을 막는다.
        let run_id = uuid::Uuid::new_v4().to_string();
        let token = tokio_util::sync::CancellationToken::new();
        self.cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(run_id.clone(), Arc::new(token.clone()));
        if let Err(error) = runs::begin_generation(
            db.as_ref(),
            &run_id,
            project_id,
            &p.draft_hash,
            p.context_version,
        ) {
            self.cancellations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&run_id);
            self.slots.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return Err(error);
        }
        Ok(Some((
            run_id,
            provider,
            input,
            token,
            secret,
            settings.capabilities.context_tokens.unwrap_or(u64::MAX),
            db,
            project_id.to_owned(),
        )))
    }
    fn generation_active(&self, run_id: &str) -> bool {
        self.cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(run_id)
    }
    fn execute(&self, command: Command) -> Result<ResponseData, String> {
        let read_or_control = matches!(
            &command,
            Command::Status
                | Command::SearchKnowledge { .. }
                | Command::ListDecisions { .. }
                | Command::GetGraph { .. }
                | Command::GetGraphPage { .. }
                | Command::SearchKnowledgePage { .. }
                | Command::SearchKnowledgeVector { .. }
                | Command::GetKnowledgeSource { .. }
                | Command::ListStorageFailures
                | Command::RetryCompletionSave { .. }
                | Command::GetProject { .. }
                | Command::ListProjects
                | Command::GetRun { .. }
                | Command::GetDiff { .. }
                | Command::ListRuns { .. }
                | Command::ListEvents { .. }
                | Command::CancelRun { .. }
        );
        let project = match &command {
            Command::StartRun { project_id, .. }
            | Command::SaveDraft { project_id, .. }
            | Command::SubmitInterview { project_id, .. }
            | Command::ConfigureModel { project_id, .. }
            | Command::GenerateSpec { project_id }
            | Command::ConfigureAgents { project_id, .. }
            | Command::SetAutonomy { project_id, .. }
            | Command::ApproveSpec { project_id, .. }
            | Command::ExportPackage { project_id, .. }
            | Command::SaveDecision { project_id, .. }
            | Command::ReviseDecision { project_id, .. }
            | Command::SelectDecision { project_id, .. }
            | Command::ConfigureMcp { project_id, .. }
            | Command::PutKnowledge { project_id, .. }
            | Command::SetKnowledgeEmbedding { project_id, .. } => Some(project_id),
            _ => None,
        };
        let run = match &command {
            Command::StartAgent { run_id, .. }
            | Command::ResumeAgent { run_id, .. }
            | Command::SubmitTool { run_id, .. }
            | Command::ExportRunPackage { run_id, .. }
            | Command::ApproveTool { run_id, .. }
            | Command::RequestMcpApproval { run_id, .. } => Some(run_id),
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
                if state == "pending" && matches!(self.completions.for_run(&run_id)["stage"].as_str(), Some("worktree" | "generation" | "agent")) {
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
            Command::SetKnowledgeEmbedding { project_id, document_id, version, model, vector } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::set_embedding(&db, &project_id, &document_id, version, model, vector)
            }
            Command::SearchKnowledgeVector { project_id, model, vector, offset, limit } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::search_vector(&db, &project_id, model, vector, offset, limit)
            }
            Command::SaveDecision { project_id, decision } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                project_decisions::save_decision(db, project_id, decision)
            }
            Command::ListDecisions { project_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                project_decisions::list_decisions(db, project_id)
            }
            Command::ReviseDecision { project_id, id, text } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                project_decisions::revise_decision(db, project_id, id, text)
            }
            Command::SelectDecision { project_id, id, version } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                project_decisions::select_decision(db, project_id, id, version)
            }
            Command::ExportPackage { project_id, destination } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                package_export::export_project(&db, &project_id, Path::new(&destination))
            }
            Command::ExportRunPackage { run_id, destination } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                package_export::export_run(&db, &run_id, Path::new(&destination))
            }
            Command::GetGraph { project_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::graph(&db, &project_id, None)
            }
            Command::GetGraphPage { project_id, offset, limit } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::graph(&db, &project_id, Some((offset, limit)))
            }
            Command::PutKnowledge { project_id, document } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::put(&db, &project_id, document)
            }
            Command::GetKnowledgeSource { project_id, document_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::source(&db, &project_id, &document_id)
            }
            Command::SearchKnowledgePage { project_id, query, offset, limit } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::search(&db, &project_id, &query, offset, limit)
            }
            Command::SearchKnowledge { project_id, query, semantic } => {
                if semantic { return Err("semantic search unavailable".into()); }
                let db = self.db.get().ok_or("storage unavailable")?;
                knowledge_text::search(&db, &project_id, &query, 0, 100)
            }
            Command::ConfigureMcp { project_id, endpoint } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                mcp_connections::configure(&db, &project_id, &endpoint)
            }
            Command::RequestMcpApproval { run_id, endpoint, name, arguments, cwd } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                Ok(ResponseData::Data(runs::request_mcp(&db, &run_id, endpoint, name, arguments, cwd)?))
            }
            Command::SubmitTool { run_id, tool, arguments } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                Ok(ResponseData::Data(runs::submit_patch(&db, &run_id, tool, arguments)?))
            }
            Command::ApproveTool { run_id, approval_id, approved } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                Ok(ResponseData::Data(runs::approve_patch(&db, &run_id, &approval_id, approved)?))
            }
            Command::CancelRun { run_id } => {
                // 2026-11-16: 저장 실패가 진행 중인 모델 작업의 취소 전달을 막지 않게 한다.
                if let Some(token) = self.cancellations.lock().unwrap_or_else(|e| e.into_inner()).get(&run_id) { token.cancel(); }
                let db = self.db.get().ok_or("storage unavailable")?;
                runs::cancel(&db, &run_id)?;
                Ok(ResponseData::Empty)
            }
            Command::GetDiff { run_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                Ok(ResponseData::Data(runs::diff(&db, &run_id)?))
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
                            run["resources_released"] = (resources_released(run)? && !self.generation_active(&id)).into();
                            run["persistence"] = self.completions.for_run(&id);
                        }
                    }
                }
                Ok(ResponseData::Data(page))
            }
            Command::GetRun { run_id } => {
                let db = self.db.get().ok_or("storage unavailable")?;
                let mut run = runs::get(&db, &run_id)?;
                let released = resources_released(&run)? && !self.generation_active(&run_id);
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
                    "engine ready; worktree preparation, spec generation, agent execution and storage recovery available; agent resume and local inference unavailable"
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
            // 2026-11-16: 연결 자격은 영속 상태와 journal에 넣지 않고 프로세스 메모리에만 둔다.
            Command::SetProviderCredential {project_id,endpoint,credential} => {
                let db=self.db.get().ok_or("storage unavailable")?;let tx=db.begin_read().map_err(err)?;let table=tx.open_table(PROJECTS).map_err(err)?;
                if table.get(project_id.as_str()).map_err(err)?.is_none(){return Err("project not found".into());}
                if endpoint.trim().is_empty() || credential.0.trim().is_empty(){return Err("provider configuration incomplete".into());}
                self.credentials.lock().map_err(|_|"credential storage unavailable")?.insert(project_id,(endpoint,credential));
                Ok(ResponseData::Empty)
            }
            // 2026-11-16: 실제 실행은 연결하지 않고 역할별 설정 계약과 기존 승인 근거를 보존한다.
            Command::ConfigureAgents { project_id, agents } => self.update(&project_id, |p| {
                let roles = ["coordinator", "implementer", "reviewer"];
                if agents.len() != 3 || roles.iter().any(|role| agents.iter().filter(|a| a.role == *role).count() != 1) {
                    return Err("exactly one configuration for each role is required".into());
                }
                for agent in &agents {
                    if agent.model.trim().is_empty()
                        || !(1..=64).contains(&agent.limits.get("max_rounds").and_then(|v| v.as_u64()).unwrap_or(0))
                        || !(1..=1_000_000).contains(&agent.limits.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0))
                    { return Err("agent model and limits required".into()); }
                    if agent.limits.get("max_output_tokens").is_some_and(|v| v.as_u64().is_none_or(|n| !(1..=1_000_000).contains(&n))) {
                        return Err("invalid agent output limit".into());
                    }
                    if agent.tools.iter().any(|tool| !["read", "search", "patch", "exec", "git", "mcp"].contains(&tool.as_str())
                        || (agent.role == "reviewer" && !["read", "search"].contains(&tool.as_str()))) {
                        return Err("invalid role tool".into());
                    }
                }
                let next_context = p.context_version.checked_add(1).ok_or("interview context version exhausted")?;
                p.agent_configs = agents;
                p.context_version = next_context;
                Ok(ResponseData::Project(p.clone()))
            }),
            // 2026-11-16: 자율 모드 변경도 평가 문맥에 반영해 이전 평가의 최신성을 보장한다.
            Command::SetAutonomy { project_id, mode } => self.update(&project_id, |p| {
                if !["step", "approved_scope"].contains(&mode.as_str()) { return Err("invalid autonomy mode".into()); }
                let next_context = p.context_version.checked_add(1).ok_or("interview context version exhausted")?;
                p.autonomy_mode = mode;
                p.context_version = next_context;
                Ok(ResponseData::Project(p.clone()))
            }),
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
            // 2026-11-16: mock은 사용자가 입력한 내용을 명시적 미완성 초안으로만 돌려준다.
            Command::GenerateSpec { project_id } => self.update(&project_id, |project| {
                let settings=project.model_settings.as_ref().ok_or("configure a model first")?;
                if settings.provider!="mock" {return Err("selected provider is not available".into());}
                if project.interview.is_empty(){return Err("interview is empty".into());}
                let draft=SpecDraft {title:format!("[Mock 초안] {}",project.name),requirements:project.interview.clone(),mandatory_questions:vec!["범위와 수용 기준, 기준 Git revision을 확인하세요.".into()],..Default::default()};
                project.draft_hash=draft_hash(&draft)?;project.draft=draft;project.approved=None;
                Ok(ResponseData::Project(project.clone()))
            }),            Command::SubmitInterview {
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

// 2026-11-16: 완료 payload는 저장 작업자 외부에도 보존해 JoinError 때 같은 ID로 재시도한다.
#[cfg(windows)]
async fn save_generation_result<F>(
    manager: Arc<completion::CompletionManager>,
    db: Arc<Database>,
    run_id: String,
    project_id: String,
    result: Result<provider::sdd::SpecResult, String>,
    save: F,
) where
    F: FnOnce(&completion::CompletionManager, &Database, serde_json::Value) + Send + 'static,
{
    let record = completion::CompletionManager::generation_record(&run_id, &project_id, result);
    let worker_record = record.clone();
    let worker_manager = Arc::clone(&manager);
    if tokio::task::spawn_blocking(move || save(&worker_manager, &db, worker_record))
        .await
        .is_err()
        && tokio::task::spawn_blocking(move || manager.preserve_failed_handoff(record))
            .await
            .is_err()
    {
        eprintln!("명세 완료 결과의 저장 진단 작업이 중단되었습니다.");
    }
}
// 2026-11-16: 작업자 반환 오류에도 동일한 완료 ID를 진단에 남기고 저장만 다시 시도한다.
#[cfg(windows)]
pub(crate) async fn save_agent_result(
    manager: Arc<completion::CompletionManager>,
    db: Option<Arc<Database>>,
    record: serde_json::Value,
) {
    save_agent_result_with(manager, db, record, |manager, db, record| match db {
        Some(db) => manager.register_agent_record(&db, record),
        None => manager.preserve_failed_handoff(record),
    })
    .await;
}

#[cfg(windows)]
async fn save_agent_result_with<F>(
    manager: Arc<completion::CompletionManager>,
    db: Option<Arc<Database>>,
    record: serde_json::Value,
    save: F,
) where
    F: FnOnce(&completion::CompletionManager, Option<Arc<Database>>, serde_json::Value)
        + Send
        + 'static,
{
    let worker_record = record.clone();
    let worker_manager = Arc::clone(&manager);
    if tokio::task::spawn_blocking(move || save(&worker_manager, db, worker_record))
        .await
        .is_err()
        && tokio::task::spawn_blocking(move || manager.preserve_failed_handoff(record))
            .await
            .is_err()
    {
        eprintln!("에이전트 완료 결과의 저장 진단 작업이 중단되었습니다.");
    }
}

fn draft_hash(draft: &SpecDraft) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(draft).map_err(err)?)
    ))
}

// 2026-11-16: 복구 중 일반 I/O 실패는 DB 핸들을 공개하지 않고 독립 진단과 재시도를 유지한다.
fn open_existing_storage(
    path: std::path::PathBuf,
    db: Database,
    completions: &completion::CompletionManager,
) -> Result<storage::StorageSlot, String> {
    match check_existing_schema(&db) {
        Err(SchemaError::Invalid) => return Err("unsupported storage schema version".into()),
        Err(SchemaError::Storage) => {
            if !completions.blocked() {
                completions.add_startup_issue(false);
            }
            drop(db);
            return Ok(storage::StorageSlot::empty(path));
        }
        Ok(()) => {}
    }
    completions.replay(&db, None);
    if !completions.blocked() && runs::initialize(&db).is_err() {
        completions.add_startup_issue(false);
        drop(db);
        return Ok(storage::StorageSlot::empty(path));
    }
    Ok(storage::StorageSlot::new(path, db))
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
#[derive(Debug)]
enum SchemaError {
    Invalid,
    Storage,
}
fn schema_table_error(error: redb::TableError) -> SchemaError {
    match error {
        redb::TableError::Storage(_) => SchemaError::Storage,
        _ => SchemaError::Invalid,
    }
}
fn check_existing_schema(db: &Database) -> Result<(), SchemaError> {
    let tx = db.begin_read().map_err(|_| SchemaError::Storage)?;
    let meta = tx.open_table(META).map_err(schema_table_error)?;
    if meta
        .get("schema_version")
        .map_err(|_| SchemaError::Storage)?
        .map(|value| value.value())
        != Some(SCHEMA_VERSION)
    {
        return Err(SchemaError::Invalid);
    }
    tx.open_table(PROJECTS).map_err(schema_table_error)?;
    Ok(())
}
fn validate_existing_schema(db: &Database) -> Result<(), String> {
    check_existing_schema(db).map_err(|error| match error {
        SchemaError::Invalid => "unsupported storage schema version".into(),
        SchemaError::Storage => "storage unavailable".into(),
    })
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

#[cfg(test)]
mod status_guidance_tests {
    use super::*;

    #[test]
    fn ready_status_identifies_execution_and_resume_support() {
        let root = std::env::temp_dir().join(format!("magi-status-guide-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = Engine::open(&root.join("state.redb")).unwrap();
        let response = engine.handle(Request {
            id: "status".into(),
            command: Command::Status,
        });
        match response.result.unwrap() {
            ResponseData::Status { message } => assert_eq!(message, "engine ready; worktree preparation, spec generation, agent execution and storage recovery available; agent resume and local inference unavailable"),
            _ => panic!("expected status"),
        }
        drop(engine);
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-status-guide-"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, windows))]
mod agent_settings_tests {
    use super::*;
    fn request(engine: &Engine, command: Command) -> Result<ResponseData, String> {
        engine
            .handle(Request {
                id: "agent-settings".into(),
                command,
            })
            .result
    }
    fn agents() -> Vec<domain::AgentConfig> {
        ["coordinator", "implementer", "reviewer"]
            .iter()
            .map(|role| domain::AgentConfig {
                role: (*role).into(),
                model: "fixture".into(),
                tools: vec!["read".into()],
                limits: serde_json::json!({"max_rounds":1,"max_tokens":100}),
            })
            .collect()
    }
    #[test]
    fn pending_completion_blocks_only_its_project_and_startup_blocks_both() {
        let root =
            std::env::temp_dir().join(format!("magi-settings-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = Engine::open(&path).unwrap();
        let create = |name: &str| match request(
            &engine,
            Command::CreateProject {
                name: name.into(),
                path: "fixture".into(),
            },
        )
        .unwrap()
        {
            ResponseData::Project(p) => p,
            _ => panic!("expected project"),
        };
        let first = create("first");
        let second = create("second");
        let run_id = uuid::Uuid::new_v4().to_string();
        let db = engine.db.get().unwrap();
        engine.completions.register(&db, &run_id, &first.id, Ok(()));
        drop(db);
        assert!(!engine.completions.for_run(&run_id).is_null());
        let before = serde_json::to_value(&first).unwrap();
        let error = "완료 결과 저장을 먼저 복구해야 합니다.";
        assert_eq!(
            request(
                &engine,
                Command::ConfigureAgents {
                    project_id: first.id.clone(),
                    agents: agents()
                }
            )
            .unwrap_err(),
            error
        );
        assert_eq!(
            request(
                &engine,
                Command::SetAutonomy {
                    project_id: first.id.clone(),
                    mode: "approved_scope".into()
                }
            )
            .unwrap_err(),
            error
        );
        let after = match request(
            &engine,
            Command::GetProject {
                project_id: first.id.clone(),
            },
        )
        .unwrap()
        {
            ResponseData::Project(p) => p,
            _ => panic!("expected project"),
        };
        assert_eq!(serde_json::to_value(after).unwrap(), before);
        assert!(request(
            &engine,
            Command::ConfigureAgents {
                project_id: second.id.clone(),
                agents: agents()
            }
        )
        .is_ok());
        assert!(request(
            &engine,
            Command::SetAutonomy {
                project_id: second.id.clone(),
                mode: "approved_scope".into()
            }
        )
        .is_ok());
        engine.completions.add_startup_issue(false);
        for command in [
            Command::ConfigureAgents {
                project_id: second.id.clone(),
                agents: agents(),
            },
            Command::SetAutonomy {
                project_id: second.id,
                mode: "step".into(),
            },
        ] {
            assert_eq!(request(&engine, command).unwrap_err(), error);
        }
        drop(engine);
        let journal = root.join("state.redb.completion-journal");
        let _ = std::fs::remove_file(journal.join(format!("{run_id}.json")));
        let _ = std::fs::remove_file(journal.join("events.log"));
        let _ = std::fs::remove_dir(journal);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
