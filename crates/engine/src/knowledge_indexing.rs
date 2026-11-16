use crate::{err, knowledge_text, Engine, PROJECTS};
use domain::{ResponseData, SecretString};
use redb::{Database, TableDefinition};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
use tokio_util::sync::CancellationToken;
const PROFILES: TableDefinition<&str, &str> =
    TableDefinition::new("integration_embedding_profiles");
const STATUSES: TableDefinition<&str, &str> = TableDefinition::new("integration_embedding_status");

struct SearchSlot(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for SearchSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

// 2026-11-16: 쿼리 임베딩도 공통 슬롯을 사용하고 설정 세대가 바뀐 결과는 반환하지 않는다.
pub(crate) async fn semantic_search(
    engine: Arc<Engine>,
    project: String,
    query: String,
) -> Result<ResponseData, String> {
    if query.trim().is_empty() {
        return Err("search query is empty".into());
    }
    let profile = {
        let state = engine
            .indexing
            .lock()
            .map_err(|_| "index state unavailable")?;
        if state.configuring.contains(&project) {
            return Err("embedding configuration in progress".into());
        }
        state.profiles.get(&project).cloned()
    };
    let mut vector = None;
    if let Some(profile) = &profile {
        engine
            .slots
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| "run capacity reached")?;
        let _slot = SearchSlot(engine.slots.clone());
        let provider = provider::OpenAiEmbeddingProvider::new(provider::OpenAiEmbeddingConfig {
            endpoint: profile.endpoint.clone(),
            model: profile.model.clone(),
            api_key: profile.credential.0.clone(),
            dimensions: profile.dimensions,
            max_response_bytes: 1024 * 1024,
        });
        if let Ok(provider) = provider {
            if let Ok(vectors) = provider
                .embed(vec![query.clone()], CancellationToken::new())
                .await
            {
                vector = vectors.into_iter().next();
            }
        }
    }
    let e = engine.clone();
    let p = project.clone();
    let q = query.clone();
    let profile_copy = profile.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<ResponseData, String> {
        let db = e.db.get().ok_or("storage unavailable")?;
        if let (Some(profile), Some(vector)) = (&profile_copy, vector) {
            let model = serde_json::json!({"model":profile.model,"version":profile.version,"dimensions":profile.dimensions});
            if let Ok(result) = knowledge_text::search_vector(&db, &p, model, vector, 0, 100) { return Ok(result); }
        }
        let result = knowledge_text::search(&db, &p, &q, 0, 100)?;
        match result { ResponseData::Data(mut value) => { value["mode"] = "text_fallback".into(); Ok(ResponseData::Data(value)) }, _ => Err("invalid search result".into()) }
    }).await.map_err(err)??;
    let state = engine
        .indexing
        .lock()
        .map_err(|_| "index state unavailable")?;
    if state.configuring.contains(&project)
        || state.profiles.get(&project).map(|p| &p.generation)
            != profile.as_ref().map(|p| &p.generation)
    {
        return Err("embedding configuration changed".into());
    }
    Ok(result)
}

#[derive(Clone)]
struct Profile {
    endpoint: String,
    model: String,
    version: String,
    dimensions: usize,
    credential: SecretString,
    generation: String,
}
struct Job {
    generation: String,
    profile_generation: String,
    cancel: CancellationToken,
    phase: AtomicU8,
}
#[derive(Default)]
pub(crate) struct IndexState {
    profiles: HashMap<String, Profile>,
    jobs: HashMap<(String, String), Arc<Job>>,
    configuring: HashSet<String>,
}
// 2026-11-16: worker 종료와 슬롯 반환을 같은 짧은 상태 경계에서 처리한다.
struct JobLease {
    engine: Arc<Engine>,
    key: (String, String),
    job: Arc<Job>,
}
impl Drop for JobLease {
    fn drop(&mut self) {
        let mut state = self
            .engine
            .indexing
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.engine.slots.fetch_sub(1, Ordering::AcqRel);
        if state
            .jobs
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.job))
        {
            state.jobs.remove(&self.key);
        }
    }
}
fn key(project: &str, doc: &str) -> Result<String, String> {
    serde_json::to_string(&(project, doc)).map_err(err)
}
fn write_status(db: &Database, project: &str, doc: &str, status: &str) -> Result<(), String> {
    let tx = db.begin_write().map_err(err)?;
    {
        let mut table = tx.open_table(STATUSES).map_err(err)?;
        table
            .insert(key(project, doc)?.as_str(), status)
            .map_err(err)?;
    }
    tx.commit().map_err(err)
}

// 2026-11-16: 설정 갱신 중 등록을 막고 기존 작업을 취소하되 저장 대기 동안 상태 잠금을 잡지 않는다.
pub(crate) fn configure(
    engine: &Engine,
    project_id: String,
    endpoint: String,
    model: String,
    version: String,
    dimensions: usize,
    credential: SecretString,
) -> Result<ResponseData, String> {
    if version.trim().is_empty() {
        return Err("embedding version required".into());
    }
    let profile = Profile {
        endpoint,
        model,
        version,
        dimensions,
        credential,
        generation: uuid::Uuid::new_v4().to_string(),
    };
    provider::OpenAiEmbeddingProvider::new(provider::OpenAiEmbeddingConfig {
        endpoint: profile.endpoint.clone(),
        model: profile.model.clone(),
        api_key: profile.credential.0.clone(),
        dimensions: profile.dimensions,
        max_response_bytes: 1024 * 1024,
    })
    .map_err(err)?;
    {
        let mut state = engine
            .indexing
            .lock()
            .map_err(|_| "index state unavailable")?;
        if state.configuring.contains(&project_id) {
            return Err("embedding configuration in progress".into());
        }
        if state
            .jobs
            .iter()
            .any(|((p, _), j)| p == &project_id && j.phase.load(Ordering::Acquire) == 2)
        {
            return Err("embedding publication in progress".into());
        }
        state.configuring.insert(project_id.clone());
        for ((p, _), job) in &state.jobs {
            if p == &project_id {
                job.phase.store(1, Ordering::Release);
                job.cancel.cancel();
            }
        }
    }
    let stored = (|| {
        let db = engine.db.get().ok_or("storage unavailable")?;
        let tx = db.begin_write().map_err(err)?;
        {
            use redb::ReadableTable;
            let table = tx.open_table(PROJECTS).map_err(err)?;
            if table.get(project_id.as_str()).map_err(err)?.is_none() {
                return Err("project not found".into());
            }
        }
        {
            let mut table = tx.open_table(PROFILES).map_err(err)?;
            let value=serde_json::json!({"endpoint":profile.endpoint,"model":profile.model,"version":profile.version,"dimensions":profile.dimensions}).to_string();
            table
                .insert(project_id.as_str(), value.as_str())
                .map_err(err)?;
        }
        tx.commit().map_err(err)
    })();
    let mut state = engine
        .indexing
        .lock()
        .map_err(|_| "index state unavailable")?;
    state.configuring.remove(&project_id);
    stored?;
    state.profiles.insert(project_id, profile);
    Ok(ResponseData::Empty)
}

// 2026-11-16: 원문 조회 후 설정 세대를 다시 확인하고 기존 전역 용량 안에서 문서별 작업을 등록한다.
pub(crate) async fn start(
    engine: Arc<Engine>,
    project_id: String,
    document_id: String,
) -> Result<ResponseData, String> {
    tokio::spawn(start_inner(engine, project_id, document_id))
        .await
        .map_err(err)?
}
async fn start_inner(
    engine: Arc<Engine>,
    project_id: String,
    document_id: String,
) -> Result<ResponseData, String> {
    let profile = {
        let state = engine
            .indexing
            .lock()
            .map_err(|_| "index state unavailable")?;
        if state.configuring.contains(&project_id) {
            return Err("embedding configuration in progress".into());
        }
        state
            .profiles
            .get(&project_id)
            .cloned()
            .ok_or("embedding credentials unavailable")?
    };
    let worker = Arc::clone(&engine);
    let p = project_id.clone();
    let d = document_id.clone();
    let doc: integrations::Document = tokio::task::spawn_blocking(move || {
        let db = worker.db.get().ok_or("storage unavailable")?;
        let value = knowledge_text::source(&db, &p, &d)?;
        match value {
            ResponseData::Data(v) => serde_json::from_value(v).map_err(err),
            _ => Err("invalid document".into()),
        }
    })
    .await
    .map_err(err)??;
    let key = (project_id.clone(), document_id.clone());
    let job = Arc::new(Job {
        generation: uuid::Uuid::new_v4().to_string(),
        profile_generation: profile.generation.clone(),
        cancel: CancellationToken::new(),
        phase: AtomicU8::new(0),
    });
    {
        let mut state = engine
            .indexing
            .lock()
            .map_err(|_| "index state unavailable")?;
        if state.configuring.contains(&project_id)
            || state
                .profiles
                .get(&project_id)
                .is_none_or(|p| p.generation != profile.generation)
        {
            return Err("embedding configuration changed".into());
        }
        if state.jobs.contains_key(&key) {
            return Err("index already active".into());
        }
        engine
            .slots
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| "run capacity reached")?;
        state.jobs.insert(key.clone(), Arc::clone(&job));
    }
    let lease = JobLease {
        engine: Arc::clone(&engine),
        key: key.clone(),
        job: Arc::clone(&job),
    };
    let persist_engine = Arc::clone(&engine);
    let p = project_id.clone();
    let d = document_id.clone();
    tokio::task::spawn_blocking(move || {
        let db = persist_engine.db.get().ok_or("storage unavailable")?;
        write_status(&db, &p, &d, "running")
    })
    .await
    .map_err(err)??;
    let task_engine = Arc::clone(&engine);
    tokio::spawn(async move {
        let result=async {
            let provider=provider::OpenAiEmbeddingProvider::new(provider::OpenAiEmbeddingConfig{endpoint:profile.endpoint,model:profile.model.clone(),api_key:profile.credential.0.clone(),dimensions:profile.dimensions,max_response_bytes:1024*1024}).map_err(err)?;
            let vectors=provider.embed(vec![doc.text],job.cancel.clone()).await.map_err(err)?;
            let model=serde_json::json!({"model":profile.model,"version":profile.version,"dimensions":profile.dimensions});
            let write_engine=Arc::clone(&task_engine);let p=project_id.clone();let d=document_id.clone();let write_job=Arc::clone(&job);let write_key=key.clone();
            tokio::task::spawn_blocking(move || {let db=write_engine.db.get().ok_or("storage unavailable")?;knowledge_text::set_embedding_checked(&db,&p,&d,doc.version,model,vectors.into_iter().next().ok_or("empty embedding")?,||{let state=write_engine.indexing.lock().map_err(|_|"index state unavailable")?;let current=state.jobs.get(&write_key).ok_or("index cancelled")?;if !Arc::ptr_eq(current,&write_job) || current.generation!=write_job.generation || current.phase.load(Ordering::Acquire)!=0 || current.cancel.is_cancelled() || state.configuring.contains(&p) || state.profiles.get(&p).is_none_or(|v|v.generation!=write_job.profile_generation){return Err("index cancelled or superseded".into());}current.phase.store(2,Ordering::Release);Ok(())})}).await.map_err(err)??;
            Ok::<(),String>(())
        }.await;
        let status = if result.is_ok() {
            "completed"
        } else if job.phase.load(Ordering::Acquire) == 2 {
            "uncertain"
        } else if job.cancel.is_cancelled() {
            "cancelled"
        } else {
            "failed"
        };
        let final_engine = Arc::clone(&task_engine);
        let p = project_id.clone();
        let d = document_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let db = final_engine.db.get().ok_or("storage unavailable")?;
            write_status(&db, &p, &d, status)
        })
        .await;
        drop(lease);
    });
    Ok(ResponseData::Data(serde_json::json!({"status":"running"})))
}

// 2026-11-16: 게시권 획득 전 취소만 성공으로 응답하고 실제 worker 종료까지 작업을 유지한다.
pub(crate) fn cancel(
    engine: &Engine,
    project: &str,
    document: &str,
) -> Result<ResponseData, String> {
    let state = engine
        .indexing
        .lock()
        .map_err(|_| "index state unavailable")?;
    let job = state
        .jobs
        .get(&(project.to_owned(), document.to_owned()))
        .ok_or("index job not found")?;
    if job.phase.load(Ordering::Acquire) == 2 {
        return Err("embedding publication in progress".into());
    }
    job.phase.store(1, Ordering::Release);
    job.cancel.cancel();
    Ok(ResponseData::Empty)
}
pub(crate) fn status(
    engine: &Engine,
    project: &str,
    document: &str,
) -> Result<ResponseData, String> {
    {
        let state = engine
            .indexing
            .lock()
            .map_err(|_| "index state unavailable")?;
        if let Some(job) = state.jobs.get(&(project.to_owned(), document.to_owned())) {
            let status = match job.phase.load(Ordering::Acquire) {
                1 => "cancelling",
                2 => "publishing",
                _ => "running",
            };
            return Ok(ResponseData::Data(
                serde_json::json!({"status":status,"resources_released":false}),
            ));
        }
    }
    let db = engine.db.get().ok_or("storage unavailable")?;
    let tx = db.begin_read().map_err(err)?;
    {
        let projects = tx.open_table(PROJECTS).map_err(err)?;
        if projects.get(project).map_err(err)?.is_none() {
            return Err("project not found".into());
        }
    }
    let result = match tx.open_table(STATUSES) {
        Ok(table) => table
            .get(key(project, document)?.as_str())
            .map_err(err)?
            .map(|v| v.value().to_owned()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(err(e)),
    };
    let status = match result.as_deref() {
        Some("running") => "interrupted",
        Some(s) => s,
        None => "not_running",
    };
    Ok(ResponseData::Data(
        serde_json::json!({"status":status,"resources_released":true}),
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use domain::{Command, Request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn cancellation_and_status_do_not_wait_for_blocked_database_writer() {
        let root = std::env::temp_dir().join(format!("magi-index-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let engine = Arc::new(Engine::open(&root.join("state.redb")).unwrap());
        let project = match engine
            .handle(Request {
                id: "create".into(),
                command: Command::CreateProject {
                    name: "fixture".into(),
                    path: root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            ResponseData::Project(p) => p.id,
            _ => panic!("project"),
        };
        engine.clone().handle_async(Request{id:"put".into(),command:Command::PutKnowledge{project_id:project.clone(),document:serde_json::json!({"id":"d","version":1,"source":"source","text":"text"})}}).await.result.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            ready_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
            let _=socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await;
        });
        engine
            .clone()
            .handle_async(Request {
                id: "configure".into(),
                command: Command::ConfigureEmbedding {
                    project_id: project.clone(),
                    endpoint: format!("http://{address}/embeddings"),
                    model: "fake-model".into(),
                    version: "v1".into(),
                    dimensions: 2,
                    credential: SecretString("fake-key".into()),
                },
            })
            .await
            .result
            .unwrap();
        engine
            .clone()
            .handle_async(Request {
                id: "start".into(),
                command: Command::IndexKnowledge {
                    project_id: project.clone(),
                    document_id: "d".into(),
                },
            })
            .await
            .result
            .unwrap();
        ready_rx.await.unwrap();
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        let (write_ready, write_wait) = std::sync::mpsc::channel();
        *crate::knowledge_text::TEST_INDEX_BEFORE_WRITE
            .lock()
            .unwrap() = Some((project.clone(), write_ready));
        release_tx.send(()).unwrap();
        server.await.unwrap();
        tokio::task::spawn_blocking(move || {
            write_wait.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .unwrap()
        .unwrap();
        let cancel = engine.clone().handle_async(Request {
            id: "cancel".into(),
            command: Command::CancelKnowledgeIndex {
                project_id: project.clone(),
                document_id: "d".into(),
            },
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), cancel)
            .await
            .unwrap()
            .result
            .unwrap();
        let status = engine.clone().handle_async(Request {
            id: "status".into(),
            command: Command::KnowledgeIndexStatus {
                project_id: project.clone(),
                document_id: "d".into(),
            },
        });
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), status)
            .await
            .unwrap()
            .result
            .unwrap();
        let ResponseData::Data(value) = response else {
            panic!("status")
        };
        assert_eq!(value["resources_released"], false);
        drop(tx);
        drop(db);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let result = engine
                    .clone()
                    .handle_async(Request {
                        id: "status".into(),
                        command: Command::KnowledgeIndexStatus {
                            project_id: project.clone(),
                            document_id: "d".into(),
                        },
                    })
                    .await
                    .result
                    .unwrap();
                if let ResponseData::Data(v) = result {
                    if v["resources_released"] == true {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let search = engine
            .clone()
            .handle_async(Request {
                id: "search".into(),
                command: Command::SearchKnowledgeVector {
                    project_id: project,
                    model: serde_json::json!({"model":"fake-model","version":"v1","dimensions":2}),
                    vector: vec![1.0, 0.0],
                    offset: 0,
                    limit: 1,
                },
            })
            .await
            .result;
        assert!(search.is_err());
        drop(engine);
        let _ = std::fs::remove_file(root.join("state.redb"));
        let _ = std::fs::remove_dir(root);
    }
}
