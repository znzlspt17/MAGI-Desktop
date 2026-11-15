use redb::Database;
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReopenError {
    Busy,
    OpenFailed,
}
pub(crate) struct StorageSlot {
    path: PathBuf,
    database: RwLock<Option<Arc<Database>>>,
}
impl StorageSlot {
    pub(crate) fn new(path: PathBuf, database: Database) -> Self {
        Self {
            path,
            database: RwLock::new(Some(Arc::new(database))),
        }
    }
    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            database: RwLock::new(None),
        }
    }
    pub(crate) fn get(&self) -> Option<Arc<Database>> {
        self.database.try_read().ok()?.clone()
    }
    pub(crate) fn reopen_when_idle(&self) -> Result<Arc<Database>, ReopenError> {
        let mut slot = self.database.try_write().map_err(|_| ReopenError::Busy)?;
        if slot.as_ref().is_some_and(|db| Arc::strong_count(db) != 1) {
            return Err(ReopenError::Busy);
        }
        // 2026-11-15: 기존 핸들을 닫은 뒤 원본 파일만 열어 빈 DB 생성을 막는다.
        drop(slot.take());
        let database = Database::open(&self.path).map_err(|error| match error {
            redb::DatabaseError::DatabaseAlreadyOpen => ReopenError::Busy,
            _ => ReopenError::OpenFailed,
        })?;
        // 2026-11-15: 기존 schema 검증 전에는 다른 요청에 새 핸들을 공개하지 않는다.
        crate::validate_existing_schema(&database).map_err(|_| ReopenError::OpenFailed)?;
        let database = Arc::new(database);
        *slot = Some(Arc::clone(&database));
        Ok(database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reopen_waits_for_live_handles_and_never_creates_missing_database() {
        let directory = std::env::temp_dir().join(format!("magi-slot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.redb");
        let database = Database::create(&path).unwrap();
        crate::initialize_schema(&database).unwrap();
        let slot = StorageSlot::new(path.clone(), database);
        let held = slot.get().unwrap();
        assert!(matches!(slot.reopen_when_idle(), Err(ReopenError::Busy)));
        let tx = held.begin_read().unwrap();
        drop(held);
        assert!(matches!(slot.reopen_when_idle(), Err(ReopenError::Busy)));
        drop(tx);
        drop(slot.reopen_when_idle().expect("idle original opens"));
        drop(slot);
        std::fs::remove_file(&path).unwrap();
        let temp = directory.join("temp.redb");
        let slot = StorageSlot::new(path.clone(), Database::create(&temp).unwrap());
        assert!(matches!(
            slot.reopen_when_idle(),
            Err(ReopenError::OpenFailed)
        ));
        assert!(!path.exists());
        drop(slot);
        std::fs::remove_file(temp).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[derive(Debug)]
    struct FailingBackend {
        file: redb::backends::FileBackend,
        broken: Arc<std::sync::atomic::AtomicBool>,
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
    fn previous_io_reopens_the_existing_database() {
        use redb::TableDefinition;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = std::env::temp_dir().join(format!("magi-poison-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("state.redb");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let broken = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(f).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        let table = TableDefinition::<&str, u64>::new("items");
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(table).unwrap().insert("before", 7).unwrap();
        }
        tx.commit().unwrap();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(table).unwrap().insert("failed", 8).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(matches!(
            tx.commit(),
            Err(redb::CommitError::Storage(redb::StorageError::Io(_)))
        ));
        assert!(matches!(
            db.begin_write(),
            Err(redb::TransactionError::Storage(
                redb::StorageError::PreviousIo
            ))
        ));
        broken.store(false, Ordering::SeqCst);
        let slot = StorageSlot::new(path.clone(), db);
        let db = slot.reopen_when_idle().unwrap();
        let tx = db.begin_read().unwrap();
        assert_eq!(
            tx.open_table(table)
                .unwrap()
                .get("before")
                .unwrap()
                .unwrap()
                .value(),
            7
        );
        drop(tx);
        drop(db);
        drop(slot);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn poisoned_database_keeps_journal_until_save_only_retry_succeeds() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir =
            std::env::temp_dir().join(format!("magi-poison-completion-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("state.redb");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let broken = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let value=serde_json::json!({"id":run_id,"project_id":project_id,"status":"preparing","events":[]}).to_string();
            table.insert(run_id.as_str(), value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("dummy"))
                .unwrap();
            table.insert("x", 1).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(tx.commit().is_err());
        let slot = StorageSlot::new(path.clone(), db);
        let dir_journal = dir.join("state.redb.completion-journal");
        let manager = crate::completion::CompletionManager::empty(
            dir_journal.clone(),
            Arc::new(std::sync::Mutex::new(())),
        );
        let poisoned = slot.get().unwrap();
        manager.register(&poisoned, &run_id, &project_id, Ok(()));
        drop(poisoned);
        let status = manager.status();
        assert_eq!(status["failures"][0]["journaled"], true);
        assert_eq!(status["log_attempted"], true);
        assert!(dir_journal.join(format!("{run_id}.json")).exists());
        broken.store(false, Ordering::SeqCst);
        let engine = crate::Engine {
            db: slot,
            completions: Arc::new(manager),
            run_root: dir.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: Arc::new(std::sync::Mutex::new(())),
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let held = engine.db.get().unwrap();
        let busy = engine
            .handle(domain::Request {
                id: "busy".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let busy = match busy {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry data"),
        };
        assert_eq!(busy["state"], "pending");
        let diagnostic = engine
            .handle(domain::Request {
                id: "status".into(),
                command: domain::Command::ListStorageFailures,
            })
            .result
            .unwrap();
        let diagnostic = match diagnostic {
            domain::ResponseData::Data(value) => value,
            _ => panic!("diagnostic"),
        };
        assert!(diagnostic["failures"][0]["message"]
            .as_str()
            .unwrap()
            .contains("사용 중"));
        drop(held);
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let data = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry data"),
        };
        assert_eq!(data["state"], "saved");
        let run = engine.handle(domain::Request {
            id: "run".into(),
            command: domain::Command::GetRun {
                run_id: run_id.clone(),
            },
        });
        let run = match run.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("run data"),
        };
        assert_eq!(run["status"], "awaiting_implementation");
        let events = engine.handle(domain::Request {
            id: "events".into(),
            command: domain::Command::ListEvents { run_id, after: 0 },
        });
        let events = match events.result.unwrap() {
            domain::ResponseData::Events(events) => events,
            _ => panic!("events"),
        };
        assert_eq!(events.len(), 1);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(dir_journal.join("events.log")).unwrap();
        std::fs::remove_dir(dir_journal).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn real_git_completion_recovers_from_poisoned_database_without_reexecuting_git() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            sync::atomic::{AtomicBool, Ordering},
            time::{Duration, Instant},
        };
        let _test_gate = crate::runs::TEST_GIT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("magi-real-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "fixture").unwrap();
        git(&["add", "a.txt"]);
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
        let path = root.join("state.redb");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let broken = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        crate::runs::initialize(&db).unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = crate::completion::CompletionManager::empty(
            root.join("state.redb.completion-journal"),
            Arc::clone(&gate),
        );
        let engine = crate::Engine {
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
        };
        let invoke = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
        };
        let project = match invoke(domain::Command::CreateProject {
            name: "fake".into(),
            path: repo.to_string_lossy().into_owned(),
        })
        .unwrap()
        {
            domain::ResponseData::Project(p) => p,
            _ => panic!("project"),
        };
        {
            let db = engine.db.get().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx.open_table(crate::PROJECTS).unwrap();
                let mut value = serde_json::to_value(&project).unwrap();
                value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":revision,"scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
                value["model_settings"] = serde_json::json!({"provider":"mock","model":"fixture","capabilities":{"streaming":false,"tools":false,"context_tokens":null}});
                let json = value.to_string();
                table.insert(project.id.as_str(), json.as_str()).unwrap();
            }
            tx.commit().unwrap();
        }
        crate::runs::TEST_GIT_CALLS.store(0, Ordering::SeqCst);
        *crate::runs::TEST_FAIL_AFTER_GIT.lock().unwrap() = Some(Arc::clone(&broken));
        let run_id = match invoke(domain::Command::StartRun {
            project_id: project.id,
            expected_approved_hash: project.draft_hash,
        })
        .unwrap()
        {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = match invoke(domain::Command::ListStorageFailures).unwrap() {
                domain::ResponseData::Data(v) => v,
                _ => panic!("status"),
            };
            if status["failures"][0]["state"] == "pending" && status["log_attempted"] == true {
                assert_eq!(status["failures"][0]["journaled"], true);
                break;
            }
            assert!(Instant::now() < deadline, "completion not pending");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(crate::runs::TEST_GIT_CALLS.load(Ordering::SeqCst), 1);
        {
            let db = engine.db.get().unwrap();
            assert!(matches!(
                db.begin_write(),
                Err(redb::TransactionError::Storage(
                    redb::StorageError::PreviousIo
                ))
            ));
        }
        broken.store(false, Ordering::SeqCst);
        loop {
            let reply = match invoke(domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            })
            .unwrap()
            {
                domain::ResponseData::Data(v) => v,
                _ => panic!("retry"),
            };
            if reply["state"] == "saved" {
                break;
            }
            assert!(Instant::now() < deadline, "retry remains pending");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(crate::runs::TEST_GIT_CALLS.load(Ordering::SeqCst), 1);
        let run = match invoke(domain::Command::GetRun {
            run_id: run_id.clone(),
        })
        .unwrap()
        {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "awaiting_implementation");
        let events = match invoke(domain::Command::ListEvents {
            run_id: run_id.clone(),
            after: 0,
        })
        .unwrap()
        {
            domain::ResponseData::Events(v) => v,
            _ => panic!("events"),
        };
        assert_eq!(
            events.iter().filter(|e| e.kind == "worktree_ready").count(),
            1
        );
        *crate::runs::TEST_FAIL_AFTER_GIT.lock().unwrap() = None;
        drop(engine);
        let reopened = crate::Engine::open(&path).unwrap();
        let recovered = reopened
            .handle(domain::Request {
                id: "reopen".into(),
                command: domain::Command::GetRun { run_id },
            })
            .result
            .unwrap();
        let recovered = match recovered {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(recovered["status"], "awaiting_implementation");
        drop(reopened);
        let resolved = root.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(temp.as_path()));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("magi-real-recovery-"));
        std::fs::remove_dir_all(resolved).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn failed_preparing_commit_never_starts_git() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _test_gate = crate::runs::TEST_GIT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("magi-start-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.redb");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let broken = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        crate::runs::initialize(&db).unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = crate::completion::CompletionManager::empty(
            root.join("state.redb.completion-journal"),
            Arc::clone(&gate),
        );
        let engine = crate::Engine {
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
        };
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fake".into(),
                    path: root.join("not-repo").to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(p) => p,
            _ => panic!("project"),
        };
        crate::runs::TEST_GIT_CALLS.store(0, Ordering::SeqCst);
        let unapproved = engine.handle(domain::Request {
            id: "start".into(),
            command: domain::Command::StartRun {
                project_id: project.id.clone(),
                expected_approved_hash: project.draft_hash.clone(),
            },
        });
        assert_eq!(unapproved.result.unwrap_err(), "approval required");
        assert_eq!(crate::runs::TEST_GIT_CALLS.load(Ordering::SeqCst), 0);
        {
            let db = engine.db.get().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx.open_table(crate::PROJECTS).unwrap();
                let mut value = serde_json::to_value(&project).unwrap();
                value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fake","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
                value["model_settings"] = serde_json::json!({"provider":"mock","model":"fake","capabilities":{"streaming":false,"tools":false,"context_tokens":null}});
                let json = value.to_string();
                table.insert(project.id.as_str(), json.as_str()).unwrap();
            }
            tx.commit().unwrap();
        }
        crate::runs::TEST_GIT_CALLS.store(0, Ordering::SeqCst);
        let stale = engine.handle(domain::Request {
            id: "start".into(),
            command: domain::Command::StartRun {
                project_id: project.id.clone(),
                expected_approved_hash: "stale".into(),
            },
        });
        assert_eq!(stale.result.unwrap_err(), "approved specification changed");
        assert_eq!(crate::runs::TEST_GIT_CALLS.load(Ordering::SeqCst), 0);
        broken.store(true, Ordering::SeqCst);
        let response = engine.handle(domain::Request {
            id: "start".into(),
            command: domain::Command::StartRun {
                project_id: project.id,
                expected_approved_hash: project.draft_hash,
            },
        });
        assert!(response.result.is_err());
        assert_eq!(crate::runs::TEST_GIT_CALLS.load(Ordering::SeqCst), 0);
        assert!(!root.join("magi-runs").exists());
        broken.store(false, Ordering::SeqCst);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn simultaneous_journal_and_redb_failure_remains_visible_until_retry() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::env::temp_dir().join(format!("magi-memory-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.redb");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let broken = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let json =
                serde_json::json!({"id":id,"project_id":project,"status":"preparing","events":[]})
                    .to_string();
            table.insert(id.as_str(), json.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("dummy"))
                .unwrap();
            t.insert("x", 1).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(tx.commit().is_err());
        let journal = root.join("state.redb.completion-journal");
        std::fs::write(&journal, b"blocker").unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager =
            crate::completion::CompletionManager::empty(journal.clone(), Arc::clone(&gate));
        manager.register(&db, &id, &project, Ok(()));
        let engine = crate::Engine {
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
        };
        let response = engine
            .handle(domain::Request {
                id: "status".into(),
                command: domain::Command::ListStorageFailures,
            })
            .result
            .unwrap();
        let status = match response {
            domain::ResponseData::Data(v) => v,
            _ => panic!("status"),
        };
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_available"], false);
        assert_eq!(status["log_attempted"], true);
        std::fs::remove_file(&journal).unwrap();
        broken.store(false, Ordering::SeqCst);
        let response = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave { run_id: id },
            })
            .result
            .unwrap();
        let result = match response {
            domain::ResponseData::Data(v) => v,
            _ => panic!("retry"),
        };
        assert_eq!(result["state"], "saved");
        drop(engine);
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn future_schema_is_never_published_into_live_storage_slot() {
        use redb::ReadableTableMetadata;
        let root = std::env::temp_dir().join(format!("magi-future-slot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.redb");
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut meta = tx.open_table(crate::META).unwrap();
            meta.insert("schema_version", 99).unwrap();
            tx.open_table(crate::PROJECTS).unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let dir = root.join("state.redb.completion-journal");
        std::fs::create_dir(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = uuid::Uuid::new_v4().to_string();
        let journal = dir.join(format!("{id}.json"));
        let bytes=serde_json::to_vec(&serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project,"outcome":{"stage":"worktree","result":{"Ok":null}}})).unwrap();
        std::fs::write(&journal, &bytes).unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = crate::completion::CompletionManager::load(&dir, Arc::clone(&gate)).unwrap();
        let engine = crate::Engine {
            db: StorageSlot::empty(path.clone()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
        };
        let retry = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave { run_id: id },
            })
            .result
            .unwrap();
        assert!(matches!(retry,domain::ResponseData::Data(v) if v["state"]=="pending"));
        let create = engine.handle(domain::Request {
            id: "create".into(),
            command: domain::Command::CreateProject {
                name: "other".into(),
                path: "not-created".into(),
            },
        });
        assert!(create.result.is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), bytes);
        drop(engine);
        let db = Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let table = tx.open_table(crate::PROJECTS).unwrap();
        assert_eq!(table.len().unwrap(), 0);
        drop(table);
        drop(tx);
        drop(db);
        std::fs::remove_file(journal).unwrap();
        let log = dir.join("events.log");
        if log.exists() {
            std::fs::remove_file(log).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
