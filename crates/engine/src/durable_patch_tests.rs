use super::*;
use crate::Engine;
use domain::{Command, Request, ResponseData};
use std::{fs, path::PathBuf};

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    project_id: String,
    run_id: String,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-bound-patch-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(worktree.join("src")).unwrap();
        fs::write(worktree.join("src/file.txt"), "before").unwrap();
        let db = root.join("state.redb");
        let engine = Engine::open(&db).unwrap();
        let response = engine
            .handle(Request {
                id: "create".into(),
                command: Command::CreateProject {
                    name: "fixture".into(),
                    path: root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap();
        let project = match response {
            ResponseData::Project(p) => p,
            _ => panic!("project"),
        };
        drop(engine);
        let database = Database::open(&db).unwrap();
        let tx = database.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[{"id":"task","dependencies":[],"owned_paths":["src"],"validation":[]}],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx.open_table(RUNS).unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"context_version":project.context_version,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null,"agent_active":true,"agent_configs":[{"role":"implementer","model":"fixture","tools":["patch"],"limits":{}}],"used_calls":[]});
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(database);
        Self {
            root,
            db,
            project_id: project.id,
            run_id,
        }
    }
    fn file(&self) -> PathBuf {
        self.root.join("worktree/src/file.txt")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_file(self.file());
        let _ = fs::remove_dir(self.root.join("worktree/src"));
        let _ = fs::remove_dir(self.root.join("worktree"));
        let _ = fs::remove_dir(&self.root);
    }
}

#[test]
fn bound_patch_claim_and_pending_are_atomic_and_completed_result_is_reused() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    let request = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    let approval = request["approval_id"].as_str().unwrap();
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
    let run = get(&db, &f.run_id).unwrap();
    assert_eq!(run["pending"]["call_id"], "call-1");
    let tx = db.begin_read().unwrap();
    let calls = tx.open_table(CALLS).unwrap();
    let key = format!("{}:call-1", f.run_id);
    let record: serde_json::Value =
        serde_json::from_str(calls.get(key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(record["status"], "waiting");
    assert_eq!(record["approval_id"], approval);
    drop(calls);
    drop(tx);
    approve_patch(&db, &f.run_id, approval, true).unwrap();
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "after");
    fs::write(f.file(), "later").unwrap();
    let reused = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    assert_eq!(reused["status"], "completed");
    assert_eq!(reused["result"]["status"], "applied");
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "later");
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    drop(engine);
    let db = Database::open(&f.db).unwrap();
    use sha2::{Digest, Sha256};
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&("implementer", "task", "patch", &args)).unwrap())
    );
    let stored = claim_call(&db, &f.run_id, "call-1", &hash).unwrap();
    assert_eq!(stored["status"], "completed");
    assert_eq!(stored["result"]["status"], "applied");
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "later");
}

#[test]
fn call_identity_rejects_different_arguments_role_or_task() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    let first = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    let waiting =
        submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).unwrap();
    assert_eq!(waiting["approval_id"], first["approval_id"]);
    assert_eq!(
        submit_bound_patch(
            &db,
            &f.run_id,
            "implementer",
            "task",
            "call-1",
            serde_json::json!({"path":"src/file.txt","content":"other"})
        )
        .unwrap_err(),
        "call identity conflict"
    );
    assert!(submit_bound_patch(
        &db,
        &f.run_id,
        "reviewer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"})
    )
    .is_err());
    assert!(submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "other",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"})
    )
    .is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
}

#[test]
fn denied_and_cancelled_durable_calls_cannot_be_resubmitted() {
    for cancelled in [false, true] {
        let f = Fixture::new();
        let db = Database::open(&f.db).unwrap();
        let args = serde_json::json!({"path":"src/file.txt","content":"after"});
        let proposal = submit_bound_patch(
            &db,
            &f.run_id,
            "implementer",
            "task",
            "call-1",
            args.clone(),
        )
        .unwrap();
        if cancelled {
            cancel(&db, &f.run_id).unwrap();
        } else {
            approve_patch(
                &db,
                &f.run_id,
                proposal["approval_id"].as_str().unwrap(),
                false,
            )
            .unwrap();
        }
        assert!(approve_patch(
            &db,
            &f.run_id,
            proposal["approval_id"].as_str().unwrap(),
            true
        )
        .is_err());
        assert!(submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).is_err());
        let run = get(&db, &f.run_id).unwrap();
        assert!(run["pending"].is_null());
        assert_eq!(
            run["status"],
            if cancelled {
                "cancelled"
            } else {
                "awaiting_implementation"
            }
        );
        assert!(run["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"]
                == if cancelled {
                    "cancelled"
                } else {
                    "tool_denied"
                }));
        let tx = db.begin_read().unwrap();
        let calls = tx.open_table(CALLS).unwrap();
        let key = format!("{}:call-1", f.run_id);
        let record: serde_json::Value =
            serde_json::from_str(calls.get(key.as_str()).unwrap().unwrap().value()).unwrap();
        assert_eq!(record["status"], "waiting");
        drop(calls);
        drop(tx);
        assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
    }
}

#[test]
fn stale_file_marks_call_uncertain_and_does_not_allow_reexecution() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    let proposal = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    fs::write(f.file(), "external").unwrap();
    assert_eq!(
        approve_patch(
            &db,
            &f.run_id,
            proposal["approval_id"].as_str().unwrap(),
            true
        )
        .unwrap_err(),
        "file changed since approval"
    );
    let tx = db.begin_read().unwrap();
    let calls = tx.open_table(CALLS).unwrap();
    let key = format!("{}:call-1", f.run_id);
    let record: serde_json::Value =
        serde_json::from_str(calls.get(key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(record["status"], "uncertain");
    drop(calls);
    drop(tx);
    assert!(submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "external");
}

#[test]
fn restart_after_interrupted_patch_never_reexecutes_ambiguous_call() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RUNS).unwrap();
        let mut run: serde_json::Value = {
            let value = table.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        run["status"] = "executing".into();
        table
            .insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    fs::write(f.file(), "unknown-effect").unwrap();
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    let run = engine
        .handle(Request {
            id: "run".into(),
            command: Command::GetRun {
                run_id: f.run_id.clone(),
            },
        })
        .result
        .unwrap();
    let run = match run {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "uncertain");
    drop(engine);
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_read().unwrap();
    let calls = tx.open_table(CALLS).unwrap();
    let key = format!("{}:call-1", f.run_id);
    let record: serde_json::Value =
        serde_json::from_str(calls.get(key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(record["status"], "uncertain");
    drop(calls);
    drop(tx);
    assert!(submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "unknown-effect");
}

#[test]
fn context_change_prevents_approval_of_pending_durable_call() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let proposal = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"}),
    )
    .unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(crate::PROJECTS).unwrap();
        let mut project: serde_json::Value = {
            let value = table.get(f.project_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        project["context_version"] = 1.into();
        table
            .insert(f.project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(
        approve_patch(
            &db,
            &f.run_id,
            proposal["approval_id"].as_str().unwrap(),
            true
        )
        .unwrap_err(),
        "approved specification changed"
    );
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
}

#[derive(Debug)]
struct FailingBackend {
    file: redb::backends::FileBackend,
    broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl redb::StorageBackend for FailingBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.file.len()
    }
    fn read(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.file.read(offset, length)
    }
    fn set_len(&self, length: u64) -> std::io::Result<()> {
        self.file.set_len(length)
    }
    fn sync_data(&self, eventual: bool) -> std::io::Result<()> {
        if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("fake failure"));
        }
        self.file.sync_data(eventual)
    }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("fake failure"));
        }
        self.file.write(offset, data)
    }
}

#[test]
fn failed_atomic_claim_leaves_no_pending_or_call_and_no_file_effect() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let f = Fixture::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&f.db)
        .unwrap();
    let broken = Arc::new(AtomicBool::new(false));
    let db = Database::builder()
        .set_cache_size(0)
        .create_with_backend(FailingBackend {
            file: redb::backends::FileBackend::new(file).unwrap(),
            broken: Arc::clone(&broken),
        })
        .unwrap();
    broken.store(true, Ordering::SeqCst);
    assert!(submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"})
    )
    .is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
    broken.store(false, Ordering::SeqCst);
    drop(db);
    let db = Database::open(&f.db).unwrap();
    let run = get(&db, &f.run_id).unwrap();
    assert_eq!(run["status"], "awaiting_implementation");
    assert!(run["pending"].is_null());
    assert!(run["used_calls"].as_array().unwrap().is_empty());
    let tx = db.begin_read().unwrap();
    match tx.open_table(CALLS) {
        Ok(table) => assert!(table
            .get(format!("{}:call-1", f.run_id).as_str())
            .unwrap()
            .is_none()),
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => panic!("table: {e}"),
    };
}

#[cfg(windows)]
#[test]
fn completed_bound_patch_is_reused_without_second_git_worktree_effect() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let repo = f.root.join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(repo.join("src/file.txt"), "before").unwrap();
    git(&["add", "src/file.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    let revision = git(&["rev-parse", "HEAD"]);
    git(&[
        "worktree",
        "add",
        "--detach",
        "../actual-worktree",
        &revision,
    ]);
    let actual = f.root.join("actual-worktree").canonicalize().unwrap();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RUNS).unwrap();
        let mut run: serde_json::Value = {
            let v = table.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(v.value()).unwrap()
        };
        run["worktree"] = actual.to_string_lossy().to_string().into();
        run["revision"] = revision.into();
        table
            .insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    let proposal = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "git-call",
        args.clone(),
    )
    .unwrap();
    approve_patch(
        &db,
        &f.run_id,
        proposal["approval_id"].as_str().unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(actual.join("src/file.txt")).unwrap(),
        "after"
    );
    assert_eq!(
        fs::read_to_string(repo.join("src/file.txt")).unwrap(),
        "before"
    );
    fs::write(actual.join("src/file.txt"), "external").unwrap();
    assert_eq!(
        submit_bound_patch(&db, &f.run_id, "implementer", "task", "git-call", args).unwrap()
            ["status"],
        "completed"
    );
    assert_eq!(
        fs::read_to_string(actual.join("src/file.txt")).unwrap(),
        "external"
    );
    drop(db);
    assert!(actual.starts_with(&f.root));
    git(&["worktree", "remove", "--force", "../actual-worktree"]);
    assert!(repo.starts_with(&f.root));
    fs::remove_dir_all(repo).unwrap();
}

#[test]
fn storage_failure_after_bound_patch_preserves_ambiguous_effect_and_never_replays() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let f = Fixture::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&f.db)
        .unwrap();
    let broken = Arc::new(AtomicBool::new(false));
    let db = Database::builder()
        .set_cache_size(0)
        .create_with_backend(FailingBackend {
            file: redb::backends::FileBackend::new(file).unwrap(),
            broken: Arc::clone(&broken),
        })
        .unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    let proposal = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        args.clone(),
    )
    .unwrap();
    let approval = proposal["approval_id"].as_str().unwrap().to_owned();
    *TEST_FAIL_AFTER_PATCH.lock().unwrap() = Some((f.run_id.clone(), Arc::clone(&broken)));
    assert!(approve_patch(&db, &f.run_id, &approval, true).is_err());
    *TEST_FAIL_AFTER_PATCH.lock().unwrap() = None;
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "after");
    broken.store(false, Ordering::SeqCst);
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    let run = engine
        .handle(Request {
            id: "run".into(),
            command: Command::GetRun {
                run_id: f.run_id.clone(),
            },
        })
        .result
        .unwrap();
    let run = match run {
        ResponseData::Data(v) => v,
        _ => panic!("run"),
    };
    assert_eq!(run["status"], "uncertain");
    drop(engine);
    fs::write(f.file(), "external").unwrap();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(CALLS).unwrap();
    let key = format!("{}:call-1", f.run_id);
    let record: serde_json::Value =
        serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(record["status"], "uncertain");
    drop(table);
    drop(tx);
    assert!(approve_patch(&db, &f.run_id, &approval, true).is_err());
    assert!(submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "external");
}

#[test]
fn concurrent_same_bound_call_creates_one_waiting_record_and_approval() {
    let f = Fixture::new();
    let db = std::sync::Arc::new(Database::open(&f.db).unwrap());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let barrier = barrier.clone();
            let id = f.run_id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                submit_bound_patch(
                    &db,
                    &id,
                    "implementer",
                    "task",
                    "call-1",
                    serde_json::json!({"path":"src/file.txt","content":"after"}),
                )
            })
        })
        .collect();
    barrier.wait();
    let values: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let accepted: Vec<_> = values.iter().filter_map(|v| v.as_ref().ok()).collect();
    assert!(!accepted.is_empty());
    if accepted.len() == 2 {
        assert_eq!(accepted[0]["approval_id"], accepted[1]["approval_id"]);
    }
    let replay = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"}),
    )
    .unwrap();
    assert_eq!(replay["approval_id"], accepted[0]["approval_id"]);
    let run = get(&db, &f.run_id).unwrap();
    assert_eq!(run["status"], "awaiting_approval");
    assert_eq!(run["used_calls"].as_array().unwrap().len(), 1);
    assert_eq!(
        run["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "approval_required")
            .count(),
        1
    );
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
}

#[test]
fn changed_pending_arguments_cannot_reuse_bound_approval() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let p = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"}),
    )
    .unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RUNS).unwrap();
        let mut run: serde_json::Value = {
            let value = table.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        run["pending"]["arguments"]["content"] = "tampered".into();
        table
            .insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(
        approve_patch(&db, &f.run_id, p["approval_id"].as_str().unwrap(), true).unwrap_err(),
        "call identity conflict"
    );
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
}

#[test]
fn task_ownership_and_role_tools_are_enforced_before_waiting() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut projects = tx.open_table(crate::PROJECTS).unwrap();
        let mut project: serde_json::Value = {
            let value = projects.get(f.project_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        project["approved"]["tasks"][0]["owned_paths"] = serde_json::json!(["src/nested"]);
        projects
            .insert(f.project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    let args = serde_json::json!({"path":"src/file.txt","content":"after"});
    assert_eq!(
        submit_bound_patch(&db, &f.run_id, "implementer", "task", "call-1", args).unwrap_err(),
        "path outside task ownership"
    );
    let run = get(&db, &f.run_id).unwrap();
    assert!(run["pending"].is_null());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
}

#[test]
fn failed_execution_commit_keeps_bound_approval_and_file_unchanged() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let f = Fixture::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&f.db)
        .unwrap();
    let broken = Arc::new(AtomicBool::new(false));
    let db = Database::builder()
        .set_cache_size(0)
        .create_with_backend(FailingBackend {
            file: redb::backends::FileBackend::new(file).unwrap(),
            broken: Arc::clone(&broken),
        })
        .unwrap();
    let p = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"}),
    )
    .unwrap();
    let approval = p["approval_id"].as_str().unwrap().to_owned();
    broken.store(true, Ordering::SeqCst);
    assert!(approve_patch(&db, &f.run_id, &approval, true).is_err());
    assert_eq!(fs::read_to_string(f.file()).unwrap(), "before");
    broken.store(false, Ordering::SeqCst);
    drop(db);
    let db = Database::open(&f.db).unwrap();
    let run = get(&db, &f.run_id).unwrap();
    assert_eq!(run["status"], "awaiting_approval");
    assert_eq!(run["pending"]["id"], approval);
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(CALLS).unwrap();
    let key = format!("{}:call-1", f.run_id);
    let record: serde_json::Value =
        serde_json::from_str(table.get(key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(record["status"], "waiting");
}

#[test]
fn legacy_run_without_optional_used_calls_can_start_bound_patch() {
    let f = Fixture::new();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RUNS).unwrap();
        let mut run: serde_json::Value = {
            let value = table.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        run.as_object_mut().unwrap().remove("used_calls");
        table
            .insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    let request = submit_bound_patch(
        &db,
        &f.run_id,
        "implementer",
        "task",
        "call-1",
        serde_json::json!({"path":"src/file.txt","content":"after"}),
    )
    .unwrap();
    assert_eq!(request["status"], "waiting");
    let run = get(&db, &f.run_id).unwrap();
    assert_eq!(run["used_calls"].as_array().unwrap().len(), 1);
}
