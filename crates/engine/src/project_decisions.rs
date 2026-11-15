use crate::err;
use domain::ResponseData;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;
const DECISIONS: TableDefinition<&str, &str> = TableDefinition::new("integration_decisions");
// 2026-11-16: 프로젝트와 결정의 버전 이력을 함께 저장하여 승인 맥락이 어긋나지 않게 한다.
pub(crate) fn save_decision(
    db: Arc<Database>,
    project_id: String,
    text: String,
) -> Result<ResponseData, String> {
    if text.trim().is_empty() {
        return Err("decision is empty".into());
    }
    let tx = db.begin_write().map_err(err)?;
    let result;
    {
        let mut projects = tx.open_table(crate::PROJECTS).map_err(err)?;
        let mut project: domain::Project = {
            let entry = projects
                .get(project_id.as_str())
                .map_err(err)?
                .ok_or("project not found")?;
            serde_json::from_str(entry.value()).map_err(err)?
        };
        let mut table = tx.open_table(DECISIONS).map_err(err)?;
        let mut records: Vec<integrations::Decision> =
            match table.get(project_id.as_str()).map_err(err)? {
                Some(v) => serde_json::from_str(v.value()).map_err(err)?,
                None => project
                    .decisions
                    .iter()
                    .enumerate()
                    .map(|(i, t)| integrations::Decision {
                        id: format!("legacy:{i}"),
                        version: 1,
                        text: t.clone(),
                    })
                    .collect(),
            };
        records.push(integrations::Decision {
            id: uuid::Uuid::new_v4().to_string(),
            version: 1,
            text: text.clone(),
        });
        project.decisions.push(text);
        project.context_version = project
            .context_version
            .checked_add(1)
            .ok_or("context version overflow")?;
        project.approved = None;
        let value = serde_json::to_string(&records).map_err(err)?;
        table
            .insert(project_id.as_str(), value.as_str())
            .map_err(err)?;
        let value = serde_json::to_string(&project).map_err(err)?;
        projects
            .insert(project_id.as_str(), value.as_str())
            .map_err(err)?;
        result = ResponseData::Project(project);
    }
    tx.commit().map_err(err)?;
    Ok(result)
}
pub(crate) fn list_decisions(
    db: Arc<Database>,
    project_id: String,
) -> Result<ResponseData, String> {
    let tx = db.begin_read().map_err(err)?;
    let projects = tx.open_table(crate::PROJECTS).map_err(err)?;
    let p = projects
        .get(project_id.as_str())
        .map_err(err)?
        .ok_or("project not found")?;
    let project: domain::Project = serde_json::from_str(p.value()).map_err(err)?;
    let records: Vec<integrations::Decision> = match tx.open_table(DECISIONS) {
        Ok(table) => match table.get(project_id.as_str()).map_err(err)? {
            Some(v) => serde_json::from_str(v.value()).map_err(err)?,
            None => vec![],
        },
        Err(redb::TableError::TableDoesNotExist(_)) => vec![],
        Err(error) => return Err(err(error)),
    };
    let records = if records.is_empty() {
        project
            .decisions
            .into_iter()
            .enumerate()
            .map(|(i, text)| integrations::Decision {
                id: format!("legacy:{i}"),
                version: 1,
                text,
            })
            .collect()
    } else {
        records
    };
    Ok(ResponseData::Data(serde_json::json!({"decisions":records})))
}
// 2026-11-16: 선택 당시의 원문과 버전을 초안에 복사하고 이전 승인을 무효화한다.
pub(crate) fn select_decision(
    db: Arc<Database>,
    project_id: String,
    id: String,
    version: u64,
) -> Result<ResponseData, String> {
    let tx = db.begin_write().map_err(err)?;
    let selected;
    {
        let mut projects = tx.open_table(crate::PROJECTS).map_err(err)?;
        let mut project: domain::Project = {
            let p = projects
                .get(project_id.as_str())
                .map_err(err)?
                .ok_or("project not found")?;
            serde_json::from_str(p.value()).map_err(err)?
        };
        let records: Vec<integrations::Decision> = match tx.open_table(DECISIONS) {
            Ok(table) => match table.get(project_id.as_str()).map_err(err)? {
                Some(v) => serde_json::from_str(v.value()).map_err(err)?,
                None => vec![],
            },
            Err(redb::TableError::TableDoesNotExist(_)) => vec![],
            Err(error) => return Err(err(error)),
        };
        let records = if records.is_empty() {
            project
                .decisions
                .iter()
                .enumerate()
                .map(|(i, text)| integrations::Decision {
                    id: format!("legacy:{i}"),
                    version: 1,
                    text: text.clone(),
                })
                .collect()
        } else {
            records
        };
        selected = records
            .into_iter()
            .find(|d| d.id == id && d.version == version)
            .ok_or("decision version not found")?;
        project
            .draft
            .reused_decisions
            .push(serde_json::to_string(&selected).map_err(err)?);
        project.draft_hash = crate::draft_hash(&project.draft)?;
        project.context_version = project
            .context_version
            .checked_add(1)
            .ok_or("context version overflow")?;
        project.approved = None;
        let value = serde_json::to_string(&project).map_err(err)?;
        projects
            .insert(project_id.as_str(), value.as_str())
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Data(
        serde_json::to_value(selected).map_err(err)?,
    ))
}

// 2026-11-16: 개정은 새 버전을 추가하며 기존 선택 사본과 승인 이력의 내용을 변경하지 않는다.
pub(crate) fn revise_decision(
    db: Arc<Database>,
    project_id: String,
    id: String,
    text: String,
) -> Result<ResponseData, String> {
    if text.trim().is_empty() {
        return Err("decision is empty".into());
    }
    let tx = db.begin_write().map_err(err)?;
    let revised;
    {
        let mut projects = tx.open_table(crate::PROJECTS).map_err(err)?;
        let mut project: domain::Project = {
            let p = projects
                .get(project_id.as_str())
                .map_err(err)?
                .ok_or("project not found")?;
            serde_json::from_str(p.value()).map_err(err)?
        };
        let mut table = tx.open_table(DECISIONS).map_err(err)?;
        let mut records: Vec<integrations::Decision> =
            match table.get(project_id.as_str()).map_err(err)? {
                Some(v) => serde_json::from_str(v.value()).map_err(err)?,
                None => project
                    .decisions
                    .iter()
                    .enumerate()
                    .map(|(i, text)| integrations::Decision {
                        id: format!("legacy:{i}"),
                        version: 1,
                        text: text.clone(),
                    })
                    .collect(),
            };
        let version = records
            .iter()
            .filter(|d| d.id == id)
            .map(|d| d.version)
            .max()
            .ok_or("decision not found")?
            .checked_add(1)
            .ok_or("decision version overflow")?;
        revised = integrations::Decision {
            id,
            version,
            text: text.clone(),
        };
        records.push(revised.clone());
        let json = serde_json::to_string(&records).map_err(err)?;
        table
            .insert(project_id.as_str(), json.as_str())
            .map_err(err)?;
        project.decisions.push(text);
        project.context_version = project
            .context_version
            .checked_add(1)
            .ok_or("context version overflow")?;
        project.approved = None;
        let json = serde_json::to_string(&project).map_err(err)?;
        projects
            .insert(project_id.as_str(), json.as_str())
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    Ok(ResponseData::Data(
        serde_json::to_value(revised).map_err(err)?,
    ))
}
