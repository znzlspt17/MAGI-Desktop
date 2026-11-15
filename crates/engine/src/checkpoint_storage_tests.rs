use super::*;
use redb::StorageBackend;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
};

struct Fixture {
    root: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-checkpoint-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let db = Database::create(&path).unwrap();
        initialize(&db).unwrap();
        drop(db);
        Self { root, path }
    }
    fn open(&self) -> Database {
        Database::open(&self.path).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.root);
    }
}

#[test]
fn checkpoint_revision_zero_cas_limits_and_reopen_preserve_original_record() {
    let f = Fixture::new();
    let db = f.open();
    assert!(load_checkpoint(&db, "run").unwrap().is_none());
    assert_eq!(
        save_checkpoint(
            &db,
            "run",
            0,
            serde_json::json!({"revision":999,"input":"가짜"})
        )
        .unwrap(),
        1
    );
    assert_eq!(
        save_checkpoint(&db, "run", 0, serde_json::json!({})).unwrap_err(),
        "checkpoint revision conflict"
    );
    assert_eq!(
        save_checkpoint(&db, "run", u64::MAX, serde_json::json!({})).unwrap_err(),
        "checkpoint revision overflow"
    );
    assert_eq!(
        save_checkpoint(
            &db,
            "run",
            1,
            serde_json::json!({"huge":"x".repeat(MAX_DURABLE_RECORD_BYTES)})
        )
        .unwrap_err(),
        "checkpoint too large"
    );
    assert_eq!(
        save_checkpoint(&db, "run", 1, serde_json::json!(null)).unwrap_err(),
        "invalid checkpoint"
    );
    drop(db);
    let db = f.open();
    let (revision, value) = load_checkpoint(&db, "run").unwrap().unwrap();
    assert_eq!(revision, 1);
    assert_eq!(value["revision"], 1);
    assert_eq!(value["input"], "가짜");
    assert_eq!(
        save_checkpoint(&db, "run", 1, serde_json::json!({"input":"다음"})).unwrap(),
        2
    );
}

#[test]
fn concurrent_checkpoint_writers_only_one_wins_revision_zero() {
    let f = Fixture::new();
    let db = Arc::new(f.open());
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|n| {
            let db = Arc::clone(&db);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                save_checkpoint(&db, "run", 0, serde_json::json!({"writer":n}))
            })
        })
        .collect();
    barrier.wait();
    let result: Vec<_> = workers.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(result.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        result
            .iter()
            .filter(|r| r
                .as_ref()
                .is_err_and(|e| e == "checkpoint revision conflict"))
            .count(),
        1
    );
    assert_eq!(load_checkpoint(&db, "run").unwrap().unwrap().0, 1);
}

#[test]
fn call_claim_completion_and_reopen_preserve_identity_and_result() {
    let f = Fixture::new();
    let db = f.open();
    let hash = "a".repeat(64);
    let first = claim_call(&db, "run", "call", &hash).unwrap();
    assert_eq!(first["status"], "planned");
    assert_eq!(first["fresh"], true);
    let second = claim_call(&db, "run", "call", &hash).unwrap();
    assert_eq!(second["fresh"], false);
    assert_eq!(second["status"], "planned");
    assert_eq!(
        claim_call(&db, "run", "call", &"b".repeat(64)).unwrap_err(),
        "call identity conflict"
    );
    assert_eq!(
        claim_call(&db, "run", "", &hash).unwrap_err(),
        "invalid call identity"
    );
    assert_eq!(
        complete_call(&db, "run", "missing", serde_json::json!({})).unwrap_err(),
        "call not found"
    );
    assert_eq!(
        complete_call(
            &db,
            "run",
            "call",
            serde_json::json!({"x":"a".repeat(MAX_DURABLE_RECORD_BYTES)})
        )
        .unwrap_err(),
        "call result too large"
    );
    complete_call(&db, "run", "call", serde_json::json!({"output":"결과"})).unwrap();
    assert_eq!(
        complete_call(&db, "run", "call", serde_json::json!({})).unwrap_err(),
        "call not pending"
    );
    drop(db);
    let db = f.open();
    let record = claim_call(&db, "run", "call", &hash).unwrap();
    assert_eq!(record["status"], "completed");
    assert_eq!(record["fresh"], false);
    assert_eq!(record["result"]["output"], "결과");
}

#[test]
fn initializing_checkpoint_table_preserves_existing_runs_and_unrelated_tables() {
    let f = Fixture::new();
    let db = f.open();
    let tx = db.begin_write().unwrap();
    {
        let mut runs = tx.open_table(RUNS).unwrap();
        runs.insert("run", r#"{"id":"run","status":"cancelled","events":[]}"#)
            .unwrap();
        let mut other = tx
            .open_table(TableDefinition::<&str, &str>::new("existing-fixture"))
            .unwrap();
        other.insert("x", "keep").unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let db = f.open();
    initialize(&db).unwrap();
    let tx = db.begin_read().unwrap();
    let runs = tx.open_table(RUNS).unwrap();
    assert!(runs.get("run").unwrap().is_some());
    let other = tx
        .open_table(TableDefinition::<&str, &str>::new("existing-fixture"))
        .unwrap();
    assert_eq!(other.get("x").unwrap().unwrap().value(), "keep");
}

#[derive(Debug)]
struct FailingBackend {
    file: redb::backends::FileBackend,
    broken: Arc<std::sync::atomic::AtomicBool>,
}
impl StorageBackend for FailingBackend {
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
            return Err(std::io::Error::other("fake storage failure"));
        }
        self.file.sync_data(eventual)
    }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("fake storage failure"));
        }
        self.file.write(offset, data)
    }
}

#[test]
fn real_redb_write_failure_is_returned_without_overwriting_prior_checkpoint_or_call() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = std::env::temp_dir().join(format!("magi-checkpoint-io-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let path = root.join("state.redb");
    let file = fs::OpenOptions::new()
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
    initialize(&db).unwrap();
    save_checkpoint(&db, "run", 0, serde_json::json!({"value":"before"})).unwrap();
    claim_call(&db, "run", "call", &"a".repeat(64)).unwrap();
    broken.store(true, Ordering::SeqCst);
    assert!(save_checkpoint(&db, "run", 1, serde_json::json!({"value":"after"})).is_err());
    assert!(complete_call(&db, "run", "call", serde_json::json!({"result":"after"})).is_err());
    assert!(claim_call(&db, "run", "other", &"a".repeat(64)).is_err());
    broken.store(false, Ordering::SeqCst);
    drop(db);
    let db = Database::open(&path).unwrap();
    let (version, checkpoint) = load_checkpoint(&db, "run").unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(checkpoint["value"], "before");
    let call = claim_call(&db, "run", "call", &"a".repeat(64)).unwrap();
    assert_eq!(call["status"], "planned");
    assert_eq!(call["fresh"], false);
    drop(db);
    fs::remove_file(path).unwrap();
    fs::remove_dir(root).unwrap();
}
