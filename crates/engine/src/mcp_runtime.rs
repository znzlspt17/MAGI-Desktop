use crate::{err, runs, Engine};
use domain::ResponseData;
use std::sync::Arc;

type AuthStamp = (String, (String, Option<String>), Option<(String, bool)>);
#[cfg(all(test, windows))]
type ClaimHook = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(all(test, windows))]
pub(crate) static TEST_AFTER_MCP_CLAIM: std::sync::Mutex<Option<ClaimHook>> =
    std::sync::Mutex::new(None);

// 2026-11-16: 취소가 상위 future를 버려도 외부 worker가 끝나기 전 agent lease를 돌려주지 않는다.
#[derive(Default)]
pub(crate) struct AgentWorkers {
    count: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}
impl AgentWorkers {
    fn register(self: &Arc<Self>) -> AgentWorker {
        self.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        AgentWorker(Arc::clone(self))
    }
    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}
struct AgentWorker(Arc<AgentWorkers>);
impl Drop for AgentWorker {
    fn drop(&mut self) {
        self.0
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}

// 2026-11-16: 승인 실행의 프로젝트와 정확히 같은 MCP 주소에만 세대가 결박된 토큰을 사용한다.
fn run_auth(
    engine: &Engine,
    run_id: &str,
    target: &str,
) -> Result<(Option<AuthStamp>, Option<domain::SecretString>), String> {
    let _gate = engine
        .mutation_gate
        .lock()
        .map_err(|_| "OAuth state unavailable")?;
    let db = engine.db.get().ok_or("storage unavailable")?;
    let run = runs::get(&db, run_id)?;
    let project = run["project_id"]
        .as_str()
        .ok_or("invalid run project")?
        .to_owned();
    let endpoint = match crate::mcp_connections::endpoint(&db, &project) {
        Ok(endpoint) => endpoint,
        Err(error) if error == "MCP not configured" => return Ok((None, None)),
        Err(error) => return Err(error),
    };
    if endpoint.0 != target {
        return Ok((None, None));
    }
    let (stamp, token) = crate::mcp_oauth::authentication(engine, &project, &endpoint)?;
    Ok((Some((project, endpoint, stamp)), token))
}

struct CancellationLease {
    engine: Arc<Engine>,
    run_id: String,
    token: Arc<tokio_util::sync::CancellationToken>,
}
impl Drop for CancellationLease {
    fn drop(&mut self) {
        let mut tokens = self
            .engine
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if tokens
            .get(&self.run_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.token))
        {
            tokens.remove(&self.run_id);
        }
    }
}

// 2026-11-16: 소유 작업으로 실행해 요청 연결이 끊겨도 결과를 저장하고 같은 승인을 재전송하지 않는다.
pub(crate) async fn call(
    engine: Arc<Engine>,
    run_id: String,
    approval_id: String,
) -> Result<ResponseData, String> {
    call_inner(engine, run_id, approval_id, None, None).await
}

#[cfg(windows)]
pub(crate) async fn call_bound(
    engine: Arc<Engine>,
    run_id: String,
    approval_id: String,
    call_id: String,
    token: tokio_util::sync::CancellationToken,
    identity: crate::agent_runtime::RunIdentity,
    workers: Arc<AgentWorkers>,
) -> Result<ResponseData, String> {
    call_inner(
        engine,
        run_id,
        approval_id,
        Some((call_id, token, identity)),
        Some(workers),
    )
    .await
}

async fn call_inner(
    engine: Arc<Engine>,
    run_id: String,
    approval_id: String,
    bound: Option<(
        String,
        tokio_util::sync::CancellationToken,
        crate::agent_runtime::RunIdentity,
    )>,
    workers: Option<Arc<AgentWorkers>>,
) -> Result<ResponseData, String> {
    let guard = workers.as_ref().map(AgentWorkers::register);
    let task = tokio::spawn(async move {
        let _guard = guard;
        let worker = Arc::clone(&engine);
        let consume_run = run_id.clone();
        let consume_approval = approval_id.clone();
        let consume_bound = bound.clone();
        let (request, token, owned) = tokio::task::spawn_blocking(move || {
            let _gate = worker
                .mutation_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if worker
                .startup_recovery_pending
                .load(std::sync::atomic::Ordering::Acquire)
                || worker.completions.blocks_global()
                || worker.completions.blocks_run(&consume_run)
            {
                return Err("완료 결과 저장을 먼저 복구해야 합니다.".into());
            }
            let db = worker.db.get().ok_or("storage unavailable")?;
            if let Some((call, token, identity)) = consume_bound {
                if token.is_cancelled() {
                    return Err("cancelled".into());
                }
                #[cfg(windows)]
                {
                    crate::agent_runtime::check_identity(&db, &identity)?;
                    return Ok((
                        runs::consume_bound_mcp(&db, &consume_run, &consume_approval, &call)?,
                        Arc::new(token),
                        false,
                    ));
                }
                #[cfg(not(windows))]
                return Err("agent MCP unavailable".into());
            }
            let token = Arc::new(tokio_util::sync::CancellationToken::new());
            {
                let mut tokens = worker
                    .cancellations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if tokens.contains_key(&consume_run) {
                    return Err("run already active".into());
                }
                tokens.insert(consume_run.clone(), Arc::clone(&token));
            }
            match runs::consume_mcp(&db, &consume_run, &consume_approval) {
                Ok(request) => Ok((request, token, true)),
                Err(error) => {
                    worker
                        .cancellations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&consume_run);
                    Err(error)
                }
            }
        })
        .await
        .map_err(err)??;
        #[cfg(all(test, windows))]
        {
            let hook = {
                let mut hook = TEST_AFTER_MCP_CLAIM.lock().unwrap();
                if hook.as_ref().is_some_and(|v| v.0 == run_id) {
                    hook.take()
                } else {
                    None
                }
            };
            if let Some((_, ready, release)) = hook {
                tokio::task::spawn_blocking(move || {
                    ready.send(()).unwrap();
                    release
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                })
                .await
                .map_err(err)?;
            }
        }
        let _lease = owned.then(|| CancellationLease {
            engine: Arc::clone(&engine),
            run_id: run_id.clone(),
            token: Arc::clone(&token),
        });
        let result=async {
            let endpoint=request["endpoint"].as_str().ok_or("invalid endpoint")?;
            let name=request["name"].as_str().ok_or("invalid tool name")?;
            let arguments=request.get("arguments").cloned().ok_or("invalid arguments")?;
            let cwd=request["cwd"].as_str().ok_or("invalid cwd")?;
            let e=engine.clone();let r=run_id.clone();let target=endpoint.to_owned();
            let (auth_stamp, credential)=tokio::task::spawn_blocking(move || run_auth(&e,&r,&target)).await.map_err(err)??;
            if bound.is_some() && auth_stamp.as_ref().is_none_or(|(_,endpoint,_)| request["_endpoint_stamp"] != serde_json::json!(endpoint)) { return Err("MCP endpoint changed".into()); }
            let client=match credential {
                Some(credential) => tokio::select! { biased; _ = token.cancelled() => return Err("MCP connection cancelled".into()), result = integrations::mcp::McpClient::connect_authenticated(endpoint,credential.0.clone()) => result? },
                None => integrations::mcp::McpClient::connect_cancelable(endpoint, token.as_ref().clone()).await?,
            };
            let tools=client.list_tools_cancelable(token.as_ref().clone()).await?;
            let schema=tools.iter().find(|tool|tool["name"].as_str()==Some(name)).and_then(|tool|tool.get("inputSchema")).ok_or("MCP tool schema missing")?;
            if auth_stamp.is_some() {
                let e=engine.clone();let r=run_id.clone();let target=endpoint.to_owned();
                let (current, _)=tokio::task::spawn_blocking(move || run_auth(&e,&r,&target)).await.map_err(err)??;
                if current != auth_stamp { return Err("MCP endpoint or authentication changed".into()); }
            }
            let authorization=integrations::mcp::CallAuthorization::Approved{name:name.into(),arguments:arguments.clone(),cwd:cwd.into()};
            let output=client.call_tool_cancelable(name,arguments,schema,cwd,&authorization,token.as_ref().clone()).await?;
            Ok(ResponseData::Data(serde_json::json!({"content":output.content,"structured_content":output.structured_content,"is_error":output.is_error})))
        }.await;
        let finish_engine = Arc::clone(&engine);
        let saved = result.as_ref().ok().and_then(|v| match v {
            ResponseData::Data(v) => Some(v.clone()),
            _ => None,
        });
        let complete = tokio::task::spawn_blocking(move || {
            let db = finish_engine.db.get().ok_or("storage unavailable")?;
            if let Some((call, _, _)) = bound {
                #[cfg(windows)]
                return runs::finish_bound_mcp(&db, &run_id, &approval_id, &call, saved);
                #[cfg(not(windows))]
                return Err("agent MCP unavailable".into());
            }
            runs::finish_mcp(&db, &run_id, &approval_id, saved.is_some())
        })
        .await
        .map_err(err)??;
        if !complete && result.is_ok() {
            return Err("MCP 실행이 취소되어 결과를 확인해야 합니다.".into());
        }
        result
    });
    task.await.map_err(err)?
}
