use magi_provider::sdd::{parse_candidate, SpecInput};
use serde_json::{json, Value};
const HEADINGS: [&str; 24] = [
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
];
fn markdown() -> String {
    let mut s = HEADINGS
        .iter()
        .map(|h| format!("{h}\n검색 제공\n"))
        .collect::<String>();
    s.push_str("## 22. 수동 검증 확인 목록\n검색 제공\n## 23. AI 코딩 에이전트 지침\n검색 제공\n");
    s
}
fn input(importance: &str) -> SpecInput {
    serde_json::from_value(json!({"request":"사용자가 검색을 요청함","answers":[],"decisions":[],"base_revision":"abc","rubric":[{"id":"r1","requirement_id":"req1","requirement":"검색 제공","required":true,"importance":importance,"allowed_sections":["## 7. 필수 요구사항"]}],"max_revisions":4})).unwrap()
}
fn candidate() -> Value {
    json!({"policy_version":4,"draft":{"title":"검색","scope":["검색 제공"],"requirements":["검색 제공"],"acceptance":["검색 결과 확인"],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":"abc","markdown":markdown(),"source_links":["req1"],"constraints":["제약 없음"],"tasks":[{"id":"task1","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/readme.txt:검색"]}],"reused_decisions":[],"task_bindings":[{"task_id":"task1","goal":"검색 구현","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]},"assumptions":[],"evidence":[{"requirement_id":"req1","source":"request","quote":"검색"}],"verdicts":[{"rubric_id":"r1","passed":true,"section":"## 7. 필수 요구사항","quote":"검색 제공","reason":"요구사항 포함","issue_checklist_id":null}],"issues":[],"revision_directives":[],"score_reason":"기준 충족","proposed_score":10,"score":10})
}
fn failed(severity: &str) -> Value {
    let mut c = candidate();
    c["verdicts"][0]["passed"] = json!(false);
    c["verdicts"][0]["issue_checklist_id"] = json!("i1");
    c["issues"] = json!([{"checklist_id":"i1","requirement_id":"req1","rubric_ids":["r1"],"severity":severity,"evidence":[{"source":"## 7. 필수 요구사항","quote":"검색 제공"}]}]);
    c["revision_directives"] = json!(["검색 요구사항 보완"]);
    c
}
#[test]
fn full_document_and_evidence_validation() {
    let c = candidate();
    assert_eq!(
        parse_candidate(&c.to_string(), &input("core"))
            .unwrap()
            .score,
        10
    );
    let mut b = c.clone();
    b["draft"]["markdown"] = json!("# 최종 에이전트 명세");
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    b = c.clone();
    b["evidence"][0]["quote"] = json!("없는 말");
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    b = c.clone();
    b["policy_version"] = json!(3);
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
}
#[test]
fn severity_caps_and_link_validation() {
    for (importance, severity, cap) in [
        ("core", "major", 4),
        ("critical", "major", 7),
        ("secondary", "minor", 9),
    ] {
        let c = failed(severity);
        let r = parse_candidate(&c.to_string(), &input(importance)).unwrap();
        assert_eq!(r.score, cap);
        if importance != "secondary" {
            assert!(!r.draft.required_failures.is_empty());
        }
        let mut b = c.clone();
        b["issues"][0]["requirement_id"] = json!("other");
        assert!(parse_candidate(&b.to_string(), &input(importance)).is_err());
        b = c.clone();
        b["revision_directives"] = json!([]);
        assert!(parse_candidate(&b.to_string(), &input(importance)).is_err());
    }
}
#[test]
fn rejects_duplicate_unknown_and_disallowed_evidence() {
    let c = candidate();
    let mut b = c.clone();
    b["verdicts"] = json!([c["verdicts"][0], c["verdicts"][0]]);
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    b = c.clone();
    b["verdicts"][0]["section"] = json!("## 8. 권장 요구사항");
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    b = c.clone();
    b["verdicts"][0]["quote"] = json!("허위");
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
}

#[test]
fn rejects_unlinked_sources_or_invented_decisions() {
    let c = candidate();
    let mut b = c.clone();
    b["draft"]["source_links"] = json!(["unknown"]);
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    b = c.clone();
    b["draft"]["reused_decisions"] = json!(["fabricated"]);
    assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
}

#[test]
fn listed_required_failure_prevents_perfect_score() {
    let mut c = candidate();
    c["draft"]["required_failures"] = json!(["아직 미결"]);
    assert!(
        parse_candidate(&c.to_string(), &input("core"))
            .unwrap()
            .score
            < 10
    );
}

#[test]
fn rejects_missing_invented_or_duplicate_task_bindings() {
    let c = candidate();
    for bad_bindings in [
        json!([]),
        json!([{"task_id":"unknown","goal":"구현","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]),
        json!([{"task_id":"task1","goal":"구현","requirement_ids":["requirement-99"],"acceptance_ids":["acceptance-0"]}]),
        json!([{"task_id":"task1","goal":"","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]),
    ] {
        let mut b = c.clone();
        b["draft"]["task_bindings"] = bad_bindings;
        assert!(parse_candidate(&b.to_string(), &input("core")).is_err());
    }
}

#[test]
fn failed_secondary_and_unlinked_issues_create_approval_barriers() {
    let secondary = failed("minor");
    let r = parse_candidate(&secondary.to_string(), &input("secondary")).unwrap();
    assert!(r.draft.optional_issues.iter().any(|v| v.contains("i1")));
    let mut standalone = candidate();
    standalone["issues"] = json!([{"checklist_id":"standalone","requirement_id":"req1","rubric_ids":[],"severity":"blocking","evidence":[]}]);
    standalone["revision_directives"] = json!(["수정"]);
    let r = parse_candidate(&standalone.to_string(), &input("core")).unwrap();
    assert!(r
        .draft
        .required_failures
        .iter()
        .any(|v| v.contains("standalone")));
}

#[test]
fn task_verification_requires_supported_structured_commands() {
    let original = candidate();
    for validation in [
        "cargo test",
        "read_contains:../secret:value",
        "read_contains:src/a:",
        "exec_json:{\"program\":\"cargo\",\"args\":\"test\",\"cwd\":\"src\"}",
        "exec_json:{\"program\":\"\",\"args\":[],\"cwd\":\"src\"}",
        "exec_json:{\"program\":\"cargo\",\"args\":[\"test\"],\"cwd\":\"../src\"}",
    ] {
        let mut c = original.clone();
        c["draft"]["tasks"][0]["validation"] = json!([validation]);
        assert!(
            parse_candidate(&c.to_string(), &input("core")).is_err(),
            "{validation}"
        );
    }
    for validation in [
        "read_contains:src/readme.txt:검색",
        "exec_json:{\"program\":\"cargo\",\"args\":[\"test\"],\"cwd\":\"src\"}",
    ] {
        let mut c = original.clone();
        c["draft"]["tasks"][0]["validation"] = json!([validation]);
        assert!(
            parse_candidate(&c.to_string(), &input("core")).is_ok(),
            "{validation}"
        );
    }
}

#[test]
fn unknown_verification_can_be_returned_as_mandatory_question() {
    let mut c = candidate();
    c["draft"]["tasks"][0]["validation"] = json!([]);
    c["draft"]["mandatory_questions"] = json!(["어떤 명령으로 검증하나요?"]);
    let result = parse_candidate(&c.to_string(), &input("core"));
    assert!(result.is_ok());
}
