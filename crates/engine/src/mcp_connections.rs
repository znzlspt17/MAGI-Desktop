use crate::{err, Engine, PROJECTS};
use domain::ResponseData;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

const ENDPOINTS: TableDefinition<&str, &str> = TableDefinition::new("integration_mcp_endpoints");
const GENERATIONS: TableDefinition<&str, &str> =
    TableDefinition::new("integration_mcp_endpoint_versions");
const POLICIES: TableDefinition<&str, &str> = TableDefinition::new("integration_mcp_policy");

// 2026-11-16: endpoint 변경과 이전 정책 무효화를 같은 트랜잭션에 기록해 다른 서버에 과거 권한이 적용되지 않게 한다.
pub(crate) fn configure(
    db: &Database,
    project_id: &str,
    endpoint: &str,
) -> Result<ResponseData, String> {
    integrations::mcp::validate_endpoint(endpoint)?;
    let tx = db.begin_write().map_err(err)?;
    {
        let projects = tx.open_table(PROJECTS).map_err(err)?;
        if projects.get(project_id).map_err(err)?.is_none() {
            return Err("project not found".into());
        }
    }
    {
        let mut table = tx.open_table(ENDPOINTS).map_err(err)?;
        table.insert(project_id, endpoint).map_err(err)?;
    }
    {
        let mut table = tx.open_table(GENERATIONS).map_err(err)?;
        table
            .insert(project_id, uuid::Uuid::new_v4().to_string().as_str())
            .map_err(err)?;
    }
    {
        let mut table = tx.open_table(POLICIES).map_err(err)?;
        table.remove(project_id).map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Empty)
}

// 2026-11-16: 동일 주소를 다시 설정한 경우도 새 세대로 구분해 늦은 목록 응답을 폐기한다.
pub(crate) fn endpoint(
    db: &Database,
    project_id: &str,
) -> Result<(String, Option<String>), String> {
    let tx = db.begin_read().map_err(err)?;
    {
        let projects = tx.open_table(PROJECTS).map_err(err)?;
        if projects.get(project_id).map_err(err)?.is_none() {
            return Err("project not found".into());
        }
    }
    let endpoint = {
        let table = match tx.open_table(ENDPOINTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Err("MCP not configured".into()),
            Err(error) => return Err(err(error)),
        };
        let row = table
            .get(project_id)
            .map_err(err)?
            .ok_or("MCP not configured")?;
        row.value().to_owned()
    };
    let generation = match tx.open_table(GENERATIONS) {
        Ok(table) => table
            .get(project_id)
            .map_err(err)?
            .map(|v| v.value().to_owned()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(error) => return Err(err(error)),
    };
    Ok((endpoint, generation))
}

pub(crate) async fn list(engine: Arc<Engine>, project_id: String) -> Result<ResponseData, String> {
    let first_engine = Arc::clone(&engine);
    let first_id = project_id.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        let _gate = first_engine
            .mutation_gate
            .lock()
            .map_err(|_| "OAuth state unavailable")?;
        let db = first_engine.db.get().ok_or("storage unavailable")?;
        let endpoint = endpoint(&db, &first_id)?;
        let (auth, token) = crate::mcp_oauth::authentication(&first_engine, &first_id, &endpoint)?;
        Ok::<_, String>((endpoint, auth, token))
    })
    .await
    .map_err(err)??;
    let client = match &snapshot.2 {
        Some(token) => {
            integrations::mcp::McpClient::connect_authenticated(&snapshot.0 .0, token.0.clone())
                .await?
        }
        None => integrations::mcp::McpClient::connect(&snapshot.0 .0).await?,
    };
    let tools = client.list_tools().await?;
    let current = tokio::task::spawn_blocking(move || {
        let _gate = engine
            .mutation_gate
            .lock()
            .map_err(|_| "OAuth state unavailable")?;
        let db = engine.db.get().ok_or("storage unavailable")?;
        let endpoint = endpoint(&db, &project_id)?;
        let (auth, _) = crate::mcp_oauth::authentication(&engine, &project_id, &endpoint)?;
        Ok::<_, String>((endpoint, auth))
    })
    .await
    .map_err(err)??;
    if (snapshot.0, snapshot.1) != current {
        return Err("MCP endpoint changed or authentication changed".into());
    }
    Ok(ResponseData::Data(serde_json::json!({"tools":tools})))
}
