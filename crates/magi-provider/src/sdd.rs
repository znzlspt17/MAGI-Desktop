use crate::ProviderError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const POLICY_VERSION: u32 = 4;
pub const HEADINGS: [&str; 26] = [
    "# 최종 에이전트 명세",
    "## 1. 목표",
    "## 2. 배경",
    "## 3. 사용자 의도",
    "## 4. 범위",
    "### 4.1 범위 포함",
    "### 4.2 범위 제외",
    "## 5. 용어 정의",
    "## 6. 근거 요약",
    "## 7. 필수 요구사항",
    "## 8. 권장 요구사항",
    "## 9. 선택 요구사항",
    "## 10. 금지 행동",
    "## 11. 입력 계약",
    "## 12. 출력 계약",
    "## 13. 아키텍처",
    "## 14. 모듈 책임",
    "## 15. 데이터 흐름",
    "## 16. 오류 처리 정책",
    "## 17. 설정 정책",
    "## 18. 저장 및 산출물 정책",
    "## 19. 구현 순서",
    "## 20. 수용 기준",
    "## 21. 테스트 계획",
    "## 22. 수동 검증 확인 목록",
    "## 23. AI 코딩 에이전트 지침",
];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RubricItem {
    pub id: String,
    pub requirement_id: String,
    pub requirement: String,
    pub importance: String,
    pub allowed_sections: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpecInput {
    pub request: String,
    pub answers: Vec<String>,
    pub decisions: Vec<String>,
    pub base_revision: String,
    pub rubric: Vec<RubricItem>,
    pub max_revisions: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub requirement_id: String,
    pub source: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub rubric_id: String,
    pub passed: bool,
    pub section: String,
    pub quote: String,
    pub reason: String,
    pub issue_checklist_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueEvidence {
    pub source: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub checklist_id: String,
    pub requirement_id: String,
    pub rubric_ids: Vec<String>,
    pub severity: String,
    pub evidence: Vec<IssueEvidence>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpecResult {
    pub policy_version: u32,
    pub draft: magi_core::SpecDraft,
    pub assumptions: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub verdicts: Vec<Verdict>,
    pub issues: Vec<Issue>,
    pub revision_directives: Vec<String>,
    pub score_reason: String,
    pub proposed_score: u32,
    pub score: u32,
    #[serde(default)]
    pub rubric_hash: String,
    #[serde(default)]
    pub draft_hash: String,
}
fn bad() -> ProviderError {
    ProviderError("Invalid specification response")
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// 2026-11-14: 검증 지시는 셸 문장이 아니라 실행기가 이해할 수 있는 명시적 인자 계약이어야 한다.
fn safe_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with(['/', '\\']) {
        return false;
    }
    path.split(['/', '\\']).all(|part| {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return false;
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) {
            return false;
        }
        !["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
    })
}
fn validation_supported(task: &magi_core::TaskSpec, entry: &str) -> bool {
    let inside = |path: &str| {
        if !safe_relative(path) {
            return false;
        }
        let path = path.replace('\\', "/").to_lowercase();
        task.owned_paths.iter().any(|scope| {
            let scope = scope
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_lowercase();
            !scope.is_empty() && (path == scope || path.starts_with(&(scope + "/")))
        })
    };
    if let Some(raw) = entry.strip_prefix("read_contains:") {
        return raw
            .split_once(':')
            .is_some_and(|(path, expected)| inside(path) && !expected.is_empty());
    }
    if let Some(raw) = entry.strip_prefix("exec_json:") {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Command {
            program: String,
            args: Vec<String>,
            cwd: String,
        }
        return serde_json::from_str::<Command>(raw).is_ok_and(|command| {
            !command.program.trim().is_empty()
                && !command.program.contains('\0')
                && command.args.len() <= 128
                && command.args.iter().all(|a| !a.contains('\0'))
                && inside(&command.cwd)
        });
    }
    false
}
// 2026-11-14: 생성 여부와 무관하게 문서의 구조와 작업 추적성을 같은 규칙으로 검증한다.
pub fn validate_draft_structure(draft: &magi_core::SpecDraft) -> Result<Vec<usize>, ProviderError> {
    let lines: Vec<&str> = draft.markdown.lines().collect();
    let mut positions = Vec::new();
    for heading in HEADINGS {
        let found: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| (*l == heading).then_some(i))
            .collect();
        if found.len() != 1 || positions.last().is_some_and(|p| *p >= found[0]) {
            return Err(bad());
        }
        positions.push(found[0]);
    }
    for (i, start) in positions.iter().enumerate() {
        let end = positions.get(i + 1).copied().unwrap_or(lines.len());
        if i != 4 && !lines[start + 1..end].iter().any(|s| !s.trim().is_empty()) {
            return Err(bad());
        }
    }
    if draft.tasks.is_empty()
        || draft.tasks.iter().any(|t| {
            t.id.trim().is_empty()
                || (t.validation.is_empty()
                    && !draft
                        .mandatory_questions
                        .iter()
                        .any(|q| !q.trim().is_empty()))
                || t.validation
                    .iter()
                    .any(|entry| !validation_supported(t, entry))
        })
    {
        return Err(bad());
    }
    // 2026-11-14: 각 실행 작업을 명세의 요구사항과 수용 기준에 연결해 추적 가능한 실행만 허용한다.
    let task_ids: HashSet<&str> = draft.tasks.iter().map(|task| task.id.as_str()).collect();
    if task_ids.len() != draft.tasks.len() {
        return Err(bad());
    }
    let requirements: HashSet<String> = (0..draft.requirements.len())
        .map(|i| format!("requirement-{i}"))
        .collect();
    let acceptance: HashSet<String> = (0..draft.acceptance.len())
        .map(|i| format!("acceptance-{i}"))
        .collect();
    let mut bound = HashSet::new();
    for binding in &draft.task_bindings {
        if !task_ids.contains(binding.task_id.as_str())
            || !bound.insert(binding.task_id.as_str())
            || binding.goal.trim().is_empty()
            || binding.requirement_ids.is_empty()
            || binding.acceptance_ids.is_empty()
            || binding
                .requirement_ids
                .iter()
                .any(|id| !requirements.contains(id))
            || binding
                .acceptance_ids
                .iter()
                .any(|id| !acceptance.contains(id))
            || binding.requirement_ids.iter().collect::<HashSet<_>>().len()
                != binding.requirement_ids.len()
            || binding.acceptance_ids.iter().collect::<HashSet<_>>().len()
                != binding.acceptance_ids.len()
        {
            return Err(bad());
        }
    }
    if bound.len() != task_ids.len() {
        return Err(bad());
    }
    Ok(positions)
}

// 2026-11-14: rubric 판정은 지정된 초안 섹션의 인용과 원문 근거, 연결된 결함을 확인한 뒤 점수를 제한한다.
pub fn parse_candidate(raw: &str, input: &SpecInput) -> Result<SpecResult, ProviderError> {
    let mut c: SpecResult = serde_json::from_str(raw).map_err(|_| bad())?;
    if c.policy_version != POLICY_VERSION
        || input.request.trim().is_empty()
        || input.base_revision.trim().is_empty()
        || input.rubric.is_empty()
        || c.draft.base_revision != input.base_revision
        || c.draft.title.trim().is_empty()
        || c.proposed_score > 10
        || c.score_reason.trim().is_empty()
    {
        return Err(bad());
    }
    let positions = validate_draft_structure(&c.draft)?;
    let lines: Vec<&str> = c.draft.markdown.lines().collect();
    let mut criteria = HashMap::new();
    let mut requirement_ids = HashSet::new();
    for item in &input.rubric {
        if item.id.trim().is_empty()
            || item.requirement_id.trim().is_empty()
            || item.requirement.trim().is_empty()
            || !matches!(item.importance.as_str(), "core" | "critical" | "secondary")
            || item.allowed_sections.is_empty()
            || item
                .allowed_sections
                .iter()
                .any(|s| !HEADINGS.contains(&s.as_str()))
            || item.allowed_sections.iter().collect::<HashSet<_>>().len()
                != item.allowed_sections.len()
            || criteria.insert(item.id.as_str(), item).is_some()
        {
            return Err(bad());
        }
        requirement_ids.insert(item.requirement_id.as_str());
    }
    if c.draft.source_links.is_empty()
        || c.draft
            .source_links
            .iter()
            .any(|id| !requirement_ids.contains(id.as_str()))
        || c.draft
            .reused_decisions
            .iter()
            .any(|d| !input.decisions.contains(d))
    {
        return Err(bad());
    }
    let mut grounded = HashSet::new();
    for e in &c.evidence {
        let source = if e.source == "request" {
            Some(input.request.as_str())
        } else if let Some(i) = e
            .source
            .strip_prefix("answer:")
            .and_then(|n| n.parse::<usize>().ok())
        {
            input.answers.get(i).map(String::as_str)
        } else if let Some(i) = e
            .source
            .strip_prefix("decision:")
            .and_then(|n| n.parse::<usize>().ok())
        {
            input.decisions.get(i).map(String::as_str)
        } else {
            None
        };
        if !requirement_ids.contains(e.requirement_id.as_str())
            || e.quote.trim().is_empty()
            || !source.is_some_and(|s| s.contains(&e.quote))
        {
            return Err(bad());
        }
        grounded.insert(e.requirement_id.as_str());
    }
    if grounded != requirement_ids {
        return Err(bad());
    }
    let mut issues = HashMap::new();
    for issue in &c.issues {
        if issue.checklist_id.trim().is_empty()
            || !matches!(issue.severity.as_str(), "blocking" | "major" | "minor")
            || issue.rubric_ids.iter().collect::<HashSet<_>>().len() != issue.rubric_ids.len()
            || issues.insert(issue.checklist_id.as_str(), issue).is_some()
        {
            return Err(bad());
        }
        for id in &issue.rubric_ids {
            if criteria
                .get(id.as_str())
                .is_none_or(|r| r.requirement_id != issue.requirement_id)
            {
                return Err(bad());
            }
        }
    }
    let mut seen = HashSet::new();
    let mut core_fail = false;
    let mut critical = false;
    let mut failed = false;
    for v in &c.verdicts {
        let item = criteria.get(v.rubric_id.as_str()).ok_or_else(bad)?;
        if !seen.insert(v.rubric_id.as_str())
            || v.reason.trim().is_empty()
            || !item.allowed_sections.contains(&v.section)
        {
            return Err(bad());
        }
        let idx = HEADINGS
            .iter()
            .position(|h| *h == v.section)
            .ok_or_else(bad)?;
        let section = lines
            [positions[idx] + 1..positions.get(idx + 1).copied().unwrap_or(lines.len())]
            .join("\n");
        if !v.quote.is_empty() && !section.contains(&v.quote) {
            return Err(bad());
        }
        let related: Vec<_> = c
            .issues
            .iter()
            .filter(|i| i.rubric_ids.contains(&v.rubric_id))
            .collect();
        if v.passed {
            if v.quote.trim().is_empty() || v.issue_checklist_id.is_some() || !related.is_empty() {
                return Err(bad());
            }
        } else {
            failed = true;
            let issue = v
                .issue_checklist_id
                .as_deref()
                .and_then(|id| issues.get(id))
                .ok_or_else(bad)?;
            if related.len() != 1
                || related[0].checklist_id != issue.checklist_id
                || (item.importance != "secondary" && issue.severity == "minor")
                || !issue
                    .evidence
                    .iter()
                    .any(|e| e.source == v.section && e.quote == v.quote)
            {
                return Err(bad());
            }
            if item.importance == "core" {
                core_fail = true;
            }
            if item.importance != "secondary" {
                critical = true;
                if !c.draft.required_failures.contains(&item.requirement) {
                    c.draft.required_failures.push(item.requirement.clone());
                }
            }
        }
    }
    if seen.len() != criteria.len()
        || c.issues
            .iter()
            .flat_map(|i| &i.rubric_ids)
            .any(|id| !seen.contains(id.as_str()))
    {
        return Err(bad());
    }
    if (!c.issues.is_empty() || failed)
        && (c.revision_directives.is_empty()
            || c.revision_directives.iter().any(|s| s.trim().is_empty()))
    {
        return Err(bad());
    }
    if c.issues
        .iter()
        .any(|i| i.rubric_ids.is_empty() && matches!(i.severity.as_str(), "blocking" | "major"))
    {
        critical = true;
    }
    // 2026-11-14: 검증된 결함을 승인에 쓰이는 초안 플래그로 옮겨 모델이 누락해도 승인 장벽을 유지한다.
    for issue in &c.issues {
        let secondary = !issue.rubric_ids.is_empty()
            && issue.rubric_ids.iter().all(|id| {
                criteria
                    .get(id.as_str())
                    .is_some_and(|item| item.importance == "secondary")
            });
        let label = format!("{}: {}", issue.checklist_id, issue.requirement_id);
        let target = if secondary || (issue.rubric_ids.is_empty() && issue.severity == "minor") {
            &mut c.draft.optional_issues
        } else {
            &mut c.draft.required_failures
        };
        if !target.contains(&label) {
            target.push(label);
        }
    }
    let cap = if core_fail {
        4
    } else if critical || !c.draft.required_failures.is_empty() {
        7
    } else if failed || !c.issues.is_empty() || !c.draft.optional_issues.is_empty() {
        9
    } else {
        10
    };
    c.score = c.proposed_score.min(cap);
    c.rubric_hash = hash(
        &serde_json::to_vec(&(
            POLICY_VERSION,
            &input.request,
            &input.answers,
            &input.decisions,
            &input.rubric,
        ))
        .map_err(|_| bad())?,
    );
    c.draft_hash = hash(&serde_json::to_vec(&c.draft).map_err(|_| bad())?);
    Ok(c)
}

// 2026-11-14: 정체가 두 번 이어지면 같은 기준으로 평가한 최고점 초안을 복원한다.
pub async fn generate_spec(
    provider: &crate::OpenAiProvider,
    input: SpecInput,
    cancel: tokio_util::sync::CancellationToken,
    events: tokio::sync::mpsc::Sender<crate::ProviderEvent>,
) -> Result<SpecResult, ProviderError> {
    generate_spec_inner(
        provider,
        input,
        cancel,
        events,
        None,
        crate::DEFAULT_MAX_OUTPUT_TOKENS,
    )
    .await
}
/// 직렬화 바이트에 고정 여유분을 더해 추정하며 정확한 토크나이저 결과는 아니다.
pub async fn generate_spec_with_context(
    provider: &crate::OpenAiProvider,
    input: SpecInput,
    cancel: tokio_util::sync::CancellationToken,
    events: tokio::sync::mpsc::Sender<crate::ProviderEvent>,
    context_tokens: u64,
    max_output_tokens: u64,
) -> Result<SpecResult, ProviderError> {
    if context_tokens == 0 || max_output_tokens == 0 {
        return Err(ProviderError("Invalid specification input"));
    }
    generate_spec_inner(
        provider,
        input,
        cancel,
        events,
        Some(context_tokens),
        max_output_tokens,
    )
    .await
}
async fn generate_spec_inner(
    provider: &crate::OpenAiProvider,
    input: SpecInput,
    cancel: tokio_util::sync::CancellationToken,
    events: tokio::sync::mpsc::Sender<crate::ProviderEvent>,
    context_tokens: Option<u64>,
    max_output_tokens: u64,
) -> Result<SpecResult, ProviderError> {
    if input.max_revisions > 5 {
        return Err(ProviderError("Invalid specification input"));
    }
    let mut best: Option<SpecResult> = None;
    let mut feedback = String::new();
    let mut stale = 0;
    for _ in 0..=input.max_revisions {
        if cancel.is_cancelled() {
            return Err(ProviderError("Request cancelled"));
        }
        let prompt = serde_json::json!({"instruction":"입력에서 확인된 근거와 추정을 구분하여 JSON만 반환하세요. draft에는 기본 명세 필드와 markdown, source_links, constraints, tasks, reused_decisions, task_bindings를 포함하고 각 task_bindings는 task_id, goal, requirement_ids, acceptance_ids를 갖습니다. ID는 draft.requirements의 0부터 시작하는 requirement-0, requirement-1과 draft.acceptance의 acceptance-0, acceptance-1 형식이며 각 task마다 하나의 binding과 비어 있지 않은 goal, 두 ID 목록이 필요합니다. markdown에는 headings 전체를 순서대로 채우세요. 각 tasks 항목의 owned_paths는 프로젝트 상대 경로로 적고 validation은 실행 가능한 형식만 허용합니다. 예: read_contains:src/readme.txt:기대문구 또는 exec_json:{\"program\":\"cargo\",\"args\":[\"test\"],\"cwd\":\"src\"}. 경로와 cwd는 해당 작업 owned_paths 범위 내부여야 합니다. exec_json은 shell 없이 program과 문자열 args를 직접 실행합니다. 검증 절차가 불명확하면 성공을 추정하지 말고 mandatory_questions에 질문하세요. policy_version은 4입니다. rubric 항목마다 verdicts에 rubric_id, passed, section, quote, reason, issue_checklist_id를 넣고 실패는 issues의 checklist_id, requirement_id, rubric_ids, severity, evidence(source,quote)에 연결하세요. evidence에는 각 requirement_id와 실제 request, answer:인덱스, decision:인덱스의 원문 인용을 넣으세요. assumptions는 추정, revision_directives는 실패 개선 지침, score_reason과 proposed_score는 0~10 판정, score는 proposed_score와 같은 값입니다. 새 사용자 결정은 mandatory_questions로 남기세요.","headings":HEADINGS,"input":input,"feedback":feedback});
        // 2026-11-14: 보완 이력이 늘어나는 매 요청마다 입력 바이트와 출력 예산을 함께 검사한다.
        let prompt = prompt.to_string();
        if let Some(limit) = context_tokens {
            let encoded = serde_json::to_vec(&prompt).map_err(|_| bad())?;
            let encoded_model = serde_json::to_vec(&provider.config.model).map_err(|_| bad())?;
            let estimate = u64::try_from(encoded.len())
                .ok()
                .and_then(|n| n.checked_add(encoded_model.len() as u64))
                .and_then(|n| n.checked_add(1024))
                .and_then(|n| n.checked_add(max_output_tokens))
                .ok_or(ProviderError("Specification context limit exceeded"))?;
            if estimate > limit {
                return Err(ProviderError("Specification context limit exceeded"));
            }
        }
        let response = provider
            .generate_with_limit(
                serde_json::Value::String(prompt),
                vec![],
                cancel.clone(),
                events.clone(),
                max_output_tokens,
            )
            .await?;
        let candidate = parse_candidate(&response.text, &input)?;
        if !candidate.draft.mandatory_questions.is_empty() {
            if let Some(mut prior) = best.filter(|b| b.score >= candidate.score) {
                for q in candidate.draft.mandatory_questions {
                    if !prior.draft.mandatory_questions.contains(&q) {
                        prior.draft.mandatory_questions.push(q);
                    }
                }
                for f in candidate.draft.required_failures {
                    if !prior.draft.required_failures.contains(&f) {
                        prior.draft.required_failures.push(f);
                    }
                }
                for issue in candidate.draft.optional_issues {
                    if !prior.draft.optional_issues.contains(&issue) {
                        prior.draft.optional_issues.push(issue);
                    }
                }
                if !prior.draft.required_failures.is_empty() {
                    prior.score = prior.score.min(7);
                }
                if !prior.draft.optional_issues.is_empty() {
                    prior.score = prior.score.min(9);
                }
                prior.draft_hash = hash(&serde_json::to_vec(&prior.draft).map_err(|_| bad())?);
                return Ok(prior);
            }
            return Ok(candidate);
        }
        let improved = best.as_ref().is_none_or(|b| candidate.score > b.score);
        if improved {
            best = Some(candidate.clone());
            stale = 0;
        } else {
            stale += 1;
        }
        if (candidate.score == 10 && candidate.draft.required_failures.is_empty()) || stale >= 2 {
            break;
        }
        feedback = serde_json::to_string(&candidate).map_err(|_| bad())?;
    }
    best.ok_or_else(bad)
}
