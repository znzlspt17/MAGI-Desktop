use domain::{ApprovedSpec, Project, RunEvent, SecretString, SpecDraft, VerificationEvidence};
use serde_json::json;

#[test]
fn draft_reads_older_data_with_defaulted_fields() {
    let draft: SpecDraft = serde_json::from_value(json!({
        "title": "test", "scope": ["src"], "requirements": ["r1"],
        "acceptance": ["a1"], "mandatory_questions": [], "required_failures": [],
        "optional_issues": [], "base_revision": "revision"
    }))
    .unwrap();
    assert_eq!(draft.scope, ["src"]);
    assert!(draft.tasks.is_empty());
    assert!(draft.task_bindings.is_empty());
    assert!(draft.markdown.is_empty());
}

#[test]
fn approved_spec_round_trip_preserves_task_ownership_and_bindings() {
    let source = json!({
        "version": 1, "hash": "spec-id", "base_revision": "rev", "scope": ["src"],
        "requirements": ["r1"], "acceptance": ["a1"],
        "tasks": [{"id":"t1", "dependencies":[], "owned_paths":["src"], "validation":["cargo test"]}],
        "task_bindings": [{"task_id":"t1", "goal":"implement", "requirement_ids":["r1"], "acceptance_ids":["a1"]}]
    });
    let spec: ApprovedSpec = serde_json::from_value(source).unwrap();
    let encoded = serde_json::to_value(&spec).unwrap();
    assert_eq!(encoded["tasks"][0]["owned_paths"][0], "src");
    assert_eq!(encoded["task_bindings"][0]["acceptance_ids"][0], "a1");
    assert_eq!(encoded["version"], 1);
}

#[test]
fn project_and_evidence_accept_missing_added_fields() {
    let draft = serde_json::to_value(SpecDraft::default()).unwrap();
    let project: Project = serde_json::from_value(json!({
        "id":"p1", "name":"test", "path":"src", "draft": draft,
        "draft_hash":"draft-id", "approved":null
    }))
    .unwrap();
    assert_eq!(project.context_version, 0);
    assert!(project.agent_configs.is_empty());
    let evidence: VerificationEvidence = serde_json::from_value(json!({
        "revision":"rev", "spec_hash":"spec-id", "command":"cargo test", "passed":true, "output":"ok"
    })).unwrap();
    assert!(evidence.requirement_ids.is_empty());
    assert!(evidence.verification_id.is_empty());
    let event = RunEvent {
        sequence: 7,
        run_id: "run".into(),
        attempt: 2,
        kind: "done".into(),
        message: "ok".into(),
    };
    assert_eq!(serde_json::to_value(event).unwrap()["sequence"], 7);
}

#[test]
fn secret_debug_never_shows_its_value() {
    let fake = SecretString("FAKE_VALUE_ONLY".into());
    assert_eq!(format!("{fake:?}"), "[redacted]");
}
