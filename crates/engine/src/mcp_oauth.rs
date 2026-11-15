use crate::{mcp_connections, Engine};
use domain::{Command, ResponseData, SecretString};
use integrations::mcp::{OAuthFlow, OAuthMetadata};
use std::sync::Arc;

type Snapshot = (String, Option<String>);
pub(crate) struct Session {
    snapshot: Snapshot,
    id: String,
    flow: Option<OAuthFlow>,
    _token: Option<SecretString>,
}

async fn snapshot(engine: Arc<Engine>, project: String) -> Result<Snapshot, String> {
    tokio::task::spawn_blocking(move || {
        let db = engine.db.get().ok_or("storage unavailable")?;
        mcp_connections::endpoint(&db, &project)
    })
    .await
    .map_err(|_| "OAuth state unavailable")?
}

async fn metadata(endpoint: &str, url: &str) -> Result<OAuthMetadata, String> {
    if url.trim().is_empty() {
        integrations::mcp::discover_oauth_automatically(endpoint).await
    } else {
        integrations::mcp::discover_oauth(endpoint, url).await
    }
}

// 2026-11-16: 네트워크 작업은 잠금 밖에서 수행하고 endpoint 세대와 세션을 같은 게이트에서 확인한다.
pub(crate) async fn handle(engine: Arc<Engine>, command: Command) -> Result<ResponseData, String> {
    match command {
        Command::DiscoverMcpOAuth {
            project_id,
            metadata_url,
        } => {
            let original = snapshot(engine.clone(), project_id.clone()).await?;
            let info = metadata(&original.0, &metadata_url).await?;
            if snapshot(engine, project_id).await? != original {
                return Err("MCP endpoint changed".into());
            }
            Ok(ResponseData::Data(
                serde_json::json!({"resource":info.resource,"authorization_endpoint":info.authorization_endpoint,"token_endpoint":info.token_endpoint}),
            ))
        }
        Command::BeginMcpOAuth {
            project_id,
            metadata_url,
            client_id,
            redirect_uri,
        } => {
            let e = engine.clone();
            let p = project_id.clone();
            let (original, id) = tokio::task::spawn_blocking(move || -> Result<_, String> {
                let _gate = e
                    .mutation_gate
                    .lock()
                    .map_err(|_| "OAuth state unavailable")?;
                let db = e.db.get().ok_or("storage unavailable")?;
                let original = mcp_connections::endpoint(&db, &p)?;
                let id = uuid::Uuid::new_v4().to_string();
                e.oauth
                    .lock()
                    .map_err(|_| "OAuth state unavailable")?
                    .insert(
                        p,
                        Session {
                            snapshot: original.clone(),
                            id: id.clone(),
                            flow: None,
                            _token: None,
                        },
                    );
                Ok((original, id))
            })
            .await
            .map_err(|_| "OAuth state unavailable")??;
            let info = metadata(&original.0, &metadata_url).await?;
            let flow = info.create_flow(&client_id, &redirect_uri)?;
            let url = flow.authorization_url();
            tokio::task::spawn_blocking(move || {
                let _gate = engine
                    .mutation_gate
                    .lock()
                    .map_err(|_| "OAuth state unavailable")?;
                let db = engine.db.get().ok_or("storage unavailable")?;
                if mcp_connections::endpoint(&db, &project_id)? != original {
                    return Err("MCP endpoint changed".into());
                }
                let mut sessions = engine.oauth.lock().map_err(|_| "OAuth state unavailable")?;
                let session = sessions
                    .get_mut(&project_id)
                    .ok_or("OAuth flow not found")?;
                if session.id != id {
                    return Err("OAuth flow replaced".into());
                }
                session.flow = Some(flow);
                Ok(ResponseData::Data(
                    serde_json::json!({"authorization_url":url}),
                ))
            })
            .await
            .map_err(|_| "OAuth state unavailable")?
        }
        Command::CompleteMcpOAuth {
            project_id,
            callback,
        } => {
            let e = engine.clone();
            let p = project_id.clone();
            let (original, id, mut flow) =
                tokio::task::spawn_blocking(move || -> Result<_, String> {
                    let _gate = e
                        .mutation_gate
                        .lock()
                        .map_err(|_| "OAuth state unavailable")?;
                    let db = e.db.get().ok_or("storage unavailable")?;
                    let current = mcp_connections::endpoint(&db, &p)?;
                    let mut sessions = e.oauth.lock().map_err(|_| "OAuth state unavailable")?;
                    let session = sessions.get_mut(&p).ok_or("OAuth flow not found")?;
                    if session.snapshot != current {
                        return Err("MCP endpoint changed".into());
                    }
                    Ok((
                        current,
                        session.id.clone(),
                        session.flow.take().ok_or("OAuth flow already used")?,
                    ))
                })
                .await
                .map_err(|_| "OAuth state unavailable")??;
            let token = flow.exchange(&callback.0).await?;
            tokio::task::spawn_blocking(move || {
                let _gate = engine
                    .mutation_gate
                    .lock()
                    .map_err(|_| "OAuth state unavailable")?;
                let db = engine.db.get().ok_or("storage unavailable")?;
                if mcp_connections::endpoint(&db, &project_id)? != original {
                    return Err("MCP endpoint changed".into());
                }
                let mut sessions = engine.oauth.lock().map_err(|_| "OAuth state unavailable")?;
                let session = sessions
                    .get_mut(&project_id)
                    .ok_or("OAuth flow not found")?;
                if session.id != id {
                    return Err("OAuth flow replaced".into());
                }
                session._token = Some(SecretString(token));
                Ok(ResponseData::Empty)
            })
            .await
            .map_err(|_| "OAuth state unavailable")?
        }
        _ => Err("unsupported OAuth command".into()),
    }
}
