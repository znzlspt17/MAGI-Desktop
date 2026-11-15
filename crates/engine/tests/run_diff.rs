#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use redb::{Database, ReadableTable, TableDefinition};
use std::{
    fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
};
const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");
fn git(dir: &Path, args: &[&str]) -> String {
    let result = Process::new("git")
        .creation_flags(0x08000000)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "NUL")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(result.status.success(), "fixture git failed");
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}
fn call(engine: &Engine, command: Command) -> Result<ResponseData, String> {
    engine
        .handle(Request {
            id: "diff-test".into(),
            command,
        })
        .result
}
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    work: PathBuf,
    db: PathBuf,
    run: String,
    rev: String,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-diff-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        let work = root.join("work");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::create_dir(repo.join("src")).unwrap();
        fs::write(repo.join("src/a.txt"), "before\n").unwrap();
        fs::write(repo.join("src/delete.txt"), "old\n").unwrap();
        fs::write(repo.join("outside.txt"), "outside\n").unwrap();
        git(
            &repo,
            &["add", "src/a.txt", "src/delete.txt", "outside.txt"],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let rev = git(&repo, &["rev-parse", "HEAD"]);
        let target = work
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(work.to_str().unwrap());
        git(&repo, &["worktree", "add", "--detach", "--", target, &rev]);
        let db = root.join("state.redb");
        drop(Engine::open(&db).unwrap());
        let run = uuid::Uuid::new_v4().to_string();
        let database = Database::open(&db).unwrap();
        let tx = database.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            let data=serde_json::json!({"id":run,"project_id":uuid::Uuid::new_v4().to_string(),"spec_hash":"approved-fixture","spec_version":3,"revision":rev,"scope":["src"],"worktree":work.to_string_lossy(),"status":"completed","events":[],"agent_active":false,"resources_unknown":false}).to_string();
            table.insert(run.as_str(), data.as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(database);
        Self {
            root,
            repo,
            work,
            db,
            run,
            rev,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let root = self.root.canonicalize().unwrap();
        let repo = self.repo.canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-diff-"));
        assert_eq!(repo, root.join("repo"));
        assert_eq!(self.work.parent(), Some(root.as_path()));
        let _ = git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                self.work
                    .to_str()
                    .unwrap()
                    .strip_prefix(r"\\?\")
                    .unwrap_or(self.work.to_str().unwrap()),
            ],
        );
        let _ = fs::remove_file(&self.db);
        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir(&self.root);
    }
}

#[test]
fn diff_returns_scoped_before_after_without_mutating_run_or_files() {
    let f = Fixture::new();
    fs::write(f.work.join("src/a.txt"), "after\n").unwrap();
    fs::remove_file(f.work.join("src/delete.txt")).unwrap();
    fs::write(f.work.join("src/new.txt"), "new\n").unwrap();
    fs::write(f.work.join("outside.txt"), "outside changed\n").unwrap();
    let engine = Engine::open(&f.db).unwrap();
    let before = call(
        &engine,
        Command::GetRun {
            run_id: f.run.clone(),
        },
    )
    .unwrap();
    let diff = match call(
        &engine,
        Command::GetDiff {
            run_id: f.run.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Data(v) => v,
        _ => panic!("expected diff"),
    };
    assert_eq!(diff["revision"], f.rev);
    assert_eq!(diff["current_revision"], f.rev);
    assert_eq!(diff["spec_hash"], "approved-fixture");
    assert_eq!(diff["spec_version"], 3);
    assert_eq!(diff["scope"], serde_json::json!(["src"]));
    assert!(diff["worktree_hash"]
        .as_str()
        .is_some_and(|v| !v.is_empty()));
    assert_eq!(
        diff["changes"],
        serde_json::json!([{"path":"src/a.txt","before":"before\n","after":"after\n"},{"path":"src/delete.txt","before":"old\n","after":null},{"path":"src/new.txt","before":null,"after":"new\n"}])
    );
    assert_eq!(
        serde_json::to_value(
            call(
                &engine,
                Command::GetRun {
                    run_id: f.run.clone()
                }
            )
            .unwrap()
        )
        .unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(
        fs::read_to_string(f.work.join("src/a.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        fs::read_to_string(f.work.join("outside.txt")).unwrap(),
        "outside changed\n"
    );
}

#[test]
fn rejects_moved_head_without_modifying_run() {
    let f = Fixture::new();
    fs::write(f.work.join("src/a.txt"), "new commit\n").unwrap();
    git(&f.work, &["add", "src/a.txt"]);
    git(
        &f.work,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "advance",
        ],
    );
    let engine = Engine::open(&f.db).unwrap();
    let before = call(
        &engine,
        Command::GetRun {
            run_id: f.run.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        call(
            &engine,
            Command::GetDiff {
                run_id: f.run.clone()
            }
        )
        .unwrap_err(),
        "worktree revision changed"
    );
    assert_eq!(
        serde_json::to_value(
            call(
                &engine,
                Command::GetRun {
                    run_id: f.run.clone()
                }
            )
            .unwrap()
        )
        .unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[test]
fn rejects_hardlinks_and_reads_readonly_files() {
    let f = Fixture::new();
    let file = f.work.join("src/a.txt");
    let original_permissions = fs::metadata(&file).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&file, permissions).unwrap();
    let engine = Engine::open(&f.db).unwrap();
    assert!(
        call(
            &engine,
            Command::GetDiff {
                run_id: f.run.clone()
            }
        )
        .is_ok(),
        "read-only file must be inspectable"
    );
    fs::set_permissions(&file, original_permissions).unwrap();
    fs::hard_link(&file, f.work.join("src/alias.txt")).unwrap();
    assert!(
        call(
            &engine,
            Command::GetDiff {
                run_id: f.run.clone()
            }
        )
        .is_err(),
        "hardlinked files must be rejected"
    );
    fs::remove_file(f.work.join("src/alias.txt")).unwrap();
}

#[test]
fn rejects_protected_or_alias_scope_without_opening_it() {
    let f = Fixture::new();
    for path in ["src/.env.fake", "src/CON", "src/file:stream", "../outside"] {
        let database = Database::open(&f.db).unwrap();
        let tx = database.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            let mut value: serde_json::Value = {
                let item = table.get(f.run.as_str()).unwrap().unwrap();
                serde_json::from_str(item.value()).unwrap()
            };
            value["scope"] = serde_json::json!([path]);
            let value = value.to_string();
            table.insert(f.run.as_str(), value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(database);
        let engine = Engine::open(&f.db).unwrap();
        assert!(
            call(
                &engine,
                Command::GetDiff {
                    run_id: f.run.clone()
                }
            )
            .is_err(),
            "{path}"
        );
    }
}

#[test]
fn rejects_junction_inside_scope() {
    let f = Fixture::new();
    let target = f.root.join("other");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("dummy.txt"), "fixture").unwrap();
    let junction = f.work.join("src").join("junction");
    let source = junction
        .to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(junction.to_str().unwrap());
    let destination = target
        .to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(target.to_str().unwrap());
    let status = Process::new("cmd.exe")
        .creation_flags(0x08000000)
        .args(["/c", "mklink", "/J", source, destination])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let engine = Engine::open(&f.db).unwrap();
    assert!(call(
        &engine,
        Command::GetDiff {
            run_id: f.run.clone()
        }
    )
    .is_err());
    assert_eq!(
        fs::read_to_string(target.join("dummy.txt")).unwrap(),
        "fixture"
    );
    drop(engine);
    fs::remove_dir(junction).unwrap();
    fs::remove_file(target.join("dummy.txt")).unwrap();
    fs::remove_dir(target).unwrap();
}

#[test]
fn missing_promisor_blob_is_rejected_without_contacting_remote() {
    use std::{
        io::Write,
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let stop = Arc::clone(&stop);
        let requests = Arc::clone(&requests);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        requests.fetch_add(1, Ordering::AcqRel);
                        let _=stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("fixture listener failed: {error}"),
                }
            }
        })
    };
    git(&f.repo, &["config", "remote.fixture.url", &endpoint]);
    git(&f.repo, &["config", "remote.fixture.promisor", "true"]);
    git(
        &f.repo,
        &["config", "remote.fixture.partialclonefilter", "blob:none"],
    );
    git(&f.repo, &["config", "extensions.partialclone", "fixture"]);
    let oid = git(&f.repo, &["rev-parse", "HEAD:src/a.txt"]);
    assert!(matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()));
    let objects = f.repo.join(".git").join("objects").canonicalize().unwrap();
    let object = objects.join(&oid[..2]).join(&oid[2..]);
    assert!(object
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .starts_with(&objects));
    fs::remove_file(object).unwrap();
    let engine = Engine::open(&f.db).unwrap();
    let result = call(
        &engine,
        Command::GetDiff {
            run_id: f.run.clone(),
        },
    );
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    assert!(result.is_err(), "missing local object must be rejected");
    assert_eq!(
        requests.load(Ordering::Acquire),
        0,
        "GetDiff must not attempt a lazy fetch"
    );
}

#[test]
fn approved_base_revision_ignores_git_replacement_refs() {
    let f = Fixture::new();
    fs::write(f.repo.join("src/a.txt"), "replacement fixture\n").unwrap();
    git(&f.repo, &["add", "src/a.txt"]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "replacement",
        ],
    );
    let replacement = git(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["replace", &f.rev, &replacement]);
    let engine = Engine::open(&f.db).unwrap();
    let diff = match call(
        &engine,
        Command::GetDiff {
            run_id: f.run.clone(),
        },
    )
    .unwrap()
    {
        ResponseData::Data(value) => value,
        _ => panic!("expected diff"),
    };
    assert_eq!(diff["revision"], f.rev);
    assert_eq!(
        diff["changes"],
        serde_json::json!([]),
        "replacement refs must not alter the approved base tree"
    );
}
