//! 로컬 프로젝트의 생성 및 조회를 제공한다.
//!
//! 실행 복구와 모델 제공자 등 나머지 엔진 기능은 아직 연결되지 않았다.

pub mod agent_loop;

use magi_core::{Command, Project, Request, Response, ResponseData, SpecDraft};
use redb::{Database, ReadableTable, TableDefinition};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SCHEMA_VERSION: u64 = 1;

pub struct Engine {
    db: Database,
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
        // 2026-11-15: 완료 복구가 아직 없는 단계에서는 journal을 열거나 DB를 변경하지 않는다.
        let mut journal_path = database_path.as_os_str().to_os_string();
        journal_path.push(".completion-journal");
        match std::fs::symlink_metadata(&journal_path) {
            Ok(_) => return Err("completion journal requires the full recovery engine".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(err(error)),
        }
        let db = Database::create(database_path).map_err(err)?;
        initialize_schema(&db)?;
        Ok(Self { db })
    }

    pub fn handle(&self, request: Request) -> Response {
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
        match command {
            // 2026-11-15: 부분 엔진의 준비 상태가 미구현 기능까지 보장하는 것으로 보이지 않게 한다.
            Command::Status => Ok(ResponseData::Status {
                message: "engine ready; project storage only; workflow and models unavailable"
                    .into(),
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
                let tx = self.db.begin_write().map_err(err)?;
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
                let tx = self.db.begin_read().map_err(err)?;
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
                let tx = self.db.begin_read().map_err(err)?;
                let table = tx.open_table(PROJECTS).map_err(err)?;
                let mut projects = Vec::new();
                for entry in table.iter().map_err(err)? {
                    let (_, value) = entry.map_err(err)?;
                    projects.push(serde_json::from_str(value.value()).map_err(err)?);
                }
                Ok(ResponseData::Projects(projects))
            }
            Command::SubmitInterview {
                project_id,
                message,
            } => self.update(&project_id, |project| {
                if message.trim().is_empty() {
                    return Err("interview message is empty".into());
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
            _ => Err("requested feature is not available".into()),
        }
    }

    // 2026-11-15: 원본과 같이 읽기, 변경, 저장을 하나의 트랜잭션으로 묶는다.
    fn update(
        &self,
        id: &str,
        update: impl FnOnce(&mut Project) -> Result<ResponseData, String>,
    ) -> Result<ResponseData, String> {
        let tx = self.db.begin_write().map_err(err)?;
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
