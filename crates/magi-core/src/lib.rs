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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub command: Command,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub result: Result<ResponseData, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
// 2026-11-14: 기존 API 생성자 호환성을 유지하며 전송 프레임에서 크기를 제한한다.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    Status,
    ListStorageFailures,
    RetryCompletionSave {
        run_id: String,
    },
    SetProviderCredential {
        project_id: String,
        endpoint: String,
        credential: SecretString,
    },
    ConfigureModel {
        project_id: String,
        provider: String,
        model: String,
        capabilities: ModelCapabilities,
    },
    SubmitInterview {
        project_id: String,
        message: String,
    },
    GenerateSpec {
        project_id: String,
    },
    ConfigureAgents {
        project_id: String,
        agents: Vec<AgentConfig>,
    },
    SetAutonomy {
        project_id: String,
        mode: String,
    },
    ApproveTool {
        run_id: String,
        approval_id: String,
        approved: bool,
    },
    SubmitTool {
        run_id: String,
        tool: String,
        arguments: serde_json::Value,
    },
    GetRun {
        run_id: String,
    },
    ListRuns {
        project_id: String,
        offset: usize,
        limit: usize,
    },
    GetDiff {
        run_id: String,
    },
    ConfigureEmbedding {
        project_id: String,
        endpoint: String,
        model: String,
        version: String,
        dimensions: usize,
        credential: SecretString,
    },
    IndexKnowledge {
        project_id: String,
        document_id: String,
    },
    CancelKnowledgeIndex {
        project_id: String,
        document_id: String,
    },
    KnowledgeIndexStatus {
        project_id: String,
        document_id: String,
    },
    ExportRunPackage {
        run_id: String,
        destination: String,
    },
    SearchKnowledgePage {
        project_id: String,
        query: String,
        offset: usize,
        limit: usize,
    },
    GetGraphPage {
        project_id: String,
        offset: usize,
        limit: usize,
    },
    GetKnowledgeSource {
        project_id: String,
        document_id: String,
    },
    PutKnowledge {
        project_id: String,
        document: serde_json::Value,
    },
    SetKnowledgeEmbedding {
        project_id: String,
        document_id: String,
        version: u64,
        model: serde_json::Value,
        vector: Vec<f32>,
    },
    SearchKnowledgeVector {
        project_id: String,
        model: serde_json::Value,
        vector: Vec<f32>,
        offset: usize,
        limit: usize,
    },
    ReviseDecision {
        project_id: String,
        id: String,
        text: String,
    },
    SelectDecision {
        project_id: String,
        id: String,
        version: u64,
    },
    RequestMcpApproval {
        run_id: String,
        endpoint: String,
        name: String,
        arguments: serde_json::Value,
        cwd: String,
    },
    CallMcpApproved {
        run_id: String,
        approval_id: String,
    },
    DiscoverMcpOAuth {
        project_id: String,
        metadata_url: String,
    },
    BeginMcpOAuth {
        project_id: String,
        metadata_url: String,
        client_id: String,
        redirect_uri: String,
    },
    CompleteMcpOAuth {
        project_id: String,
        // 2026-11-15: callback의 code와 state가 Debug 출력으로 노출되지 않게 한다.
        callback: SecretString,
    },
    ConfigureMcpPolicy {
        project_id: String,
        allowlist: Vec<String>,
    },
    SearchKnowledge {
        project_id: String,
        query: String,
        semantic: bool,
    },
    GetGraph {
        project_id: String,
    },
    ListHistory {
        project_id: String,
    },
    ExportPackage {
        project_id: String,
        destination: String,
    },
    SaveDecision {
        project_id: String,
        decision: String,
    },
    ListDecisions {
        project_id: String,
    },
    ConfigureMcp {
        project_id: String,
        endpoint: String,
    },
    ListMcpTools {
        project_id: String,
    },
    CallMcpTool {
        project_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ListProjects,
    CreateProject {
        name: String,
        path: String,
    },
    GetProject {
        project_id: String,
    },
    SaveDraft {
        project_id: String,
        draft: SpecDraft,
    },
    ApproveSpec {
        project_id: String,
        expected_draft_hash: String,
        accepted_optional: Vec<String>,
    },
    ResumeAgent {
        run_id: String,
        mock_script: Vec<serde_json::Value>,
    },
    StartAgent {
        run_id: String,
        mock_script: Vec<serde_json::Value>,
    },
    StartRun {
        project_id: String,
        expected_approved_hash: String,
    },
    CancelRun {
        run_id: String,
    },
    ListEvents {
        run_id: String,
        after: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
// 2026-11-14: UI 계약 호환성을 위해 응답 variant를 유지하며 프레임 크기를 제한한다.
#[allow(clippy::large_enum_variant)]
pub enum ResponseData {
    Status { message: String },
    Projects(Vec<Project>),
    Project(Project),
    Approved(ApprovedSpec),
    Run { run_id: String },
    Events(Vec<RunEvent>),
    Data(serde_json::Value),
    Empty,
}
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
