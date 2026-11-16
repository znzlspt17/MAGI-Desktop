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
        let directory = directory.canonicalize().unwrap();
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
        let dir = dir.canonicalize().unwrap();
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
        let dir = dir.canonicalize().unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: slot,
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let root = root.canonicalize().unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let root = root.canonicalize().unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let root = root.canonicalize().unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let root = root.canonicalize().unwrap();
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
        let dir = dir.canonicalize().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = uuid::Uuid::new_v4().to_string();
        let journal = dir.join(format!("{id}.json"));
        let bytes=serde_json::to_vec(&serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":id,"project_id":project,"outcome":{"stage":"worktree","result":{"Ok":null}}})).unwrap();
        std::fs::write(&journal, &bytes).unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = crate::completion::CompletionManager::load(&dir, Arc::clone(&gate)).unwrap();
        let engine = crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::empty(path.clone()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
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
    #[test]
    fn existing_marker_must_be_recommitted_before_success_is_reported() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (initial_status, expected) in [("preparing", true), ("cancelled", false)] {
            let root = std::env::temp_dir().join(format!("magi-marker-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
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
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx
                    .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                    .unwrap();
                let value=serde_json::json!({"id":"run","project_id":"project","status":initial_status,"events":[]}).to_string();
                table.insert("run", value.as_str()).unwrap();
            }
            tx.commit().unwrap();
            assert_eq!(
                crate::runs::finish_worktree(&db, "run", &Ok(()), "marker").unwrap(),
                expected
            );
            broken.store(true, Ordering::SeqCst);
            assert!(crate::runs::finish_worktree(&db, "run", &Ok(()), "marker").is_err());
            broken.store(false, Ordering::SeqCst);
            let slot = StorageSlot::new(path.clone(), db);
            let reopened = slot.reopen_when_idle().unwrap();
            assert_eq!(
                crate::runs::finish_worktree(&reopened, "run", &Ok(()), "marker").unwrap(),
                expected
            );
            let run = crate::runs::get(&reopened, "run").unwrap();
            assert_eq!(
                run["events"].as_array().unwrap().len(),
                usize::from(expected)
            );
            drop(reopened);
            drop(slot);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(root).unwrap();
        }
    }
    #[derive(Debug)]
    struct ReadFaultBackend {
        inner: FailingBackend,
        fail_read: Arc<std::sync::atomic::AtomicBool>,
    }
    impl redb::StorageBackend for ReadFaultBackend {
        fn len(&self) -> std::io::Result<u64> {
            self.inner.len()
        }
        fn read(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
            if self.fail_read.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("fake read failure"));
            }
            self.inner.read(offset, length)
        }
        fn set_len(&self, length: u64) -> std::io::Result<()> {
            self.inner.set_len(length)
        }
        fn sync_data(&self, eventual: bool) -> std::io::Result<()> {
            self.inner.sync_data(eventual)
        }
        fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
            self.inner.write(offset, data)
        }
    }
    #[cfg(windows)]
    fn check_startup_failure(fail_schema: bool) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::env::temp_dir().join(format!("magi-startup-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let fail_read = Arc::new(AtomicBool::new(false));
        let fail_write = Arc::new(AtomicBool::new(false));
        let db = Database::builder()
            .set_cache_size(0)
            .create_with_backend(ReadFaultBackend {
                inner: FailingBackend {
                    file: redb::backends::FileBackend::new(file).unwrap(),
                    broken: Arc::clone(&fail_write),
                },
                fail_read: Arc::clone(&fail_read),
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
            let run=serde_json::json!({"id":run_id,"project_id":project_id,"status":"preparing","events":[]}).to_string();
            table.insert(run_id.as_str(), run.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let dir = root.join("state.redb.completion-journal");
        std::fs::create_dir(&dir).unwrap();
        let file = if fail_schema {
            dir.join(format!("{run_id}.json"))
        } else {
            dir.join("preserve.tmp")
        };
        let bytes = if fail_schema {
            serde_json::to_vec(&serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"worktree","result":{"Ok":null}}})).unwrap()
        } else {
            b"fixture".to_vec()
        };
        std::fs::write(&file, &bytes).unwrap();
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = crate::completion::CompletionManager::load(&dir, Arc::clone(&gate)).unwrap();
        if fail_schema {
            fail_read.store(true, Ordering::SeqCst);
        } else {
            fail_write.store(true, Ordering::SeqCst);
        }
        let slot = crate::open_existing_storage(path.clone(), db, &manager)
            .expect("I/O failure must preserve a diagnostic engine");
        let engine = crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: slot,
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(true),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let status = engine
            .handle(domain::Request {
                id: "status".into(),
                command: domain::Command::ListStorageFailures,
            })
            .result
            .unwrap();
        let status = match status {
            domain::ResponseData::Data(v) => v,
            _ => panic!("status"),
        };
        assert!(!status["failures"].as_array().unwrap().is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        fail_read.store(false, Ordering::SeqCst);
        fail_write.store(false, Ordering::SeqCst);
        let id = status["failures"][0]["run_id"].as_str().unwrap().to_owned();
        let retry = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave { run_id: id },
            })
            .result
            .unwrap();
        let retry = match retry {
            domain::ResponseData::Data(v) => v,
            _ => panic!("retry"),
        };
        assert_eq!(retry["state"], "saved");
        if fail_schema {
            assert!(!file.exists());
        } else {
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            std::fs::remove_file(file).unwrap();
        }
        drop(engine);
        let log = dir.join("events.log");
        if log.exists() {
            std::fs::remove_file(log).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn startup_schema_read_failure_keeps_diagnostic_and_retries() {
        check_startup_failure(true);
    }
    #[cfg(windows)]
    #[test]
    fn startup_run_initialization_failure_keeps_diagnostic_and_retries() {
        check_startup_failure(false);
    }
    #[cfg(windows)]
    #[test]
    fn failed_marker_recommit_keeps_journal_and_original_outcome() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (state, applied) in [("preparing", true), ("cancelled", false)] {
            let root =
                std::env::temp_dir().join(format!("magi-marker-journal-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
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
            let run_id = uuid::Uuid::new_v4().to_string();
            let project_id = uuid::Uuid::new_v4().to_string();
            let marker = uuid::Uuid::new_v4().to_string();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx
                    .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                    .unwrap();
                let data=serde_json::json!({"id":run_id,"project_id":project_id,"status":state,"events":[]}).to_string();
                t.insert(run_id.as_str(), data.as_str()).unwrap();
            }
            tx.commit().unwrap();
            assert_eq!(
                crate::runs::finish_worktree(&db, &run_id, &Ok(()), &marker).unwrap(),
                applied
            );
            let dir = root.join("state.redb.completion-journal");
            std::fs::create_dir(&dir).unwrap();
            let journal = dir.join(format!("{run_id}.json"));
            let bytes=serde_json::to_vec(&serde_json::json!({"version":1,"id":marker,"run_id":run_id,"project_id":project_id,"outcome":{"stage":"worktree","result":{"Ok":null}}})).unwrap();
            std::fs::write(&journal, &bytes).unwrap();
            let manager = crate::completion::CompletionManager::load(
                &dir,
                Arc::new(std::sync::Mutex::new(())),
            )
            .unwrap();
            broken.store(true, Ordering::SeqCst);
            assert_eq!(manager.retry(&db, &run_id).unwrap(), "pending");
            assert_eq!(std::fs::read(&journal).unwrap(), bytes);
            assert_eq!(manager.for_run(&run_id)["state"], "pending");
            broken.store(false, Ordering::SeqCst);
            let slot = StorageSlot::new(path.clone(), db);
            let db = slot.reopen_when_idle().unwrap();
            assert_eq!(
                manager.retry(&db, &run_id).unwrap(),
                if applied { "saved" } else { "superseded" }
            );
            let run = crate::runs::get(&db, &run_id).unwrap();
            assert_eq!(
                run["events"].as_array().unwrap().len(),
                usize::from(applied)
            );
            assert!(!journal.exists());
            drop(db);
            drop(slot);
            drop(manager);
            std::fs::remove_dir(dir).unwrap();
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(root).unwrap();
        }
    }
    #[cfg(windows)]
    #[test]
    fn generation_failure_is_diagnosed_and_retried_after_previous_io() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root =
            std::env::temp_dir().join(format!("magi-generation-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
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
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let value=serde_json::json!({"id":run_id,"project_id":project_id,"status":"generating","events":[]}).to_string();
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
        let journal = root.join("state.redb.completion-journal");
        let manager = crate::completion::CompletionManager::empty(
            journal.clone(),
            Arc::new(std::sync::Mutex::new(())),
        );
        let db = slot.get().unwrap();
        manager.register_generation(&db, &run_id, &project_id, Err("fake failure".into()));
        drop(db);
        let status = manager.status();
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], true);
        broken.store(false, Ordering::SeqCst);
        let engine = crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: slot,
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: Arc::new(std::sync::Mutex::new(())),
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry data"),
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        assert_eq!(crate::runs::get(&db, &run_id).unwrap()["status"], "failed");
        drop(db);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    const GENERATION_HEADINGS: [&str; 24] = [
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
    fn generation_markdown() -> String {
        let mut s = GENERATION_HEADINGS
            .iter()
            .map(|h| format!("{h}\n검색 제공\n"))
            .collect::<String>();
        s.push_str(
            "## 22. 수동 검증 확인 목록\n검색 제공\n## 23. AI 코딩 에이전트 지침\n검색 제공\n",
        );
        s
    }

    fn generation_candidate() -> serde_json::Value {
        serde_json::json!({"policy_version":4,"draft":{"title":"검색","scope":["검색 제공"],"requirements":["검색 제공"],"acceptance":["검색 결과 확인"],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":"abc","markdown":generation_markdown(),"source_links":["requirement-0"],"constraints":["제약 없음"],"tasks":[{"id":"task1","dependencies":[],"owned_paths":["src"],"validation":["read_contains:src/readme.txt:검색"]}],"reused_decisions":[],"task_bindings":[{"task_id":"task1","goal":"검색 구현","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]},"assumptions":[],"evidence":[{"requirement_id":"requirement-0","source":"request","quote":"검색"}],"verdicts":[{"rubric_id":"criterion-0","passed":true,"section":"## 7. 필수 요구사항","quote":"검색 제공","reason":"요구사항 포함","issue_checklist_id":null}],"issues":[],"revision_directives":[],"score_reason":"기준 충족","proposed_score":10,"score":10})
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn loopback_result_survives_previous_io_and_retry_without_another_request() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root =
            std::env::temp_dir().join(format!("magi-generation-result-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
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
        let journal = root.join("state.redb.completion-journal");
        let engine = Arc::new(crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(crate::completion::CompletionManager::empty(
                journal.clone(),
                Arc::clone(&gate),
            )),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        });
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-io-secret".into()),
        });
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "generate".into(),
                command: domain::Command::GenerateSpec {
                    project_id: project.id.clone(),
                },
            })
            .await;
        let run_id = match response.result.unwrap() {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut request = [0; 16384];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("dummy"))
                .unwrap();
            table.insert("x", 1).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(tx.commit().is_err());
        drop(db);
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":generation_candidate().to_string()}]}]}})
        );
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(socket);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !engine.generation_active(&run_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("generation worker did not finish");
        let status = engine.completions.status();
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], true);
        broken.store(false, Ordering::SeqCst);
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .await;
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let run = match call(domain::Command::GetRun {
            run_id: run_id.clone(),
        }) {
            domain::ResponseData::Data(value) => value,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "검색");
        let saved = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        assert_eq!(saved.draft.title, "검색");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cancellation_acknowledgment_keeps_resources_busy_until_worker_returns() {
        let root =
            std::env::temp_dir().join(format!("magi-generation-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = Arc::new(crate::Engine::open(&path).unwrap());
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-cancel-secret".into()),
        });
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "generate".into(),
                command: domain::Command::GenerateSpec {
                    project_id: project.id.clone(),
                },
            })
            .await;
        let run_id = match response.result.unwrap() {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        let (_socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
        {
            let _gate = engine.mutation_gate.lock().unwrap();
            call(domain::Command::CancelRun {
                run_id: run_id.clone(),
            });
            let run = match call(domain::Command::GetRun {
                run_id: run_id.clone(),
            }) {
                domain::ResponseData::Data(value) => value,
                _ => panic!("run"),
            };
            assert_eq!(run["status"], "cancelled");
            assert_eq!(run["resources_released"], false);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !engine.generation_active(&run_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker did not finish");
        let run = match call(domain::Command::GetRun { run_id }) {
            domain::ResponseData::Data(value) => value,
            _ => panic!("run"),
        };
        assert_eq!(run["resources_released"], true);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        if journal.exists() {
            std::fs::remove_dir(journal).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn generation_database_failure_does_not_send_network_request_or_change_project() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let root =
            std::env::temp_dir().join(format!("magi-generation-result-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
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
        let journal = root.join("state.redb.completion-journal");
        let engine = Arc::new(crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(crate::completion::CompletionManager::empty(
                journal.clone(),
                Arc::clone(&gate),
            )),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        });
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-io-secret".into()),
        });
        let before = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => serde_json::to_value(project).unwrap(),
            _ => panic!("project"),
        };
        broken.store(true, Ordering::SeqCst);
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "generate".into(),
                command: domain::Command::GenerateSpec {
                    project_id: project.id.clone(),
                },
            })
            .await;
        assert!(response.result.unwrap_err().contains("fake failure"));
        assert_eq!(engine.slots.load(Ordering::Acquire), 0);
        assert!(engine.cancellations.lock().unwrap().is_empty());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        broken.store(false, Ordering::SeqCst);
        engine.db.reopen_when_idle().unwrap();
        let after = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => serde_json::to_value(project).unwrap(),
            _ => panic!("project"),
        };
        assert_eq!(after, before);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_run_is_not_visible_before_cancel_token_is_installed() {
        let root =
            std::env::temp_dir().join(format!("magi-generation-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = Arc::new(crate::Engine::open(&path).unwrap());
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-cancel-secret".into()),
        });
        let tokens = engine.cancellations.lock().unwrap();
        let worker_engine = Arc::clone(&engine);
        let project_id = project.id.clone();
        let task = tokio::spawn(async move {
            worker_engine
                .handle_async(domain::Request {
                    id: "generate".into(),
                    command: domain::Command::GenerateSpec { project_id },
                })
                .await
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let db = engine.db.get().unwrap();
        let page = crate::runs::list(&db, &project.id, 0, 10).unwrap();
        drop(db);
        assert!(
            page["runs"].as_array().unwrap().is_empty(),
            "run became visible before its cancel token could be registered"
        );
        drop(tokens);
        let response = task.await.unwrap();
        let run_id = match response.result.unwrap() {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        call(domain::Command::CancelRun {
            run_id: run_id.clone(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !engine.generation_active(&run_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker did not finish");
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        if journal.exists() {
            std::fs::remove_dir(journal).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cancellation_reaches_stalled_provider_when_database_is_poisoned() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::AsyncReadExt;
        let root =
            std::env::temp_dir().join(format!("magi-generation-result-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
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
        let journal = root.join("state.redb.completion-journal");
        let engine = Arc::new(crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(crate::completion::CompletionManager::empty(
                journal.clone(),
                Arc::clone(&gate),
            )),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        });
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-io-secret".into()),
        });
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "generate".into(),
                command: domain::Command::GenerateSpec {
                    project_id: project.id.clone(),
                },
            })
            .await;
        let run_id = match response.result.unwrap() {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut request = [0; 16384];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        let before = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => serde_json::to_value(project).unwrap(),
            _ => panic!("project"),
        };
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("dummy"))
                .unwrap();
            table.insert("x", 1).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(tx.commit().is_err());
        drop(db);
        let token = engine
            .cancellations
            .lock()
            .unwrap()
            .get(&run_id)
            .unwrap()
            .clone();
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "cancel".into(),
                command: domain::Command::CancelRun {
                    run_id: run_id.clone(),
                },
            })
            .await;
        assert!(response.result.is_err());
        assert!(
            token.is_cancelled(),
            "DB failure prevented cancellation signal"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while engine.generation_active(&run_id) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled worker retained DB handle");
        broken.store(false, Ordering::SeqCst);
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .await;
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let after = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => serde_json::to_value(project).unwrap(),
            _ => panic!("project"),
        };
        assert_eq!(after, before);
        drop(socket);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn loopback_handoff_panic_preserves_result_and_retry_never_repeats_request() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root =
            std::env::temp_dir().join(format!("magi-generation-result-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
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
        let journal = root.join("state.redb.completion-journal");
        let engine = Arc::new(crate::Engine {
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(crate::completion::CompletionManager::empty(
                journal.clone(),
                Arc::clone(&gate),
            )),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        });
        let call = |command| {
            engine
                .handle(domain::Request {
                    id: "fixture".into(),
                    command,
                })
                .result
                .unwrap()
        };
        let project = match call(domain::Command::CreateProject {
            name: "fixture".into(),
            path: "not-created".into(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        call(domain::Command::SubmitInterview {
            project_id: project.id.clone(),
            message: "검색 제공".into(),
        });
        call(domain::Command::SaveDraft {
            project_id: project.id.clone(),
            draft: domain::SpecDraft {
                base_revision: "abc".into(),
                acceptance: vec!["검색 제공".into()],
                ..Default::default()
            },
        });
        call(domain::Command::ConfigureModel {
            project_id: project.id.clone(),
            provider: "openai".into(),
            model: "gpt-6".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(32000),
            },
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        call(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            credential: domain::SecretString("fake-io-secret".into()),
        });
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "generate".into(),
                command: domain::Command::GenerateSpec {
                    project_id: project.id.clone(),
                },
            })
            .await;
        let run_id = match response.result.unwrap() {
            domain::ResponseData::Run { run_id } => run_id,
            _ => panic!("run"),
        };
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut request = [0; 16384];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        crate::GENERATION_HANDOFF_TEST_FAILURES
            .lock()
            .unwrap()
            .insert(run_id.clone());
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":generation_candidate().to_string()}]}]}})
        );
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(socket);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !engine.generation_active(&run_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("generation worker did not finish");
        let status = engine.completions.status();
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        broken.store(false, Ordering::SeqCst);
        let response = engine
            .clone()
            .handle_async(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .await;
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let run = match call(domain::Command::GetRun {
            run_id: run_id.clone(),
        }) {
            domain::ResponseData::Data(value) => value,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "검색");
        let db = engine.db.get().unwrap();
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == "generated")
                .count(),
            1
        );
        drop(db);
        let saved = match call(domain::Command::GetProject {
            project_id: project.id.clone(),
        }) {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        assert_eq!(saved.draft.title, "검색");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        drop(engine);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn patch_commit_failure_after_file_write_is_not_reexecuted() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::env::temp_dir().join(format!("magi-patch-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();
        let target = worktree.join("src/file.txt");
        std::fs::write(&target, "before").unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let db = engine.db.get().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let proposal = crate::runs::submit_patch(
            &db,
            &run_id,
            "patch".into(),
            serde_json::json!({"path":"src/file.txt","content":"after"}),
        )
        .unwrap();
        let approval = proposal["approval_id"].as_str().unwrap().to_owned();
        *crate::runs::TEST_FAIL_AFTER_PATCH.lock().unwrap() =
            Some((run_id.clone(), Arc::clone(&broken)));
        assert!(crate::runs::approve_patch(&db, &run_id, &approval, true).is_err());
        *crate::runs::TEST_FAIL_AFTER_PATCH.lock().unwrap() = None;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
        drop(db);
        broken.store(false, Ordering::SeqCst);
        let reopened = engine.db.reopen_when_idle().unwrap();
        let run = crate::runs::get(&reopened, &run_id).unwrap();
        assert_eq!(run["status"], "executing");
        std::fs::write(&target, "externally-changed").unwrap();
        assert!(crate::runs::approve_patch(&reopened, &run_id, &approval, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "externally-changed"
        );
        drop(reopened);
        drop(engine);
        let reopened = crate::Engine::open(&path).unwrap();
        let response = reopened
            .handle(domain::Request {
                id: "run".into(),
                command: domain::Command::GetRun { run_id },
            })
            .result
            .unwrap();
        let run = match response {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "uncertain");
        drop(reopened);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(worktree.join("src")).unwrap();
        std::fs::remove_dir(worktree).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn partial_patch_write_becomes_uncertain_and_is_not_reexecuted() {
        use std::sync::atomic::AtomicBool;
        let root = std::env::temp_dir().join(format!("magi-patch-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();
        let target = worktree.join("src/file.txt");
        std::fs::write(&target, "before").unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let db = engine.db.get().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let proposal = crate::runs::submit_patch(
            &db,
            &run_id,
            "patch".into(),
            serde_json::json!({"path":"src/file.txt","content":"after"}),
        )
        .unwrap();
        let approval = proposal["approval_id"].as_str().unwrap().to_owned();
        *crate::runs::TEST_FAIL_AFTER_TRUNCATE.lock().unwrap() = Some(target.clone());
        assert_eq!(
            crate::runs::approve_patch(&db, &run_id, &approval, true).unwrap_err(),
            "injected partial write failure"
        );
        *crate::runs::TEST_FAIL_AFTER_TRUNCATE.lock().unwrap() = None;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "uncertain");
        std::fs::write(&target, "externally-changed").unwrap();
        assert!(crate::runs::approve_patch(&db, &run_id, &approval, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "externally-changed"
        );
        drop(db);
        drop(engine);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(worktree.join("src")).unwrap();
        std::fs::remove_dir(worktree).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn failed_execution_start_commit_never_writes_patch() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::env::temp_dir().join(format!("magi-patch-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();
        let target = worktree.join("src/file.txt");
        std::fs::write(&target, "before").unwrap();
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
            indexing: std::sync::Mutex::new(crate::knowledge_indexing::IndexState::default()),
            db: StorageSlot::new(path.clone(), db),
            completions: Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let db = engine.db.get().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut projects = tx.open_table(crate::PROJECTS).unwrap();
            let mut value = serde_json::to_value(&project).unwrap();
            value["approved"] = serde_json::json!({"version":1,"hash":project.draft_hash,"base_revision":"fixture","scope":["src"],"requirements":["r"],"acceptance":["a"],"accepted_optional":[],"markdown":"","source_links":[],"constraints":[],"tasks":[],"reused_decisions":[],"task_bindings":[]});
            projects
                .insert(project.id.as_str(), value.to_string().as_str())
                .unwrap();
            let mut runs = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run = serde_json::json!({"id":run_id,"project_id":project.id,"spec_hash":project.draft_hash,"spec_version":1,"status":"awaiting_implementation","worktree":worktree,"scope":["src"],"events":[],"pending":null});
            runs.insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let proposal = crate::runs::submit_patch(
            &db,
            &run_id,
            "patch".into(),
            serde_json::json!({"path":"src/file.txt","content":"after"}),
        )
        .unwrap();
        let approval = proposal["approval_id"].as_str().unwrap().to_owned();
        broken.store(true, Ordering::SeqCst);
        assert!(crate::runs::approve_patch(&db, &run_id, &approval, true).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        drop(db);
        broken.store(false, Ordering::SeqCst);
        let reopened = engine.db.reopen_when_idle().unwrap();
        let run = crate::runs::get(&reopened, &run_id).unwrap();
        assert_eq!(run["status"], "awaiting_approval");
        assert_eq!(run["pending"]["id"], approval);
        drop(reopened);
        drop(engine);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(worktree.join("src")).unwrap();
        std::fs::remove_dir(worktree).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
