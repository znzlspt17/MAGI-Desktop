use domain::{ApprovedSpec, TaskBinding, TaskSpec};
use engine::agent_loop::validate_plan;

fn task(id: &str, dependencies: &[&str], path: &str) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        dependencies: dependencies.iter().map(|value| (*value).into()).collect(),
        owned_paths: vec![path.into()],
        validation: vec!["cargo test".into()],
    }
}

fn spec(tasks: Vec<TaskSpec>) -> ApprovedSpec {
    let bindings = tasks
        .iter()
        .map(|task| TaskBinding {
            task_id: task.id.clone(),
            goal: format!("작업 {}", task.id),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        })
        .collect();
    ApprovedSpec {
        version: 1,
        hash: "approved".into(),
        base_revision: "revision".into(),
        scope: vec!["src".into()],
        requirements: vec!["requirement".into()],
        acceptance: vec!["acceptance".into()],
        accepted_optional: vec![],
        markdown: String::new(),
        source_links: vec![],
        constraints: vec![],
        tasks,
        reused_decisions: vec![],
        task_bindings: bindings,
    }
}

#[test]
fn orders_dependencies_and_rejects_missing_dependency() {
    let valid = spec(vec![task("b", &["a"], "src/b"), task("a", &[], "src/a")]);
    assert_eq!(validate_plan(&valid).unwrap(), vec!["a", "b"]);
    let invalid = spec(vec![task("a", &["missing"], "src/a")]);
    assert_eq!(validate_plan(&invalid).unwrap_err(), "missing dependency");
}

#[test]
fn rejects_cycles_but_preserves_order_of_independent_overlapping_paths() {
    let cycle = spec(vec![task("a", &["b"], "src/a"), task("b", &["a"], "src/b")]);
    assert_eq!(validate_plan(&cycle).unwrap_err(), "dependency cycle");
    let overlap = spec(vec![task("a", &[], "src/a"), task("b", &[], "src/a/b")]);
    assert_eq!(validate_plan(&overlap).unwrap(), vec!["a", "b"]);
}

#[test]
fn allows_shared_path_when_dependency_orders_tasks() {
    let valid = spec(vec![
        task("b", &["a"], "src/shared.rs"),
        task("a", &[], "src/shared.rs"),
    ]);
    assert_eq!(validate_plan(&valid).unwrap(), vec!["a", "b"]);
}

#[test]
fn checks_scope_using_normalized_path_components() {
    let mut valid = spec(vec![task("a", &[], "sRc\\module\\file.rs")]);
    valid.scope = vec!["SRC/".into()];
    assert_eq!(validate_plan(&valid).unwrap(), vec!["a"]);
    for path in ["other/file", "src2/file"] {
        let invalid = spec(vec![task("a", &[], path)]);
        assert_eq!(
            validate_plan(&invalid).unwrap_err(),
            "owned path outside approved scope"
        );
    }
    for path in ["", "/src/a", "C:\\src\\a", "src/../a", "src/./a", "src//a"] {
        let invalid = spec(vec![task("a", &[], path)]);
        assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid owned path");
    }
}

#[test]
fn rejects_unbound_or_unknown_requirements() {
    let mut invalid = spec(vec![task("a", &[], "src/a")]);
    invalid.task_bindings[0].requirement_ids = vec!["requirement-9".into()];
    assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid task binding");
    invalid.task_bindings[0].requirement_ids = vec!["requirement-00".into()];
    assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid task binding");
    invalid.task_bindings[0].requirement_ids = vec!["requirement-0".into()];
    invalid.task_bindings[0].acceptance_ids = vec!["acceptance-9".into()];
    assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid task binding");
    invalid.task_bindings.clear();
    assert_eq!(validate_plan(&invalid).unwrap_err(), "missing task binding");
}

#[test]
fn rejects_incomplete_approval_and_invalid_task_fields() {
    let base = spec(vec![task("a", &[], "src/a")]);
    let mut invalid = base.clone();
    invalid.hash.clear();
    assert_eq!(
        validate_plan(&invalid).unwrap_err(),
        "approved specification and tasks required"
    );
    invalid = base.clone();
    invalid.version = 0;
    assert_eq!(
        validate_plan(&invalid).unwrap_err(),
        "approved specification and tasks required"
    );
    invalid = base.clone();
    invalid.base_revision = "  ".into();
    assert_eq!(
        validate_plan(&invalid).unwrap_err(),
        "approved specification and tasks required"
    );
    invalid = spec(vec![]);
    assert_eq!(
        validate_plan(&invalid).unwrap_err(),
        "approved specification and tasks required"
    );
    invalid = base.clone();
    invalid.tasks[0].validation = vec!["  ".into()];
    assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid task");
    invalid = base;
    invalid.tasks[0].owned_paths.clear();
    assert_eq!(validate_plan(&invalid).unwrap_err(), "invalid task");
}

#[test]
fn rejects_duplicate_tasks_and_bindings() {
    let duplicate_tasks = spec(vec![task("a", &[], "src/a"), task("a", &[], "src/b")]);
    assert_eq!(validate_plan(&duplicate_tasks).unwrap_err(), "invalid task");
    let mut duplicate_binding = spec(vec![task("a", &[], "src/a")]);
    duplicate_binding
        .task_bindings
        .push(duplicate_binding.task_bindings[0].clone());
    assert_eq!(
        validate_plan(&duplicate_binding).unwrap_err(),
        "invalid task binding"
    );
}
