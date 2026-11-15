// 2026-11-14: 동일 점수에서는 원본 ID로 정렬해 재색인 후에도 결과 순서를 유지한다.
pub fn search_text(documents: &[(&str, &str)], query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return vec![];
    }
    let mut scores: Vec<_> = documents
        .iter()
        .filter_map(|(id, text)| {
            let score = text.to_lowercase().matches(&query).count();
            (score > 0).then_some(((*id).to_owned(), score))
        })
        .collect();
    scores.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scores.into_iter().map(|(id, _)| id).collect()
}
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
// 2026-11-15: 생성한 checkpoint가 같은 버전의 복원 한도에 맞도록 양쪽에서 공유한다.
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub version: u64,
    pub source: String,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingModel {
    pub model: String,
    pub version: String,
    pub dimensions: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub document: Document,
    pub score: f32,
}
#[derive(Default, Serialize, Deserialize)]
pub struct KnowledgeIndex {
    documents: BTreeMap<String, Document>,
    vectors: BTreeMap<String, (EmbeddingModel, Vec<f32>)>,
    #[serde(skip)]
    pending: BTreeMap<String, u64>,
}
impl KnowledgeIndex {
    pub fn checkpoint(&self) -> Result<Vec<u8>, String> {
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if data.len() > MAX_CHECKPOINT_BYTES {
            return Err("checkpoint too large".into());
        }
        Ok(data)
    }
    pub fn restore(data: &[u8]) -> Result<Self, String> {
        if data.len() > MAX_CHECKPOINT_BYTES {
            return Err("checkpoint too large".into());
        }
        let saved: Self = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        if saved.documents.iter().any(|(id, doc)| id != &doc.id) {
            return Err("invalid document key".into());
        }
        let mut index = Self::rebuild(saved.documents.into_values().collect())?;
        for (id, (model, vector)) in saved.vectors {
            let version = index.source(&id).ok_or("orphan embedding")?.version;
            index.set_embedding(&id, version, model, vector)?;
        }
        Ok(index)
    }
    pub fn rebuild(documents: Vec<Document>) -> Result<Self, String> {
        let mut index = Self::default();
        for doc in documents {
            index.upsert(doc)?;
        }
        Ok(index)
    }
    // 2026-11-14: 오래된 비동기 인덱싱 결과가 최신 원문을 덮지 못하도록 버전을 검사한다.
    pub fn upsert(&mut self, doc: Document) -> Result<(), String> {
        if doc.id.trim().is_empty() || doc.source.trim().is_empty() || doc.version == 0 {
            return Err("invalid source document".into());
        }
        if let Some(old) = self.documents.get(&doc.id) {
            if doc.version <= old.version {
                return Err("document version is not newer".into());
            }
        }
        self.vectors.remove(&doc.id);
        self.pending.remove(&doc.id);
        self.documents.insert(doc.id.clone(), doc);
        Ok(())
    }
    pub fn source(&self, id: &str) -> Option<&Document> {
        self.documents.get(id)
    }
    pub fn text_search(&self, query: &str, offset: usize, limit: usize) -> Vec<Document> {
        let docs: Vec<_> = self
            .documents
            .values()
            .map(|d| (d.id.as_str(), d.text.as_str()))
            .collect();
        search_text(&docs, query)
            .into_iter()
            .skip(offset)
            .take(limit.min(100))
            .filter_map(|id| self.documents.get(&id).cloned())
            .collect()
    }
    pub fn set_embedding(
        &mut self,
        id: &str,
        version: u64,
        model: EmbeddingModel,
        vector: Vec<f32>,
    ) -> Result<(), String> {
        if self.documents.get(id).map(|d| d.version) != Some(version) {
            return Err("stale or unknown document".into());
        }
        if model.model.trim().is_empty()
            || model.version.trim().is_empty()
            || model.dimensions == 0
            || !valid_vector(&vector, model.dimensions)
        {
            return Err("invalid embedding".into());
        }
        self.vectors.insert(id.to_owned(), (model, vector));
        Ok(())
    }
    pub fn semantic_search(
        &self,
        query: &[f32],
        model: &EmbeddingModel,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        if !valid_vector(query, model.dimensions) {
            return Err("invalid query embedding".into());
        }
        if self.vectors.is_empty() || self.vectors.values().all(|(m, _)| m != model) {
            return Err("semantic index unavailable for model/version".into());
        }
        let norm = |v: &[f32]| {
            v.iter()
                .map(|x| (*x as f64) * (*x as f64))
                .sum::<f64>()
                .sqrt()
        };
        let qnorm = norm(query);
        let mut hits: Vec<_> = self
            .vectors
            .iter()
            .filter_map(|(id, (m, v))| {
                if m != model {
                    return None;
                }
                let dot = query
                    .iter()
                    .zip(v)
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum::<f64>();
                Some(SearchHit {
                    document: self.documents[id].clone(),
                    score: (dot / (qnorm * norm(v))) as f32,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document.id.cmp(&b.document.id))
        });
        Ok(hits.into_iter().skip(offset).take(limit.min(100)).collect())
    }
}
fn valid_vector(v: &[f32], dimensions: usize) -> bool {
    v.len() == dimensions
        && !v.is_empty()
        && v.iter().all(|x| x.is_finite())
        && v.iter().any(|x| *x != 0.0)
}
#[derive(Clone, Debug)]
pub struct IndexTicket {
    pub id: String,
    pub version: u64,
    pub generation: u64,
}
impl KnowledgeIndex {
    pub fn begin_embedding(&mut self, id: &str) -> Result<IndexTicket, String> {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let version = self.source(id).ok_or("unknown document")?.version;
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pending.insert(id.to_owned(), generation);
        Ok(IndexTicket {
            id: id.into(),
            version,
            generation,
        })
    }
    pub fn cancel_embedding(&mut self, ticket: &IndexTicket) {
        if self.pending.get(&ticket.id) == Some(&ticket.generation) {
            self.pending.remove(&ticket.id);
        }
    }
    pub fn finish_embedding(
        &mut self,
        ticket: &IndexTicket,
        model: EmbeddingModel,
        vector: Vec<f32>,
    ) -> Result<(), String> {
        if self.pending.get(&ticket.id) != Some(&ticket.generation) {
            return Err("cancelled or superseded indexing".into());
        }
        self.pending.remove(&ticket.id);
        self.set_embedding(&ticket.id, ticket.version, model, vector)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub document_id: String,
    pub document_version: u64,
}
// 2026-11-14: 실제 원본 연결만 노출해 문서 유사도를 출처 관계로 오인하지 않는다.
impl KnowledgeIndex {
    pub fn graph(&self, offset: usize, limit: usize) -> Vec<GraphEdge> {
        self.documents
            .values()
            .skip(offset)
            .take(limit.min(100))
            .map(|d| GraphEdge {
                source: d.source.clone(),
                document_id: d.id.clone(),
                document_version: d.version,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub version: u64,
    pub text: String,
}
#[derive(Default)]
pub struct DecisionStore {
    versions: BTreeMap<(String, u64), Decision>,
}
impl DecisionStore {
    pub fn save(&mut self, decision: Decision) -> Result<(), String> {
        if decision.id.trim().is_empty() || decision.text.trim().is_empty() || decision.version == 0
        {
            return Err("invalid decision".into());
        }
        if self
            .versions
            .keys()
            .any(|(id, version)| id == &decision.id && *version >= decision.version)
        {
            return Err("decision version is not newer".into());
        }
        self.versions
            .insert((decision.id.clone(), decision.version), decision);
        Ok(())
    }
    pub fn select(&self, id: &str, version: u64) -> Result<Decision, String> {
        self.versions
            .get(&(id.to_owned(), version))
            .cloned()
            .ok_or_else(|| "decision version not found".into())
    }
}

#[derive(Clone, Debug)]
pub struct PackageInput {
    pub completed_id: String,
    pub readme: String,
    pub spec: String,
    pub tasks: String,
    pub constraints: String,
    pub verification: String,
    pub completeness: String,
    pub test_status: String,
}
// 2026-11-14: 임시 디렉터리를 완성한 다음 새 경로로 이동하여 부분 결과와 덮어쓰기를 방지한다.
pub fn export_package(
    input: &PackageInput,
    destination: &std::path::Path,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    if input.completed_id.trim().is_empty() {
        return Err("approval required".into());
    }
    let docs = [
        ("README.ko.md", &input.readme),
        ("SPEC.ko.md", &input.spec),
        ("TASKS.md", &input.tasks),
        ("CONSTRAINTS.md", &input.constraints),
        ("VERIFICATION.md", &input.verification),
    ];
    if docs.iter().any(|(_, body)| body.len() > 8 * 1024 * 1024) {
        return Err("package file too large".into());
    }
    let hash = |body: &[u8]| format!("{:x}", Sha256::digest(body));
    let files: Vec<_> = docs.iter().map(|(name, body)| serde_json::json!({"name": name, "size": body.len(), "sha256": hash(body.as_bytes())})).collect();
    let mut manifest = serde_json::json!({"format_version": 1, "completed_id": input.completed_id, "source_hash": hash(input.spec.as_bytes()), "files": files, "completeness": input.completeness});
    let identity = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    manifest["bundle_id"] = hash(&identity).into();
    manifest["test_status"] = input.test_status.clone().into();
    let parent = destination.parent().ok_or("destination needs parent")?;
    let name = destination
        .file_name()
        .ok_or("invalid destination")?
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
    std::fs::create_dir(&temporary).map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        for (name, body) in docs {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary.join(name))
                .map_err(|e| e.to_string())?;
            f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        }
        std::fs::write(
            temporary.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, destination).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&temporary);
    }
    result?;
    Ok(manifest)
}
