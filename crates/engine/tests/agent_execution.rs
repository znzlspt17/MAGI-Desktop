use domain::{ApprovedSpec, TaskBinding, TaskSpec, VerificationEvidence};
use engine::agent_loop::{run_loop, LoopConfig, ModelDriver, RoleConfig, ToolDriver};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn fixture(tasks: Vec<TaskSpec>) -> ApprovedSpec {
    let task_bindings = tasks
        .iter()
        .map(|t| TaskBinding {
            task_id: t.id.clone(),
            goal: t.id.clone(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        })
        .collect();
    ApprovedSpec {
        version: 1,
        hash: "approved".into(),
        base_revision: "revision".into(),
        scope: vec!["src".into()],
        requirements: vec!["req".into()],
        acceptance: vec!["accept".into()],
        accepted_optional: vec![],
        markdown: String::new(),
        source_links: vec![],
        constraints: vec![],
        tasks,
        reused_decisions: vec![],
        task_bindings,
    }
}
fn task(id: &str, deps: &[&str], path: &str) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        dependencies: deps.iter().map(|s| (*s).into()).collect(),
        owned_paths: vec![path.into()],
        validation: vec!["fixture verification".into()],
    }
}
fn config() -> LoopConfig {
    let role = |name: &str| RoleConfig {
        name: name.into(),
        tools: vec![],
        max_rounds: 1,
    };
    LoopConfig {
        coordinator: role("coordinator"),
        implementer: role("implementer"),
        reviewer: role("reviewer"),
        max_tokens: 200_000,
        max_parallel_tasks: 2,
        timeout: Duration::from_secs(20),
    }
}
#[test]
fn fixture_verification_child() {
    if std::path::Path::new("enable-verification").exists() {
        assert_eq!(std::fs::read_to_string("fixture").unwrap(), "valid");
    }
}
struct Model {
    events: Mutex<Vec<String>>,
    concurrent: AtomicUsize,
    peak: AtomicUsize,
    reject: bool,
}
impl ModelDriver for Model {
    async fn generate(
        &self,
        role: &str,
        input: Value,
        _: Vec<Value>,
        _: CancellationToken,
        _: u64,
    ) -> Result<provider::Generation, String> {
        let request: Value = serde_json::from_str(input[0]["content"].as_str().unwrap()).unwrap();
        let id = request["task"]["id"].as_str().unwrap();
        if role == "reviewer" {
            assert!(!request["verification_evidence"]
                .as_array()
                .unwrap()
                .is_empty());
        }
        self.events
            .lock()
            .unwrap()
            .push(format!("{role}:{id}:start"));
        if role == "implementer" {
            let active = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(70)).await;
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
        }
        self.events.lock().unwrap().push(format!("{role}:{id}:end"));
        Ok(provider::Generation {
            text: if role == "reviewer" {
                json!({"approved":!self.reject}).to_string()
            } else {
                "done".into()
            },
            tool_calls: vec![],
            output: vec![],
            usage: Some(json!({"total_tokens":1})),
        })
    }
}
struct Runtime {
    root: PathBuf,
    events: Mutex<Vec<String>>,
}
impl ToolDriver for Runtime {
    fn progress(&self, stage: &str, id: &str, role: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("{stage}:{id}:{role}"));
    }
    async fn fresh(&self, _: &ApprovedSpec) -> Result<(), String> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &str,
        task: &TaskSpec,
        name: &str,
        _: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let context = engine::tools::ToolContext {
            worktree: self
                .root
                .join(&task.id)
                .canonicalize()
                .map_err(|_| "fixture path")?,
            allowed_paths: vec!["src".into()],
            owned_paths: vec!["src".into()],
            allowlist: vec!["read".into()],
            timeout: Duration::from_secs(5),
            max_output_bytes: 4096,
            side_effect_authorized: false,
        };
        Ok(
            engine::tools::execute_tool(context, name, arguments, cancel)
                .await?
                .content,
        )
    }
    async fn verify(
        &self,
        task: &TaskSpec,
        command: &str,
        _: CancellationToken,
    ) -> Result<VerificationEvidence, String> {
        let cwd = self.root.join(&task.id);
        let result = tokio::task::spawn_blocking(move || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fixture_verification_child", "--nocapture"])
                .current_dir(cwd)
                .output()
        })
        .await
        .map_err(|_| "join")?
        .map_err(|_| "spawn")?;
        Ok(VerificationEvidence {
            revision: "revision".into(),
            spec_hash: "approved".into(),
            verification_id: String::new(),
            command: command.into(),
            passed: result.status.success(),
            output: String::from_utf8_lossy(&result.stdout).into_owned(),
            worktree_hash: "fixture".into(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        })
    }
}
fn setup(ids: &[&str], invalid: Option<&str>) -> (PathBuf, Runtime, Model) {
    let root = std::env::temp_dir().join(format!("magi-loop-{}", uuid::Uuid::new_v4()));
    for id in ids {
        let path = root.join(id);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("enable-verification"), "").unwrap();
        std::fs::create_dir(path.join("src")).unwrap();
        std::fs::write(path.join("src/input.txt"), "real fixture").unwrap();
        std::fs::write(
            path.join("fixture"),
            if Some(*id) == invalid {
                "invalid"
            } else {
                "valid"
            },
        )
        .unwrap();
    }
    (
        root.clone(),
        Runtime {
            root,
            events: Mutex::new(vec![]),
        },
        Model {
            events: Mutex::new(vec![]),
            concurrent: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            reject: false,
        },
    )
}
#[tokio::test]
async fn runs_actual_verification_before_independent_review_and_schedules_ownership() {
    let (root, runtime, model) = setup(&["a", "b", "c", "d"], None);
    let plan = fixture(vec![
        task("a", &[], "src/shared"),
        task("b", &[], "src/shared/nested"),
        task("c", &[], "src/other"),
        task("d", &["a"], "src/last"),
    ]);
    run_loop(&model, &runtime, &plan, &config(), CancellationToken::new())
        .await
        .unwrap();
    let events = model.events.lock().unwrap();
    let pos = |needle: &str| events.iter().position(|x| x == needle).unwrap();
    assert!(pos("implementer:a:end") < pos("implementer:b:start"));
    assert!(pos("reviewer:a:end") < pos("coordinator:d:start"));
    assert_eq!(model.peak.load(Ordering::SeqCst), 2);
    let progress = runtime.events.lock().unwrap();
    for id in ["a", "b", "c", "d"] {
        let valid = progress
            .iter()
            .position(|s| s == &format!("validation_passed:{id}:implementer"))
            .unwrap();
        let review = progress
            .iter()
            .position(|s| s == &format!("role_started:{id}:reviewer"))
            .unwrap();
        assert!(valid < review);
    }
    drop(progress);
    drop(events);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn actual_failed_verification_and_reviewer_rejection_cannot_complete() {
    let (root, runtime, model) = setup(&["a"], Some("a"));
    let plan = fixture(vec![task("a", &[], "src/a")]);
    let error = run_loop(&model, &runtime, &plan, &config(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error, "validation failed or evidence invalid");
    assert!(!model
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.starts_with("reviewer:")));
    std::fs::write(root.join("a/fixture"), "valid").unwrap();
    let reject = Model {
        reject: true,
        events: Mutex::new(vec![]),
        concurrent: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    assert_eq!(
        run_loop(
            &reject,
            &runtime,
            &plan,
            &config(),
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "review rejected or invalid"
    );
    std::fs::remove_dir_all(root).unwrap();
}

struct ToolCallModel {
    name: &'static str,
    arguments: &'static str,
    calls: AtomicUsize,
}
impl ModelDriver for ToolCallModel {
    async fn generate(
        &self,
        _: &str,
        _: Value,
        _: Vec<Value>,
        _: CancellationToken,
        _: u64,
    ) -> Result<provider::Generation, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(provider::Generation {
            text: String::new(),
            output: vec![],
            usage: Some(json!({"total_tokens":1})),
            tool_calls: vec![provider::ToolCall {
                name: self.name.into(),
                call_id: "call".into(),
                arguments: self.arguments.into(),
            }],
        })
    }
}
#[tokio::test]
async fn rejects_tool_requests_before_execution_and_honors_cancel_and_budget() {
    let (root, runtime, _) = setup(&["a"], None);
    let plan = fixture(vec![task("a", &[], "src/a")]);
    let model = ToolCallModel {
        name: "read",
        arguments: "{invalid",
        calls: AtomicUsize::new(0),
    };
    assert_eq!(
        run_loop(&model, &runtime, &plan, &config(), CancellationToken::new())
            .await
            .unwrap_err(),
        "tool not allowed"
    );
    let mut cfg = config();
    cfg.coordinator.tools = vec![json!({"name":"read"})];
    assert_eq!(
        run_loop(&model, &runtime, &plan, &cfg, CancellationToken::new())
            .await
            .unwrap_err(),
        "invalid tool arguments"
    );
    let scalar = ToolCallModel {
        name: "read",
        arguments: "[]",
        calls: AtomicUsize::new(0),
    };
    assert_eq!(
        run_loop(&scalar, &runtime, &plan, &cfg, CancellationToken::new())
            .await
            .unwrap_err(),
        "invalid or duplicate tool call"
    );
    let before = model.calls.load(Ordering::SeqCst);
    cfg.max_tokens = 1;
    assert_eq!(
        run_loop(&model, &runtime, &plan, &cfg, CancellationToken::new())
            .await
            .unwrap_err(),
        "token budget insufficient for input"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), before);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        run_loop(&model, &runtime, &plan, &config(), cancel)
            .await
            .unwrap_err(),
        "cancelled"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), before);
    std::fs::remove_dir_all(root).unwrap();
}

struct RealReadModel(AtomicUsize);
impl ModelDriver for RealReadModel {
    async fn generate(
        &self,
        role: &str,
        input: Value,
        _: Vec<Value>,
        _: CancellationToken,
        _: u64,
    ) -> Result<provider::Generation, String> {
        if role == "implementer" && self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(provider::Generation {
                text: String::new(),
                usage: Some(json!({"total_tokens":1})),
                output: vec![
                    json!({"type":"function_call","call_id":"read-1","name":"read","arguments":"{\"path\":\"src/input.txt\"}"}),
                ],
                tool_calls: vec![provider::ToolCall {
                    call_id: "read-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"src/input.txt"}"#.into(),
                }],
            });
        }
        if role == "implementer" {
            assert!(input.to_string().contains("real fixture"));
        }
        Ok(provider::Generation {
            text: if role == "reviewer" {
                r#"{"approved":true}"#.into()
            } else {
                "done".into()
            },
            tool_calls: vec![],
            output: vec![],
            usage: Some(json!({"total_tokens":1})),
        })
    }
}
#[tokio::test]
async fn scripted_model_reads_real_fixture_and_preserves_tool_result() {
    let (root, runtime, _) = setup(&["a"], None);
    let mut cfg = config();
    cfg.implementer.max_rounds = 2;
    cfg.implementer.tools = vec![json!({"name":"read"})];
    run_loop(
        &RealReadModel(AtomicUsize::new(0)),
        &runtime,
        &fixture(vec![task("a", &[], "src/a")]),
        &cfg,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
