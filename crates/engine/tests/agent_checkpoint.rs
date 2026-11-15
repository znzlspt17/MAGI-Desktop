use domain::{ApprovedSpec, TaskBinding, TaskSpec, VerificationEvidence};
use engine::agent_loop::{
    self, CheckpointStore, DurableToolDriver, LoopCheckpoint, LoopConfig, ModelDriver, RoleConfig,
    ToolDriver,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};
use tokio_util::sync::CancellationToken;

fn fixture() -> (ApprovedSpec, LoopConfig) {
    let task = TaskSpec {
        id: "a".into(),
        dependencies: vec![],
        owned_paths: vec!["src/a".into()],
        validation: vec!["check".into()],
    };
    let spec = ApprovedSpec {
        version: 1,
        hash: "approved".into(),
        base_revision: "revision".into(),
        scope: vec!["src".into()],
        requirements: vec!["r".into()],
        acceptance: vec!["a".into()],
        accepted_optional: vec![],
        markdown: String::new(),
        source_links: vec![],
        constraints: vec![],
        tasks: vec![task],
        reused_decisions: vec![],
        task_bindings: vec![TaskBinding {
            task_id: "a".into(),
            goal: "g".into(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        }],
    };
    let role = |name: &str, tools: Vec<Value>| RoleConfig {
        name: name.into(),
        tools,
        max_rounds: 4,
    };
    let cfg = LoopConfig {
        coordinator: role("c", vec![]),
        implementer: role("i", vec![json!({"name":"patch"})]),
        reviewer: role("r", vec![]),
        max_tokens: 100000,
        max_parallel_tasks: 1,
        timeout: std::time::Duration::from_secs(5),
    };
    (spec, cfg)
}
#[derive(Default)]
struct Store {
    state: Mutex<Option<LoopCheckpoint>>,
    fail_after_effect: AtomicBool,
    force_conflict: AtomicBool,
}
impl CheckpointStore for Store {
    async fn load(&self) -> Result<Option<LoopCheckpoint>, String> {
        Ok(self.state.lock().unwrap().clone())
    }
    async fn save(&self, expected: u64, mut state: LoopCheckpoint) -> Result<u64, String> {
        if !state.used_calls.is_empty() && self.fail_after_effect.swap(false, Ordering::SeqCst) {
            return Err("simulated crash after effect".into());
        }
        let mut slot = self.state.lock().unwrap();
        if state.schema_version == 2
            && !state.task_states.is_empty()
            && self.force_conflict.swap(false, Ordering::SeqCst)
        {
            slot.as_mut().unwrap().revision += 1;
            return Err("simulated CAS conflict".into());
        }
        if slot.as_ref().map_or(0, |s| s.revision) != expected {
            return Err("stale checkpoint".into());
        }
        state.revision = expected + 1;
        *slot = Some(state);
        Ok(expected + 1)
    }
}
struct Model(AtomicUsize);
impl ModelDriver for Model {
    async fn generate(
        &self,
        role: &str,
        input: Value,
        _: Vec<Value>,
        _: CancellationToken,
        _: u64,
    ) -> Result<provider::Generation, String> {
        if role == "i" && self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(provider::Generation {
                text: String::new(),
                tool_calls: vec![provider::ToolCall {
                    call_id: "id-1".into(),
                    name: "patch".into(),
                    arguments: r#"{"path":"src/a","content":"ok"}"#.into(),
                }],
                output: vec![
                    json!({"type":"reasoning","id":"reason"}),
                    json!({"type":"function_call","call_id":"id-1","name":"patch","arguments":r#"{"path":"src/a","content":"ok"}"#}),
                ],
                usage: Some(json!({"total_tokens":1})),
            });
        }
        if role == "i" {
            let text = input.to_string();
            assert!(text.contains("reason"));
            assert!(text.contains("function_call_output"));
            assert!(text.contains("id-1"));
        }
        Ok(provider::Generation {
            text: if role == "r" {
                r#"{"approved":true}"#
            } else {
                "done"
            }
            .into(),
            tool_calls: vec![],
            output: vec![],
            usage: Some(json!({"total_tokens":1})),
        })
    }
}
struct Runtime {
    effect_dir: std::path::PathBuf,
    approved: AtomicBool,
    waiting: tokio::sync::Notify,
    entries: AtomicUsize,
    effects: AtomicUsize,
    uncertain: AtomicBool,
    completed: Mutex<Option<Value>>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self.effect_dir.exists() {
            std::fs::remove_dir_all(&self.effect_dir).unwrap();
        }
    }
}
impl ToolDriver for Runtime {
    async fn fresh(&self, _: &ApprovedSpec) -> Result<(), String> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &str,
        _: &TaskSpec,
        _: &str,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, String> {
        panic!("non-durable execution forbidden")
    }
    async fn verify(
        &self,
        _: &TaskSpec,
        command: &str,
        _: CancellationToken,
    ) -> Result<VerificationEvidence, String> {
        Ok(VerificationEvidence {
            revision: "revision".into(),
            spec_hash: "approved".into(),
            verification_id: String::new(),
            command: command.into(),
            passed: true,
            output: "ok".into(),
            worktree_hash: "tree".into(),
            requirement_ids: vec!["requirement-0".into()],
            acceptance_ids: vec!["acceptance-0".into()],
        })
    }
}
impl DurableToolDriver for Runtime {
    async fn verify_durable(
        &self,
        task: &TaskSpec,
        command: &str,
        _: &str,
        cancel: CancellationToken,
    ) -> Result<VerificationEvidence, String> {
        self.verify(task, command, cancel).await
    }
    async fn execute_durable(
        &self,
        _: &str,
        _: &TaskSpec,
        _: &str,
        id: &str,
        _: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.entries.fetch_add(1, Ordering::SeqCst);
        assert!(id.contains("id-1"));
        if self.uncertain.load(Ordering::SeqCst) {
            return Err("uncertain".into());
        }
        if !self.approved.load(Ordering::SeqCst) {
            self.waiting.notify_one();
            cancel.cancelled().await;
            return Err("cancelled".into());
        }
        let mut saved = self.completed.lock().unwrap();
        if let Some(result) = saved.as_ref() {
            return Ok(result.clone());
        }
        std::fs::create_dir_all(&self.effect_dir).map_err(|_| "fixture directory")?;
        std::fs::write(self.effect_dir.join("effect"), "applied").map_err(|_| "fixture effect")?;
        self.effects.fetch_add(1, Ordering::SeqCst);
        let result = json!({"status":"applied"});
        *saved = Some(result.clone());
        Ok(result)
    }
}
#[tokio::test]
async fn resumes_same_approved_call_with_preserved_provider_output() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(false),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    let cancel = CancellationToken::new();
    let waiter = async {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            runtime.waiting.notified(),
        )
        .await;
        cancel.cancel();
    };
    let (first, _) = tokio::join!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &cfg,
            "model-v1",
            cancel.clone()
        ),
        waiter
    );
    assert!(first.is_err());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    runtime.approved.store(true, Ordering::SeqCst);
    agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completed_call_is_reused_after_checkpoint_write_failure() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    store.fail_after_effect.store(true, Ordering::SeqCst);
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(true),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    assert!(agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 1);
    agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(runtime.entries.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(runtime.effect_dir.join("effect")).unwrap(),
        "applied"
    );
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn uncertain_call_never_replays() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(true),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(true),
        completed: Mutex::new(None),
    };
    assert!(agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new()
    )
    .await
    .is_err());
    assert!(agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(runtime.entries.load(Ordering::SeqCst), 2);
    assert!(!runtime.effect_dir.exists());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changed_model_identity_cannot_resume_pending_call() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(false),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    let cancel = CancellationToken::new();
    let waiter = async {
        runtime.waiting.notified().await;
        cancel.cancel();
    };
    let (first, _) = tokio::join!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &cfg,
            "model-v1",
            cancel.clone()
        ),
        waiter
    );
    assert!(first.is_err());
    let saved = store.load().await.unwrap().unwrap();
    assert_eq!(saved.pending_calls[0].call_id, "id-1");
    assert!(serde_json::to_string(&saved.input)
        .unwrap()
        .contains("reason"));
    runtime.approved.store(true, Ordering::SeqCst);
    assert!(agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v2",
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn parallel_checkpoint_merges_task_progress_and_resumes_without_model_replay() {
    use std::{collections::HashMap, sync::Arc, time::Duration};
    struct ParallelModel {
        barrier: tokio::sync::Barrier,
        requests: AtomicUsize,
    }
    impl ModelDriver for ParallelModel {
        async fn generate(
            &self,
            role: &str,
            input: Value,
            _: Vec<Value>,
            _: CancellationToken,
            _: u64,
        ) -> Result<provider::Generation, String> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if role == "i" && !input.to_string().contains("function_call_output") {
                self.barrier.wait().await;
                return Ok(provider::Generation {
                    text: String::new(),
                    tool_calls: vec![provider::ToolCall {
                        call_id: "same".into(),
                        name: "patch".into(),
                        arguments: r#"{"path":"src/a","content":"x"}"#.into(),
                    }],
                    output: vec![
                        json!({"type":"reasoning","id":"reason"}),
                        json!({"type":"function_call","call_id":"same","name":"patch","arguments":r#"{"path":"src/a","content":"x"}"#}),
                    ],
                    usage: Some(json!({"total_tokens":1})),
                });
            }
            Ok(provider::Generation {
                text: if role == "r" {
                    r#"{"approved":true}"#
                } else {
                    "done"
                }
                .into(),
                tool_calls: vec![],
                output: vec![],
                usage: Some(json!({"total_tokens":1})),
            })
        }
    }
    struct ParallelRuntime {
        approved: AtomicBool,
        waiting: AtomicUsize,
        effects: Mutex<HashMap<String, Value>>,
    }
    impl ToolDriver for ParallelRuntime {
        async fn fresh(&self, _: &ApprovedSpec) -> Result<(), String> {
            Ok(())
        }
        async fn execute(
            &self,
            _: &str,
            _: &TaskSpec,
            _: &str,
            _: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, String> {
            panic!()
        }
        async fn verify(
            &self,
            _: &TaskSpec,
            c: &str,
            _: CancellationToken,
        ) -> Result<VerificationEvidence, String> {
            Ok(VerificationEvidence {
                revision: "r".into(),
                spec_hash: "approved".into(),
                verification_id: String::new(),
                command: c.into(),
                passed: true,
                output: "ok".into(),
                worktree_hash: "tree".into(),
                requirement_ids: vec!["requirement-0".into()],
                acceptance_ids: vec!["acceptance-0".into()],
            })
        }
    }
    impl DurableToolDriver for ParallelRuntime {
        async fn verify_durable(
            &self,
            t: &TaskSpec,
            c: &str,
            _: &str,
            x: CancellationToken,
        ) -> Result<VerificationEvidence, String> {
            self.verify(t, c, x).await
        }
        async fn execute_durable(
            &self,
            _: &str,
            _: &TaskSpec,
            _: &str,
            id: &str,
            _: Value,
            c: CancellationToken,
        ) -> Result<Value, String> {
            if !self.approved.load(Ordering::SeqCst) {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                c.cancelled().await;
                return Err("cancelled".into());
            }
            let mut effects = self.effects.lock().unwrap();
            Ok(effects
                .entry(id.into())
                .or_insert_with(|| json!({"ok":true}))
                .clone())
        }
    }
    let (mut spec, mut cfg) = fixture();
    let mut second = spec.tasks[0].clone();
    second.id = "b".into();
    second.owned_paths = vec!["src/b".into()];
    spec.tasks.push(second);
    spec.task_bindings.push(TaskBinding {
        task_id: "b".into(),
        goal: "g".into(),
        requirement_ids: vec!["requirement-0".into()],
        acceptance_ids: vec!["acceptance-0".into()],
    });
    cfg.max_parallel_tasks = 2;
    let store = Store::default();
    store.force_conflict.store(true, Ordering::SeqCst);
    let model = ParallelModel {
        barrier: tokio::sync::Barrier::new(2),
        requests: AtomicUsize::new(0),
    };
    let runtime = Arc::new(ParallelRuntime {
        approved: AtomicBool::new(false),
        waiting: AtomicUsize::new(0),
        effects: Mutex::new(HashMap::new()),
    });
    let cancel = CancellationToken::new();
    let wait = async {
        for _ in 0..200 {
            if runtime.waiting.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (first, _) = tokio::join!(
        agent_loop::run_checkpointed_loop(
            &model,
            runtime.as_ref(),
            &store,
            &spec,
            &cfg,
            "model-v1",
            cancel.clone()
        ),
        wait
    );
    assert!(first.is_err());
    assert_eq!(runtime.waiting.load(Ordering::SeqCst), 2);
    let saved = store.load().await.unwrap().unwrap();
    assert_eq!(saved.task_states.len(), 2);
    assert_eq!(
        saved.tokens_used,
        saved
            .task_states
            .values()
            .map(|s| s.tokens_used)
            .sum::<u64>()
    );
    assert!(saved.tokens_used <= cfg.max_tokens);
    let before = model.requests.load(Ordering::SeqCst);
    assert_eq!(before, 4);
    runtime.approved.store(true, Ordering::SeqCst);
    agent_loop::run_checkpointed_loop(
        &model,
        runtime.as_ref(),
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(runtime.effects.lock().unwrap().len(), 2);
    assert_eq!(model.requests.load(Ordering::SeqCst) - before, 4);
}

#[tokio::test]
async fn resumed_checkpoint_cannot_execute_unlisted_tool() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(false),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    let cancel = CancellationToken::new();
    let wait = async {
        runtime.waiting.notified().await;
        cancel.cancel();
    };
    let (first, _) = tokio::join!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &cfg,
            "model-v1",
            cancel.clone()
        ),
        wait
    );
    assert!(first.is_err());
    store.state.lock().unwrap().as_mut().unwrap().pending_calls[0].name = "unknown".into();
    runtime.approved.store(true, Ordering::SeqCst);
    assert!(agent_loop::run_checkpointed_loop(
        &model,
        &runtime,
        &store,
        &spec,
        &cfg,
        "model-v1",
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pending_checkpoint_rejects_changed_spec_version_and_configuration() {
    let (spec, cfg) = fixture();
    let store = Store::default();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(false),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    let cancel = CancellationToken::new();
    let wait = async {
        runtime.waiting.notified().await;
        cancel.cancel();
    };
    let (first, _) = tokio::join!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &cfg,
            "model-v1",
            cancel.clone()
        ),
        wait
    );
    assert!(first.is_err());
    let entries = runtime.entries.load(Ordering::SeqCst);
    let mut changed = spec.clone();
    changed.hash = "other-spec".into();
    assert_eq!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &changed,
            &cfg,
            "model-v1",
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "checkpoint does not match run configuration"
    );
    changed = spec.clone();
    changed.version += 1;
    assert_eq!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &changed,
            &cfg,
            "model-v1",
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "checkpoint does not match run configuration"
    );
    let mut other_cfg = cfg.clone();
    other_cfg.implementer.max_rounds += 1;
    assert_eq!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &store,
            &spec,
            &other_cfg,
            "model-v1",
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "checkpoint does not match run configuration"
    );
    assert_eq!(runtime.entries.load(Ordering::SeqCst), entries);
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}

struct BrokenStore;
impl CheckpointStore for BrokenStore {
    async fn load(&self) -> Result<Option<LoopCheckpoint>, String> {
        Ok(None)
    }
    async fn save(&self, _: u64, _: LoopCheckpoint) -> Result<u64, String> {
        Err("fixture save failed".into())
    }
}
#[tokio::test]
async fn storage_failure_and_budget_exhaustion_stop_before_model_and_effects() {
    let (spec, mut cfg) = fixture();
    let model = Model(AtomicUsize::new(0));
    let runtime = Runtime {
        effect_dir: std::env::temp_dir()
            .join(format!("magi-checkpoint-effect-{}", uuid::Uuid::new_v4())),
        approved: AtomicBool::new(true),
        waiting: tokio::sync::Notify::new(),
        entries: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
        uncertain: AtomicBool::new(false),
        completed: Mutex::new(None),
    };
    assert_eq!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &BrokenStore,
            &spec,
            &cfg,
            "model-v1",
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "fixture save failed"
    );
    cfg.max_tokens = 1;
    assert_eq!(
        agent_loop::run_checkpointed_loop(
            &model,
            &runtime,
            &Store::default(),
            &spec,
            &cfg,
            "model-v1",
            CancellationToken::new()
        )
        .await
        .unwrap_err(),
        "token budget insufficient for input"
    );
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.entries.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}
