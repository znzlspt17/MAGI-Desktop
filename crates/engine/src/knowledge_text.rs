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

// 2026-11-16: 손상된 checkpoint를 덮어쓰지 않아 복구할 수 없는 기존 벡터를 조용히 잃지 않게 한다.
pub(crate) fn set_embedding(
    db: &Database,
    project_id: &str,
    document_id: &str,
    version: u64,
    model: serde_json::Value,
    vector: Vec<f32>,
) -> Result<ResponseData, String> {
    let model: integrations::EmbeddingModel = serde_json::from_value(model).map_err(err)?;
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
            Some(row) => serde_json::from_str(row.value()).map_err(err)?,
            None => vec![],
        }
    };
    let checkpoint = {
        let table = tx.open_table(KNOWLEDGE).map_err(err)?;
        let row = table.get(project_id).map_err(err)?;
        row.map(|v| v.value().to_owned())
    };
    let mut index = match checkpoint {
        Some(value) => {
            KnowledgeIndex::restore(value.as_bytes()).map_err(|_| "invalid knowledge checkpoint")?
        }
        None => KnowledgeIndex::rebuild(sources)?,
    };
    index.set_embedding(document_id, version, model, vector)?;
    let checkpoint = index.checkpoint()?;
    {
        let mut table = tx.open_table(KNOWLEDGE).map_err(err)?;
        table
            .insert(project_id, std::str::from_utf8(&checkpoint).map_err(err)?)
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Empty)
}

// 2026-11-16: 일치하는 모델의 저장 벡터만 사용하고 원문 출처를 결과와 함께 반환한다.
pub(crate) fn search_vector(
    db: &Database,
    project_id: &str,
    model: serde_json::Value,
    vector: Vec<f32>,
    offset: usize,
    limit: usize,
) -> Result<ResponseData, String> {
    let model: integrations::EmbeddingModel = serde_json::from_value(model).map_err(err)?;
    let index = read(db, project_id)?;
    let results = index.semantic_search(&vector, &model, offset, limit)?;
    Ok(ResponseData::Data(
        serde_json::json!({"mode":"semantic","results":results}),
    ))
}

// 2026-11-16: 원문 출처에서만 관계를 만들고 단일 응답 상한 초과는 페이지 조회로 명시한다.
pub(crate) fn graph(
    db: &Database,
    project_id: &str,
    page: Option<(usize, usize)>,
) -> Result<ResponseData, String> {
    let index = read(db, project_id)?;
    let (offset, limit) = page.unwrap_or((0, 100));
    if page.is_none() && !index.graph(100, 1).is_empty() {
        return Err("graph exceeds one page; use GetGraphPage".into());
    }
    let edges = index.graph(offset, limit);
    let mut nodes = Vec::new();
    for edge in &edges {
        nodes.push(serde_json::json!({"id":edge.source,"kind":"source"}));
        nodes.push(serde_json::json!({"id":edge.document_id,"kind":"document","version":edge.document_version}));
    }
    nodes.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    nodes.dedup();
    Ok(ResponseData::Data(
        serde_json::json!({"nodes":nodes,"edges":edges}),
    ))
}
