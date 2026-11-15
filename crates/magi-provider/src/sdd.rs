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
