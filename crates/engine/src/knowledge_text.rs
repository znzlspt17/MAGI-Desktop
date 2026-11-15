use crate::{err, PROJECTS};
use domain::ResponseData;
use integrations::{Document, KnowledgeIndex};
use redb::{Database, ReadableTable, TableDefinition};

const SOURCES: TableDefinition<&str, &str> = TableDefinition::new("integration_knowledge_sources");
const KNOWLEDGE: TableDefinition<&str, &str> = TableDefinition::new("integration_knowledge");

// 2026-11-16: 원문과 검색 checkpoint를 같은 트랜잭션에 기록해 한쪽만 갱신되는 일을 막는다.
pub(crate) fn put(
    db: &Database,
    project_id: &str,
    document: serde_json::Value,
) -> Result<ResponseData, String> {
    let document: Document = serde_json::from_value(document).map_err(err)?;
    let tx = db.begin_write().map_err(err)?;
    {
        let projects = tx.open_table(PROJECTS).map_err(err)?;
        if projects.get(project_id).map_err(err)?.is_none() {
            return Err("project not found".into());
        }
    }
    let sources: Vec<Document> = {
        let table = tx.open_table(SOURCES).map_err(err)?;
        let row = table.get(project_id).map_err(err)?;
        match row {
            Some(v) => serde_json::from_str(v.value()).map_err(err)?,
            None => vec![],
        }
    };
    let checkpoint = {
        let table = tx.open_table(KNOWLEDGE).map_err(err)?;
        let row = table.get(project_id).map_err(err)?;
        row.map(|v| v.value().to_owned())
    };
    let mut index = match checkpoint {
        Some(value) => KnowledgeIndex::restore(value.as_bytes())
            .or_else(|_| KnowledgeIndex::rebuild(sources.clone()))?,
        None => KnowledgeIndex::rebuild(sources.clone())?,
    };
    index.upsert(document.clone())?;
    let mut sources = sources;
    sources.retain(|d| d.id != document.id);
    sources.push(document);
    let checkpoint = index.checkpoint()?;
    let sources = serde_json::to_string(&sources).map_err(err)?;
    {
        let mut table = tx.open_table(SOURCES).map_err(err)?;
        table.insert(project_id, sources.as_str()).map_err(err)?;
    }
    {
        let mut table = tx.open_table(KNOWLEDGE).map_err(err)?;
        table
            .insert(project_id, std::str::from_utf8(&checkpoint).map_err(err)?)
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Empty)
}

// 2026-11-16: 검색 투영만 손상된 경우 독립 원문을 사용해 source provenance를 보존한다.
fn read(db: &Database, project_id: &str) -> Result<KnowledgeIndex, String> {
    let tx = db.begin_read().map_err(err)?;
    {
        let projects = tx.open_table(PROJECTS).map_err(err)?;
        if projects.get(project_id).map_err(err)?.is_none() {
            return Err("project not found".into());
        }
    }
    let sources: Vec<Document> = match tx.open_table(SOURCES) {
        Ok(table) => match table.get(project_id).map_err(err)? {
            Some(v) => serde_json::from_str(v.value()).map_err(err)?,
            None => vec![],
        },
        Err(redb::TableError::TableDoesNotExist(_)) => vec![],
        Err(e) => return Err(err(e)),
    };
    let checkpoint = match tx.open_table(KNOWLEDGE) {
        Ok(table) => table
            .get(project_id)
            .map_err(err)?
            .map(|v| v.value().to_owned()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(err(e)),
    };
    match checkpoint {
        Some(value) => {
            KnowledgeIndex::restore(value.as_bytes()).or_else(|_| KnowledgeIndex::rebuild(sources))
        }
        None => KnowledgeIndex::rebuild(sources),
    }
}

pub(crate) fn source(db: &Database, project_id: &str, id: &str) -> Result<ResponseData, String> {
    let index = read(db, project_id)?;
    let doc = index.source(id).ok_or("document not found")?;
    Ok(ResponseData::Data(serde_json::to_value(doc).map_err(err)?))
}
pub(crate) fn search(
    db: &Database,
    project_id: &str,
    query: &str,
    offset: usize,
    limit: usize,
) -> Result<ResponseData, String> {
    if query.trim().is_empty() {
        return Err("search query is empty".into());
    }
    let index = read(db, project_id)?;
    Ok(ResponseData::Data(
        serde_json::json!({"mode":"text","results":index.text_search(query,offset,limit)}),
    ))
}
