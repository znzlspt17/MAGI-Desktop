use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct SecretString(pub String);
impl Drop for SecretString {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}
impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

// 2026-11-14: 전송 계층과 UI가 동일한 버전의 명시적 계약을 사용한다.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpecDraft {
    pub title: String,
    pub scope: Vec<String>,
    pub requirements: Vec<String>,
    pub acceptance: Vec<String>,
    pub mandatory_questions: Vec<String>,
    pub required_failures: Vec<String>,
    pub optional_issues: Vec<String>,
    pub base_revision: String,
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub source_links: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub reused_decisions: Vec<String>,
    #[serde(default)]
    pub task_bindings: Vec<TaskBinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovedSpec {
    pub version: u64,
    pub hash: String,
    pub base_revision: String,
    pub scope: Vec<String>,
    pub requirements: Vec<String>,
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub accepted_optional: Vec<String>,
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub source_links: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub reused_decisions: Vec<String>,
    #[serde(default)]
    pub task_bindings: Vec<TaskBinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub capabilities: ModelCapabilities,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub draft: SpecDraft,
    pub draft_hash: String,
    #[serde(default)]
    pub assessment: Option<AssessmentRef>,
    pub approved: Option<ApprovedSpec>,
    #[serde(default)]
    pub approval_version: u64,
    #[serde(default)]
    pub interview: Vec<String>,
    #[serde(default)]
    pub model_settings: Option<ModelSettings>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub approval_history: Vec<ApprovedSpec>,
    #[serde(default)]
    pub context_version: u64,
    #[serde(default)]
    pub agent_configs: Vec<AgentConfig>,
    #[serde(default)]
    pub autonomy_mode: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssessmentRef {
    pub draft_hash: String,
    pub context_version: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskBinding {
    pub task_id: String,
    pub goal: String,
    pub requirement_ids: Vec<String>,
    pub acceptance_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub dependencies: Vec<String>,
    pub owned_paths: Vec<String>,
    pub validation: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub role: String,
    pub model: String,
    pub tools: Vec<String>,
    pub limits: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub context_tokens: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub sequence: u64,
    pub run_id: String,
    pub attempt: u32,
    pub kind: String,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub revision: String,
    pub spec_hash: String,
    pub command: String,
    pub passed: bool,
    pub output: String,
    #[serde(default)]
    pub verification_id: String,
    #[serde(default)]
    pub worktree_hash: String,
    #[serde(default)]
    pub requirement_ids: Vec<String>,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
}
