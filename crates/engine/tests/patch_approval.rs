#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use redb::{Database, ReadableTable, TableDefinition};
use std::{fs, path::PathBuf};

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    project_id: String,
    run_id: String,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-patch-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(worktree.join("src")).unwrap();
        fs::write(worktree.join("src/file.txt"), "before").unwrap();
        let db = root.join("state.redb");
        let engine = Engine::open(&db).unwrap();
        let project = match call(
            &engine,
            Command::CreateProject {
                name: "fixture".into(),
                path: root.to_string_lossy().into_owned(),
            },
        )
        .unwrap()
        {
            ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        drop(engine);
        let database = Database::open(&db).unwrap();
        let tx = database.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut projects = tx
                .open_table(TableDefinition::<&str, &str>::new("projects"))
                .unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
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
    fn path(&self) -> PathBuf {
        self.root.join("worktree/src/file.txt")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(self.root.join("worktree/src"));
        let _ = fs::remove_dir(self.root.join("worktree"));
        let _ = fs::remove_dir(&self.root);
    }
}
fn call(engine: &Engine, command: Command) -> Result<ResponseData, String> {
    engine
        .handle(Request {
            id: "test".into(),
            command,
        })
        .result
}
fn submit(engine: &Engine, id: &str, path: &str, content: &str) -> Result<ResponseData, String> {
    call(
        engine,
        Command::SubmitTool {
            run_id: id.into(),
            tool: "patch".into(),
            arguments: serde_json::json!({"path":path,"content":content}),
        },
    )
}
fn data(response: ResponseData) -> serde_json::Value {
    match response {
        ResponseData::Data(value) => value,
        _ => panic!("data"),
    }
}

#[test]
fn approved_patch_is_proposed_before_writing_and_applied_once() {
    let fixture = Fixture::new();
    let engine = Engine::open(&fixture.db).unwrap();
    let proposal = data(submit(&engine, &fixture.run_id, "src/file.txt", "after").unwrap());
    let id = proposal["approval_id"].as_str().unwrap();
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), "before");
    let run = data(
        call(
            &engine,
            Command::GetRun {
                run_id: fixture.run_id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(run["pending"]["id"], id);
    assert_eq!(run["pending"]["arguments"]["path"], "src/file.txt");
    assert_eq!(run["pending"]["arguments"]["content"], "after");
    assert!(run["pending"]["before_hash"]
        .as_str()
        .is_some_and(|v| !v.is_empty()));
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: "wrong".into(),
            approved: true
        }
    )
    .is_err());
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), "before");
    assert_eq!(
        data(
            call(
                &engine,
                Command::ApproveTool {
                    run_id: fixture.run_id.clone(),
                    approval_id: id.into(),
                    approved: true
                }
            )
            .unwrap()
        )["status"],
        "applied"
    );
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), "after");
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: fixture.run_id.clone(),
            approval_id: id.into(),
            approved: true
        }
    )
    .is_err());
}

#[test]
fn new_file_proposal_does_not_create_parent_until_approval() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let parent = f.root.join("worktree/src/new");
    let p = data(submit(&engine, &f.run_id, "src/new/file.txt", "new").unwrap());
    assert!(!parent.exists());
    let run = data(
        call(
            &engine,
            Command::GetRun {
                run_id: f.run_id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(run["pending"]["before_hash"], "missing");
    call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: p["approval_id"].as_str().unwrap().into(),
            approved: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(parent.join("file.txt")).unwrap(), "new");
    fs::remove_file(parent.join("file.txt")).unwrap();
    fs::remove_dir(parent).unwrap();
}

#[test]
fn changed_file_after_proposal_is_not_overwritten() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    fs::write(f.path(), "external").unwrap();
    let error = call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: p["approval_id"].as_str().unwrap().into(),
            approved: true,
        },
    )
    .unwrap_err();
    assert_eq!(error, "file changed since approval");
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "external");
    let run = data(
        call(
            &engine,
            Command::GetRun {
                run_id: f.run_id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(run["status"], "awaiting_implementation");
    assert!(run["pending"].is_null());
}

#[test]
fn cancelled_pending_cannot_be_approved_or_revived() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    call(
        &engine,
        Command::CancelRun {
            run_id: f.run_id.clone(),
        },
    )
    .unwrap();
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: p["approval_id"].as_str().unwrap().into(),
            approved: true
        }
    )
    .is_err());
    let run = data(
        call(
            &engine,
            Command::GetRun {
                run_id: f.run_id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(run["status"], "cancelled");
    assert!(run["pending"].is_null());
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
}

#[test]
fn changed_draft_invalidates_previously_requested_patch() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    call(
        &engine,
        Command::SaveDraft {
            project_id: f.project_id.clone(),
            draft: domain::SpecDraft {
                title: "changed".into(),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        call(
            &engine,
            Command::ApproveTool {
                run_id: f.run_id.clone(),
                approval_id: p["approval_id"].as_str().unwrap().into(),
                approved: true
            }
        )
        .unwrap_err(),
        "approved specification changed"
    );
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
}

#[test]
fn protected_names_and_hardlinks_are_rejected() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    for path in [
        "../outside",
        "src/CON.txt",
        "src/file.",
        "src/.git/config",
        "other/file.txt",
    ] {
        assert!(submit(&engine, &f.run_id, path, "after").is_err(), "{path}");
    }
    let link = f.root.join("worktree/src/link.txt");
    fs::hard_link(f.path(), &link).unwrap();
    assert!(submit(&engine, &f.run_id, "src/link.txt", "after").is_err());
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
    fs::remove_file(link).unwrap();
}

#[test]
fn denied_or_old_approval_cannot_consume_new_pending() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let first = data(submit(&engine, &f.run_id, "src/file.txt", "first").unwrap());
    let first_id = first["approval_id"].as_str().unwrap();
    assert_eq!(
        data(
            call(
                &engine,
                Command::ApproveTool {
                    run_id: f.run_id.clone(),
                    approval_id: first_id.into(),
                    approved: false
                }
            )
            .unwrap()
        )["status"],
        "denied"
    );
    let second = data(submit(&engine, &f.run_id, "src/file.txt", "second").unwrap());
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: first_id.into(),
            approved: false
        }
    )
    .is_err());
    let run = data(
        call(
            &engine,
            Command::GetRun {
                run_id: f.run_id.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(run["pending"]["id"], second["approval_id"]);
    assert_eq!(run["status"], "awaiting_approval");
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
}

#[test]
fn file_created_after_new_file_proposal_is_not_overwritten() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let file = f.root.join("worktree/src/new.txt");
    let p = data(submit(&engine, &f.run_id, "src/new.txt", "proposed").unwrap());
    fs::write(&file, "external").unwrap();
    assert_eq!(
        call(
            &engine,
            Command::ApproveTool {
                run_id: f.run_id.clone(),
                approval_id: p["approval_id"].as_str().unwrap().into(),
                approved: true
            }
        )
        .unwrap_err(),
        "file changed since approval"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "external");
    fs::remove_file(file).unwrap();
}

#[test]
fn approved_patch_changes_real_git_worktree_not_original_checkout() {
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
            "git failed {:?}: {}",
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
    let actual = f.root.join("actual-worktree");
    git(&[
        "worktree",
        "add",
        "--detach",
        "../actual-worktree",
        &revision,
    ]);
    let actual = actual.canonicalize().unwrap();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut runs = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let value = runs.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        run["worktree"] = actual.to_string_lossy().to_string().into();
        run["revision"] = revision.clone().into();
        runs.insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
        let mut projects = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project: serde_json::Value = {
            let value = projects.get(f.project_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        project["approved"]["base_revision"] = revision.into();
        projects
            .insert(f.project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: p["approval_id"].as_str().unwrap().into(),
            approved: true,
        },
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
    drop(engine);
    assert!(actual.starts_with(&f.root));
    git(&["worktree", "remove", "--force", "../actual-worktree"]);
    assert!(repo.starts_with(&f.root));
    fs::remove_dir_all(repo).unwrap();
}

#[test]
fn cancel_racing_with_submit_or_denial_never_revives_run() {
    for deny in [false, true] {
        for _ in 0..12 {
            let f = Fixture::new();
            let engine = std::sync::Arc::new(Engine::open(&f.db).unwrap());
            let approval = if deny {
                Some(
                    data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap())
                        ["approval_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            } else {
                None
            };
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let a = std::sync::Arc::clone(&engine);
            let gate = std::sync::Arc::clone(&barrier);
            let id = f.run_id.clone();
            let worker = std::thread::spawn(move || {
                gate.wait();
                if let Some(approval_id) = approval {
                    call(
                        &a,
                        Command::ApproveTool {
                            run_id: id,
                            approval_id,
                            approved: false,
                        },
                    )
                } else {
                    submit(&a, &id, "src/file.txt", "after")
                }
            });
            let a = std::sync::Arc::clone(&engine);
            let gate = std::sync::Arc::clone(&barrier);
            let id = f.run_id.clone();
            let cancel = std::thread::spawn(move || {
                gate.wait();
                call(&a, Command::CancelRun { run_id: id })
            });
            barrier.wait();
            let _ = worker.join().unwrap();
            cancel.join().unwrap().unwrap();
            let run = data(
                call(
                    &engine,
                    Command::GetRun {
                        run_id: f.run_id.clone(),
                    },
                )
                .unwrap(),
            );
            assert_eq!(run["status"], "cancelled");
            assert!(run["pending"].is_null());
            assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
        }
    }
}

#[test]
fn junction_inside_scope_cannot_redirect_patch_outside_worktree() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let outside = f.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "outside").unwrap();
    let junction = f.root.join("worktree/src/link");
    let winpath = |p: &std::path::Path| {
        p.to_string_lossy()
            .strip_prefix(r"\\?\")
            .unwrap_or(&p.to_string_lossy())
            .to_owned()
    };
    let result = std::process::Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            &winpath(&junction),
            &winpath(&outside),
        ])
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(result.status.success(), "junction fixture failed");
    let engine = Engine::open(&f.db).unwrap();
    assert!(submit(&engine, &f.run_id, "src/link/file.txt", "changed").is_err());
    assert_eq!(
        fs::read_to_string(outside.join("file.txt")).unwrap(),
        "outside"
    );
    drop(engine);
    fs::remove_dir(junction).unwrap();
    fs::remove_file(outside.join("file.txt")).unwrap();
    fs::remove_dir(outside).unwrap();
}

#[test]
fn run_scope_cannot_expand_current_approved_scope() {
    let f = Fixture::new();
    let other = f.root.join("worktree/other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("file.txt"), "before").unwrap();
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let mut run: serde_json::Value = {
            let value = table.get(f.run_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        run["scope"] = serde_json::json!(["other"]);
        table
            .insert(f.run_id.as_str(), run.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "other/file.txt", "after").unwrap());
    assert_eq!(
        call(
            &engine,
            Command::ApproveTool {
                run_id: f.run_id.clone(),
                approval_id: p["approval_id"].as_str().unwrap().into(),
                approved: true
            }
        )
        .unwrap_err(),
        "approved specification changed"
    );
    assert_eq!(
        fs::read_to_string(other.join("file.txt")).unwrap(),
        "before"
    );
    drop(engine);
    fs::remove_file(other.join("file.txt")).unwrap();
    fs::remove_dir(other).unwrap();
}

#[test]
fn changed_approval_version_cannot_authorize_old_patch() {
    let f = Fixture::new();
    let engine = Engine::open(&f.db).unwrap();
    let proposal = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    let approval = proposal["approval_id"].as_str().unwrap().to_owned();
    drop(engine);
    let db = Database::open(&f.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut projects = tx
            .open_table(TableDefinition::<&str, &str>::new("projects"))
            .unwrap();
        let mut project: serde_json::Value = {
            let value = projects.get(f.project_id.as_str()).unwrap().unwrap();
            serde_json::from_str(value.value()).unwrap()
        };
        project["approved"]["version"] = 2.into();
        projects
            .insert(f.project_id.as_str(), project.to_string().as_str())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = Engine::open(&f.db).unwrap();
    assert_eq!(
        call(
            &engine,
            Command::ApproveTool {
                run_id: f.run_id.clone(),
                approval_id: approval,
                approved: true
            }
        )
        .unwrap_err(),
        "approved specification changed"
    );
    assert_eq!(fs::read_to_string(f.path()).unwrap(), "before");
}

#[test]
fn replacing_proposed_file_with_hardlink_cannot_modify_outside() {
    let f = Fixture::new();
    let outside = f.root.join("outside.txt");
    fs::write(&outside, "outside").unwrap();
    let engine = Engine::open(&f.db).unwrap();
    let p = data(submit(&engine, &f.run_id, "src/file.txt", "after").unwrap());
    fs::remove_file(f.path()).unwrap();
    fs::hard_link(&outside, f.path()).unwrap();
    assert!(call(
        &engine,
        Command::ApproveTool {
            run_id: f.run_id.clone(),
            approval_id: p["approval_id"].as_str().unwrap().into(),
            approved: true
        }
    )
    .is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    drop(engine);
    fs::remove_file(f.path()).unwrap();
    fs::remove_file(outside).unwrap();
}
