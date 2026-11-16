use crate::{err, mcp_connections};
use domain::ResponseData;
use redb::{Database, TableDefinition};

const POLICIES: TableDefinition<&str, &str> = TableDefinition::new("integration_mcp_policy");
const VERSIONS: TableDefinition<&str, &str> =
    TableDefinition::new("integration_mcp_policy_versions");

// 2026-11-16: 기존 주소와 정책 갱신을 직렬화하고 같은 정책으로 복원해도 별도 세대로 취급한다.
pub(crate) fn configure(
    db: &Database,
    project: &str,
    names: Vec<String>,
) -> Result<ResponseData, String> {
    if names.len() > 1000 || names.iter().any(|name| name.trim().is_empty()) {
        return Err("invalid MCP allowlist".into());
    }
    mcp_connections::endpoint(db, project)?;
    let value = serde_json::to_string(&names).map_err(err)?;
    let tx = db.begin_write().map_err(err)?;
    {
        let mut table = tx.open_table(POLICIES).map_err(err)?;
        table.insert(project, value.as_str()).map_err(err)?;
    }
    {
        let mut table = tx.open_table(VERSIONS).map_err(err)?;
        table
            .insert(project, uuid::Uuid::new_v4().to_string().as_str())
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Empty)
}
