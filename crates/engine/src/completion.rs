// 2026-11-15: 지원하지 않는 journal stage를 건너뛰지 않도록 복구 전에 구별한다.
fn parse_completion_record(bytes: &[u8]) -> Result<serde_json::Value, String> {
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("journal exceeds limit".into());
    }
    let record: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid completion journal")?;
    if record.get("version").and_then(|v| v.as_u64()) != Some(1)
        || ["id", "run_id", "project_id"].iter().any(|key| {
            record
                .get(key)
                .and_then(|v| v.as_str())
                .is_none_or(|id| uuid::Uuid::parse_str(id).is_err())
        })
    {
        return Err("invalid completion journal".into());
    }
    let result = record
        .pointer("/outcome/result")
        .cloned()
        .ok_or("invalid completion journal")?;
    match record
        .pointer("/outcome/stage")
        .and_then(|value| value.as_str())
    {
        Some("worktree") => {
            let _: Result<(), String> =
                serde_json::from_value(result).map_err(|_| "invalid completion journal")?;
        }
        Some("generation") => {
            let _: Result<provider::sdd::SpecResult, String> =
                serde_json::from_value(result).map_err(|_| "invalid completion journal")?;
        }
        Some("agent") => {
            let object = result.as_object().ok_or("invalid agent completion")?;
            if object.len() != 2
                || object.get("success").and_then(|v| v.as_bool()).is_none()
                || object
                    .get("attempt")
                    .and_then(|v| v.as_u64())
                    .is_none_or(|n| n == 0)
            {
                return Err("invalid agent completion".into());
            }
        }
        _ => return Err("unsupported completion journal".into()),
    }
    Ok(record)
}

#[cfg(windows)]
fn publish_completion_record(
    directory: &std::path::Path,
    record: &serde_json::Value,
) -> Result<std::path::PathBuf, String> {
    use std::{io::Write, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let bytes = serde_json::to_vec(record).map_err(|_| "journal encoding failed")?;
    let verified = parse_completion_record(&bytes)?;
    let run_id = verified["run_id"]
        .as_str()
        .ok_or("invalid completion journal")?;
    let parent = directory.parent().ok_or("invalid journal directory")?;
    let name = directory.file_name().ok_or("invalid journal directory")?;
    let _pins = crate::tools::pin_directories(parent, std::path::Path::new(name), true)?;
    let target = directory.join(format!("{run_id}.json"));
    let temp = directory.join(format!("{run_id}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| "journal creation failed")?;
    if file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .is_err()
    {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err("journal write failed".into());
    }
    drop(file);
    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // 2026-11-15: 덮어쓰기 없는 원자적 publish로 먼저 기록된 결과를 보존한다.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        let _ = std::fs::remove_file(&temp);
        return Err("journal publish failed".into());
    }
    Ok(target)
}
#[cfg(windows)]
fn read_completion_record(path: &std::path::Path) -> Result<serde_json::Value, String> {
    use std::{
        io::Read,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "journal unavailable")?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // 2026-11-15: 열린 파일을 기준으로 링크와 reparse 지점을 거절하고 교체를 막는다.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0
        || info.dwFileAttributes & 0x400 != 0
        || info.dwFileAttributes & 0x10 != 0
        || info.nNumberOfLinks != 1
    {
        return Err("invalid completion journal".into());
    }
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "journal unavailable")?;
    parse_completion_record(&bytes)
}

pub(crate) struct CompletionManager {
    directory: Option<std::path::PathBuf>,
    mutation_gate: std::sync::Arc<std::sync::Mutex<()>>,
    pending: std::sync::Mutex<Vec<serde_json::Value>>,
    logs: std::sync::Mutex<std::collections::VecDeque<serde_json::Value>>,
    log_gate: std::sync::Mutex<()>,
    log_available: std::sync::atomic::AtomicBool,
}
impl CompletionManager {
    // 2026-11-15: 미지원 및 손상된 기록은 지우지 않고 독립 진단에 남긴다.
    #[cfg(windows)]
    pub(crate) fn load(
        directory: &std::path::Path,
        mutation_gate: std::sync::Arc<std::sync::Mutex<()>>,
    ) -> Result<Self, String> {
        let parent = directory.parent().ok_or("invalid journal directory")?;
        let name = directory.file_name().ok_or("invalid journal directory")?;
        let _pins = crate::tools::pin_directories(parent, std::path::Path::new(name), false)?;
        let mut pending = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(|_| "journal directory unavailable")? {
            let path = entry.map_err(|_| "journal directory unavailable")?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
            let valid_id = uuid::Uuid::parse_str(stem).is_ok();
            let id = if valid_id {
                stem.to_owned()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let record = read_completion_record(&path);
            let known = record
                .as_ref()
                .is_ok_and(|record| valid_id && record["run_id"].as_str() == Some(id.as_str()));
            let project_id = if known {
                record
                    .as_ref()
                    .ok()
                    .and_then(|item| item["project_id"].as_str())
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            };
            pending.push(serde_json::json!({
                "run_id": id,
                "project_id": project_id,
                "stage": if known { record.as_ref().ok().and_then(|item|item.pointer("/outcome/stage")).and_then(|value|value.as_str()).unwrap_or("unknown") } else { "unknown" },
                "state": "pending",
                "attempts": 0,
                "journaled": known,
                "message": "완료 결과 저장을 먼저 복구해야 합니다.",
                "_record": if known { record.ok() } else { None },
                "_path": path
            }));
        }
        Ok(Self {
            directory: Some(directory.to_path_buf()),
            mutation_gate,
            pending: std::sync::Mutex::new(pending),
            logs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            log_gate: std::sync::Mutex::new(()),
            log_available: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub(crate) fn empty(
        directory: std::path::PathBuf,
        mutation_gate: std::sync::Arc<std::sync::Mutex<()>>,
    ) -> Self {
        Self {
            directory: Some(directory),
            mutation_gate,
            pending: std::sync::Mutex::new(Vec::new()),
            logs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            log_gate: std::sync::Mutex::new(()),
            log_available: std::sync::atomic::AtomicBool::new(false),
        }
    }
    // 2026-11-15: 메모리 진단에 먼저 등록한 뒤 디스크 I/O를 수행해 실패 결과를 잃지 않는다.
    #[cfg(windows)]
    pub(crate) fn register(
        &self,
        db: &redb::Database,
        run_id: &str,
        project_id: &str,
        result: Result<(), String>,
    ) {
        self.register_outcome(
            db,
            run_id,
            project_id,
            "worktree",
            serde_json::json!(result),
        );
    }

    // 2026-11-16: 완료 ID와 결과를 handoff 전에 고정해 작업자 반환 오류에도 재사용한다.
    #[cfg(windows)]
    pub(crate) fn generation_record(
        run_id: &str,
        project_id: &str,
        result: Result<provider::sdd::SpecResult, String>,
    ) -> serde_json::Value {
        serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"generation","result":result}})
    }

    #[cfg(all(windows, test))]
    pub(crate) fn register_generation(
        &self,
        db: &redb::Database,
        run_id: &str,
        project_id: &str,
        result: Result<provider::sdd::SpecResult, String>,
    ) {
        self.register_generation_record(db, Self::generation_record(run_id, project_id, result));
    }

    #[cfg(windows)]
    pub(crate) fn register_generation_record(
        &self,
        db: &redb::Database,
        record: serde_json::Value,
    ) {
        self.register_record(db, record);
    }

    // 2026-11-16: 반환 오류에도 최초 완료 ID와 시도 번호를 그대로 재사용한다.
    #[cfg(windows)]
    pub(crate) fn agent_record(
        run_id: &str,
        project_id: &str,
        success: bool,
        attempt: u64,
    ) -> serde_json::Value {
        serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"agent","result":{"success":success,"attempt":attempt}}})
    }

    #[cfg(windows)]
    pub(crate) fn register_agent_record(&self, db: &redb::Database, record: serde_json::Value) {
        self.register_record(db, record);
    }

    #[cfg(windows)]
    fn register_outcome(
        &self,
        db: &redb::Database,
        run_id: &str,
        project_id: &str,
        stage: &str,
        result: serde_json::Value,
    ) {
        self.register_record(db, serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":stage,"result":result}}));
    }

    #[cfg(windows)]
    fn register_record(&self, db: &redb::Database, record: serde_json::Value) {
        let run_id = record["run_id"]
            .as_str()
            .expect("validated run id")
            .to_owned();
        let project_id = record["project_id"]
            .as_str()
            .expect("validated project id")
            .to_owned();
        let stage = record["outcome"]["stage"]
            .as_str()
            .expect("validated stage")
            .to_owned();
        let run_id = run_id.as_str();
        let project_id = project_id.as_str();
        let stage = stage.as_str();
        {
            let _gate = self.mutation_gate.lock().unwrap_or_else(|e| e.into_inner());
            let mut entries = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            entries.push(serde_json::json!({
                "run_id": run_id,
                "project_id": project_id,
                "stage": stage,
                "state": "retrying",
                "attempts": 0,
                "journaled": false,
                "message": "완료 결과가 메모리에만 있습니다. 저장을 재시도하세요.",
                "_record": record
            }));
        }
        if let Some(dir) = &self.directory {
            if let Ok(path) = publish_completion_record(dir, &record) {
                if let Some(item) = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter_mut()
                    .find(|item| item["run_id"].as_str() == Some(run_id))
                {
                    item["journaled"] = true.into();
                    item["message"] = "완료 결과 저장을 다시 시도해야 합니다.".into();
                    item["_path"] = path.to_string_lossy().to_string().into();
                }
            }
        }
        for _ in 0..3 {
            if let Some(item) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter_mut()
                .find(|item| item["run_id"].as_str() == Some(run_id))
            {
                item["attempts"] = item["attempts"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .into();
            }
            if !self.replay(db, Some(run_id)).is_empty() {
                break;
            }
        }
        if let Some(item) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|item| item["run_id"].as_str() == Some(run_id))
        {
            if item["state"] == "retrying" {
                item["state"] = "pending".into();
            }
        }
        if !self.for_run(run_id).is_null() {
            self.log_event(run_id, stage, "storage_pending");
        }
    }
    // 2026-11-15: DB와 별도로 제한된 진단을 유지하며 로그 실패도 메모리에 표시한다.
    #[cfg(windows)]
    fn log_event(&self, run_id: &str, stage: &str, action: &str) {
        use std::{
            io::Write,
            os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        };
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ,
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let entry = serde_json::json!({"version":1,"timestamp_ms":timestamp,"run_id":run_id,"stage":stage,"event":action});
        {
            let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            if logs.len() == 256 {
                logs.pop_front();
            }
            logs.push_back(entry.clone());
        }
        let _gate = self.log_gate.lock().unwrap_or_else(|e| e.into_inner());
        let saved = (|| -> Result<(), String> {
            let dir = self.directory.as_ref().ok_or("log unavailable")?;
            let parent = dir.parent().ok_or("log unavailable")?;
            let name = dir.file_name().ok_or("log unavailable")?;
            let _pins = crate::tools::pin_directories(parent, std::path::Path::new(name), true)?;
            let path = dir.join("events.log");
            let mut options = std::fs::OpenOptions::new();
            options
                .append(true)
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    options.create_new(true);
                }
                Err(_) => return Err("log unavailable".into()),
            };
            let mut file = options.open(path).map_err(|_| "log unavailable")?;
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0
                || info.dwFileAttributes & 0x410 != 0
                || info.nNumberOfLinks != 1
            {
                return Err("unsafe log".into());
            }
            let line = format!("{entry}\n");
            if file
                .metadata()
                .map_err(|_| "log unavailable")?
                .len()
                .saturating_add(line.len() as u64)
                > 1024 * 1024
            {
                return Err("log full".into());
            }
            file.write_all(line.as_bytes())
                .and_then(|_| file.flush())
                .map_err(|_| "log unavailable".to_owned())
        })()
        .is_ok();
        self.log_available
            .store(saved, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn add_startup_issue(&self, directory_issue: bool) {
        let stage = if directory_issue {
            "unknown"
        } else {
            "startup"
        };
        let mut entries = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if entries
            .iter()
            .any(|entry| entry["stage"] == stage && entry["_startup"] == true)
        {
            return;
        }
        let message = if directory_issue {
            "복구 기록 폴더를 확인할 수 없습니다."
        } else {
            "저장소 초기화를 완료하지 못했습니다."
        };
        entries.push(serde_json::json!({
            "run_id": uuid::Uuid::new_v4().to_string(),
            "project_id": "",
            "stage": stage,
            "state": "pending",
            "journaled": false,
            "attempts": 0,
            "message": message,
            "_startup": true
        }));
    }
    pub(crate) fn status(&self) -> serde_json::Value {
        let entries: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|entry| {
                let mut summary = entry.clone();
                if let Some(map) = summary.as_object_mut() {
                    map.remove("_record");
                    map.remove("_path");
                    map.remove("_startup");
                }
                summary
            })
            .collect();
        let logs: Vec<_> = self
            .logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        let log_path = self
            .directory
            .as_ref()
            .map(|directory| directory.join("events.log").to_string_lossy().to_string())
            .unwrap_or_default();
        serde_json::json!({
            "failures": entries,
            "log_path": log_path,
            "log_attempted": !logs.is_empty(),
            "log_available": self.log_available.load(std::sync::atomic::Ordering::Acquire),
            "log_entries": logs
        })
    }
    // 2026-11-15: DB 적용은 journal을 지우기 전에 수행하며 중복 marker로 재적용을 막는다.
    #[cfg(windows)]
    pub(crate) fn replay(&self, db: &redb::Database, only: Option<&str>) -> Vec<(String, bool)> {
        let mut completed = Vec::new();
        let entries = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if entries
            .iter()
            .any(|entry| entry["stage"].as_str() == Some("unknown"))
        {
            return completed;
        }
        for entry in entries {
            if only.is_some_and(|id| entry["run_id"].as_str() != Some(id)) {
                continue;
            }
            let Some(record) = entry.get("_record").filter(|v| v.is_object()) else {
                continue;
            };
            let Some(run_id) = record["run_id"].as_str() else {
                continue;
            };
            let Some(project_id) = record["project_id"].as_str() else {
                continue;
            };
            let Some(completion_id) = record["id"].as_str() else {
                continue;
            };
            if crate::runs::get(db, run_id)
                .ok()
                .and_then(|run| run["project_id"].as_str().map(str::to_owned))
                .as_deref()
                != Some(project_id)
            {
                continue;
            }
            let journaled = entry["journaled"].as_bool().unwrap_or(false);
            // 2026-11-15: 게시되지 않은 결과는 기존 기록이나 접근 불가 상태를 덮지 않는다.
            if !journaled {
                let Some(directory) = self.directory.as_ref() else {
                    continue;
                };
                match std::fs::symlink_metadata(directory.join(format!("{run_id}.json"))) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => continue,
                }
            }
            let applied = match record
                .pointer("/outcome/stage")
                .and_then(|value| value.as_str())
            {
                Some("worktree") => serde_json::from_value::<Result<(), String>>(
                    record["outcome"]["result"].clone(),
                )
                .map_err(|e| e.to_string())
                .and_then(|result| {
                    crate::runs::finish_worktree(db, run_id, &result, completion_id)
                }),
                Some("generation") => serde_json::from_value::<
                    Result<provider::sdd::SpecResult, String>,
                >(record["outcome"]["result"].clone())
                .map_err(|e| e.to_string())
                .and_then(|result| {
                    crate::runs::finish_generation(db, run_id, result, completion_id)
                }),
                Some("agent") => {
                    crate::agent_completion::finish_agent(db, &self.mutation_gate, record)
                }
                _ => Err("unsupported completion journal".into()),
            };
            let Ok(applied) = applied else { continue };
            if let Some(item) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter_mut()
                .find(|item| item["run_id"].as_str() == Some(run_id))
            {
                item["state"] = "cleanup_pending".into();
                item["message"] = "처리 결과를 확인했지만 복구 기록 정리가 필요합니다.".into();
            }
            if journaled {
                let Some(directory) = self.directory.as_ref() else {
                    continue;
                };
                let Some(parent) = directory.parent() else {
                    continue;
                };
                let Some(name) = directory.file_name() else {
                    continue;
                };
                let Ok(_pins) =
                    crate::tools::pin_directories(parent, std::path::Path::new(name), false)
                else {
                    continue;
                };
                let path = directory.join(format!("{run_id}.json"));
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => continue,
                }
            }
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|item| item["run_id"].as_str() != Some(run_id));
            completed.push((run_id.to_owned(), applied));
        }
        completed
    }
    // 2026-11-16: 저장 작업자 반환 오류는 같은 완료 ID의 결과와 진단을 메모리에 남긴다.
    #[cfg(windows)]
    pub(crate) fn preserve_failed_handoff(&self, record: serde_json::Value) {
        let run_id = record["run_id"]
            .as_str()
            .expect("validated run id")
            .to_owned();
        let project_id = record["project_id"]
            .as_str()
            .expect("validated project id")
            .to_owned();
        let stage = record["outcome"]["stage"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let existing = self.directory.as_ref().and_then(|directory| {
            let parent = directory.parent()?;
            let name = directory.file_name()?;
            let _pins =
                crate::tools::pin_directories(parent, std::path::Path::new(name), false).ok()?;
            let path = directory.join(format!("{run_id}.json"));
            (read_completion_record(&path).ok()? == record).then_some(path)
        });
        {
            let _gate = self.mutation_gate.lock().unwrap_or_else(|e| e.into_inner());
            let mut entries = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(item) = entries
                .iter_mut()
                .find(|item| item["run_id"] == run_id && item["_record"]["id"] == record["id"])
            {
                if item["state"] == "retrying" {
                    item["state"] = "pending".into();
                }
                if let Some(path) = &existing {
                    item["journaled"] = true.into();
                    item["_path"] = path.to_string_lossy().to_string().into();
                }
                item["message"] =
                    "저장 작업이 중단되었습니다. 완료 결과 저장을 다시 시도하세요.".into();
            } else if !entries.iter().any(|item| item["run_id"] == run_id) {
                entries.push(serde_json::json!({"run_id":run_id,"project_id":project_id,"stage":stage,"state":"pending","attempts":0,"journaled":existing.is_some(),"message":"저장 작업이 중단되었습니다. 완료 결과 저장을 다시 시도하세요.","_path":existing.map(|p|p.to_string_lossy().to_string()),"_record":record}));
            }
        }
        self.log_event(&run_id, &stage, "storage_handoff_failed");
    }
    pub(crate) fn for_run(&self, run_id: &str) -> serde_json::Value {
        self.status()["failures"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["run_id"].as_str() == Some(run_id))
            })
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
    pub(crate) fn retry(&self, db: &redb::Database, run_id: &str) -> Result<&'static str, String> {
        {
            let mut entries = self
                .pending
                .lock()
                .map_err(|_| "storage state unavailable")?;
            let item = entries
                .iter_mut()
                .find(|item| item["run_id"].as_str() == Some(run_id))
                .ok_or("completion save not pending")?;
            if item["stage"] == "unknown" {
                return Ok("pending");
            }
            if item["state"] == "retrying" {
                return Err("completion save already retrying".into());
            }
            item["state"] = "retrying".into();
            item["attempts"] = item["attempts"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1)
                .into();
        }
        let record = {
            self.pending
                .lock()
                .map_err(|_| "storage state unavailable")?
                .iter()
                .find(|item| item["run_id"].as_str() == Some(run_id) && item["journaled"] == false)
                .and_then(|item| item.get("_record"))
                .filter(|record| record.is_object())
                .cloned()
        };
        if let (Some(directory), Some(record)) = (&self.directory, record) {
            if let Ok(path) = publish_completion_record(directory, &record) {
                if let Some(item) = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter_mut()
                    .find(|item| item["run_id"].as_str() == Some(run_id))
                {
                    item["journaled"] = true.into();
                    item["_path"] = path.to_string_lossy().to_string().into();
                }
            }
        }
        let completed = self.replay(db, Some(run_id));
        let mut entries = self
            .pending
            .lock()
            .map_err(|_| "storage state unavailable")?;
        if let Some(item) = entries
            .iter_mut()
            .find(|item| item["run_id"].as_str() == Some(run_id))
        {
            if item["state"] == "retrying" {
                item["state"] = "pending".into();
            }
            Ok(if item["state"] == "cleanup_pending" {
                "cleanup_pending"
            } else {
                "pending"
            })
        } else {
            Ok(if completed.iter().any(|(_, applied)| !*applied) {
                "superseded"
            } else {
                "saved"
            })
        }
    }
    pub(crate) fn note_reopen_issue(&self, run_id: &str, busy: bool) {
        if let Some(item) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|item| item["run_id"].as_str() == Some(run_id))
        {
            item["message"] = if busy {
                "다른 작업이 저장소를 사용 중입니다. 잠시 뒤 다시 시도하세요."
            } else {
                "저장소를 다시 열지 못했습니다. 저장 장치 상태를 확인한 뒤 다시 시도하세요."
            }
            .into();
        }
        let stage = self.for_run(run_id)["stage"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        self.log_event(
            run_id,
            &stage,
            if busy {
                "storage_busy"
            } else {
                "storage_reopen_failed"
            },
        );
    }
    pub(crate) fn startup_issue(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| {
                item["run_id"].as_str() == Some(id)
                    && item["stage"] == "startup"
                    && item["_startup"] == true
            })
    }
    pub(crate) fn other_pending(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| item["run_id"].as_str() != Some(id))
    }
    pub(crate) fn clear_startup(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|item| item["run_id"].as_str() != Some(id));
    }
    pub(crate) fn blocks_global(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| item["stage"] == "unknown" || item["stage"] == "startup")
    }
    pub(crate) fn blocks_project(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| item["project_id"].as_str() == Some(id))
    }
    pub(crate) fn blocks_run(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| item["run_id"].as_str() == Some(id))
    }
    pub(crate) fn blocked(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_worktree_and_rejects_other_stages_without_rewriting() {
        let fixture = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":uuid::Uuid::new_v4().to_string(),"project_id":uuid::Uuid::new_v4().to_string(),"outcome":{"stage":"worktree","result":{"Ok":null}}});
        let bytes = serde_json::to_vec(&fixture).unwrap();
        assert_eq!(parse_completion_record(&bytes).unwrap(), fixture);
        for stage in ["generation", "unknown"] {
            let mut unsupported = fixture.clone();
            unsupported["outcome"]["stage"] = stage.into();
            let encoded = serde_json::to_vec(&unsupported).unwrap();
            assert!(parse_completion_record(&encoded).is_err());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
                unsupported
            );
        }
    }
    #[cfg(windows)]
    #[test]
    fn journal_publish_does_not_overwrite_an_existing_result() {
        let root = std::env::temp_dir().join(format!("magi-journal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dir = root.join("state.redb.completion-journal");
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":uuid::Uuid::new_v4().to_string(),"project_id":uuid::Uuid::new_v4().to_string(),"outcome":{"stage":"worktree","result":{"Ok":null}}});
        let path = publish_completion_record(&dir, &record).unwrap();
        assert_eq!(
            parse_completion_record(&std::fs::read(&path).unwrap()).unwrap(),
            record
        );
        let mut changed = record.clone();
        changed["id"] = uuid::Uuid::new_v4().to_string().into();
        assert!(publish_completion_record(&dir, &changed).is_err());
        assert_eq!(
            parse_completion_record(&std::fs::read(&path).unwrap()).unwrap(),
            record
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn journal_reader_rejects_multiple_hard_links_without_modifying_them() {
        let root = std::env::temp_dir().join(format!("magi-journal-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("entry.json");
        let alias = root.join("alias.json");
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":uuid::Uuid::new_v4().to_string(),"project_id":uuid::Uuid::new_v4().to_string(),"outcome":{"stage":"worktree","result":{"Ok":null}}});
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_completion_record(&path).unwrap(), record);
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(read_completion_record(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&alias).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn existing_journal_is_not_deleted_or_overwritten_by_another_completion() {
        let root = std::env::temp_dir().join(format!("journal-collision-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dbpath = root.join("state.redb");
        let db = redb::Database::create(&dbpath).unwrap();
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
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
        let path = publish_completion_record(&dir, &record).unwrap();
        let manager =
            CompletionManager::empty(dir.clone(), std::sync::Arc::new(std::sync::Mutex::new(())));
        manager.register(&db, &run_id, &project_id, Err("different result".into()));
        assert_eq!(read_completion_record(&path).unwrap(), record);
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["status"],
            "preparing"
        );
        assert!(!manager.for_run(&run_id).is_null());
        drop(manager);
        drop(db);
        std::fs::remove_file(path).unwrap();
        let log = dir.join("events.log");
        if log.exists() {
            std::fs::remove_file(log).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
        std::fs::remove_file(dbpath).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn journal_and_database_failures_leave_memory_only_diagnostic() {
        let root = std::env::temp_dir().join(format!("journal-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dbpath = root.join("state.redb");
        let db = redb::Database::create(&dbpath).unwrap();
        let directory = root.join("blocked");
        std::fs::write(&directory, b"fixture").unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let manager = CompletionManager::empty(
            directory.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        manager.register(&db, &run_id, &project_id, Ok(()));
        let gate = std::sync::Arc::clone(&manager.mutation_gate);
        let engine = crate::Engine {
            db: crate::storage::StorageSlot::new(dbpath.clone(), db),
            run_root: root.clone(),
            git_gate: std::sync::Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: std::sync::Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let response = engine
            .handle(domain::Request {
                id: "diagnostic".into(),
                command: domain::Command::ListStorageFailures,
            })
            .result
            .unwrap();
        let status = match response {
            domain::ResponseData::Data(value) => value,
            _ => panic!("diagnostic"),
        };
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        assert_eq!(status["log_available"], false);
        assert_eq!(status["log_entries"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(&directory).unwrap(), b"fixture");
        drop(engine);
        std::fs::remove_file(directory).unwrap();
        std::fs::remove_file(dbpath).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn crash_after_publish_child() {
        let Ok(root) = std::env::var("MAGI_TEST_CRASH_DIRECTORY") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("record.json")).unwrap()).unwrap();
        publish_completion_record(&root.join("state.redb.completion-journal"), &record).unwrap();
        std::process::exit(86);
    }
    #[cfg(windows)]
    #[test]
    fn child_exit_after_publish_is_replayed_before_interruption_marking() {
        use std::os::windows::process::CommandExt;
        let root = std::env::temp_dir().join(format!("magi-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dbpath = root.join("state.redb");
        drop(crate::Engine::open(&dbpath).unwrap());
        let db = redb::Database::open(&dbpath).unwrap();
        let tx = db.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let data=serde_json::json!({"id":run_id,"project_id":project_id,"status":"preparing","events":[]}).to_string();
            table.insert(run_id.as_str(), data.as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"worktree","result":{"Ok":null}}});
        std::fs::write(
            root.join("record.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "completion::tests::crash_after_publish_child",
                "--nocapture",
            ])
            .env("MAGI_TEST_CRASH_DIRECTORY", &root)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert_eq!(child.status.code(), Some(86));
        let engine = crate::Engine::open(&dbpath).unwrap();
        let result = engine
            .handle(domain::Request {
                id: "run".into(),
                command: domain::Command::GetRun {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let run = match result {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "awaiting_implementation");
        let events = engine
            .handle(domain::Request {
                id: "events".into(),
                command: domain::Command::ListEvents { run_id, after: 0 },
            })
            .result
            .unwrap();
        let events = match events {
            domain::ResponseData::Events(v) => v,
            _ => panic!("events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "worktree_ready");
        drop(engine);
        std::fs::remove_file(root.join("record.json")).unwrap();
        std::fs::remove_dir(root.join("state.redb.completion-journal")).unwrap();
        std::fs::remove_file(dbpath).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn generation_child_exit_replays_report_before_interruption_without_credentials() {
        use std::os::windows::process::CommandExt;
        let root = std::env::temp_dir().join(format!("magi-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dbpath = root.join("state.redb");
        let engine = crate::Engine::open(&dbpath).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "not-created".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let project_id = project.id.clone();
        let draft_hash = project.draft_hash.clone();
        drop(engine);
        let db = redb::Database::open(&dbpath).unwrap();
        let tx = db.begin_write().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();

        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let data=serde_json::json!({"id":run_id,"project_id":project_id,"status":"generating","events":[],"spec_hash":draft_hash,"context_version":project.context_version}).to_string();
            table.insert(run_id.as_str(), data.as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let draft = domain::SpecDraft {
            title: "생성됨".into(),
            ..Default::default()
        };
        let report = serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null});
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"generation","result":{"Ok":report}}});
        std::fs::write(
            root.join("record.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "completion::tests::crash_after_publish_child",
                "--nocapture",
            ])
            .env("MAGI_TEST_CRASH_DIRECTORY", &root)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert_eq!(child.status.code(), Some(86));
        let engine = crate::Engine::open(&dbpath).unwrap();
        let result = engine
            .handle(domain::Request {
                id: "run".into(),
                command: domain::Command::GetRun {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let run = match result {
            domain::ResponseData::Data(v) => v,
            _ => panic!("run"),
        };
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "생성됨");
        let events = engine
            .handle(domain::Request {
                id: "events".into(),
                command: domain::Command::ListEvents { run_id, after: 0 },
            })
            .result
            .unwrap();
        let events = match events {
            domain::ResponseData::Events(v) => v,
            _ => panic!("events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "generated");
        drop(engine);
        std::fs::remove_file(root.join("record.json")).unwrap();
        std::fs::remove_dir(root.join("state.redb.completion-journal")).unwrap();
        std::fs::remove_file(dbpath).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn generation_journal_and_storage_failure_remain_memory_only_and_visible() {
        let root = std::env::temp_dir().join(format!("journal-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let dbpath = root.join("state.redb");
        let db = redb::Database::create(&dbpath).unwrap();
        let directory = root.join("blocked");
        std::fs::write(&directory, b"fixture").unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let manager = CompletionManager::empty(
            directory.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        manager.register_generation(&db, &run_id, &project_id, Err("fake failure".into()));
        let gate = std::sync::Arc::clone(&manager.mutation_gate);
        let engine = crate::Engine {
            db: crate::storage::StorageSlot::new(dbpath.clone(), db),
            run_root: root.clone(),
            git_gate: std::sync::Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: std::sync::Arc::new(manager),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let response = engine
            .handle(domain::Request {
                id: "diagnostic".into(),
                command: domain::Command::ListStorageFailures,
            })
            .result
            .unwrap();
        let status = match response {
            domain::ResponseData::Data(value) => value,
            _ => panic!("diagnostic"),
        };
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        assert_eq!(status["log_available"], false);
        assert_eq!(status["log_entries"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(&directory).unwrap(), b"fixture");
        drop(engine);
        std::fs::remove_file(directory).unwrap();
        std::fs::remove_file(dbpath).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_panic_before_registration_keeps_result_for_retry() {
        let root =
            std::env::temp_dir().join(format!("magi-handoff-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = crate::Engine::open(&path).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "not-created".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run=serde_json::json!({"id":run_id,"project_id":project.id,"status":"generating","events":[],"spec_hash":project.draft_hash,"context_version":project.context_version}).to_string();
            table.insert(run_id.as_str(), run.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let draft = domain::SpecDraft {
            title: "생성됨".into(),
            ..Default::default()
        };
        let report: provider::sdd::SpecResult = serde_json::from_value(serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null})).unwrap();
        let captured_id = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let worker_id = std::sync::Arc::clone(&captured_id);
        crate::save_generation_result(
            std::sync::Arc::clone(&engine.completions),
            std::sync::Arc::clone(&db),
            run_id.clone(),
            project.id.clone(),
            Ok(report),
            move |_, _, record| {
                *worker_id.lock().unwrap() = record["id"].as_str().unwrap().to_owned();
                panic!("fake handoff failure")
            },
        )
        .await;
        drop(db);
        let status = engine.completions.status();
        assert_eq!(
            status["failures"].as_array().unwrap().len(),
            1,
            "completed result lost after handoff failure"
        );
        let completion_id = captured_id.lock().unwrap().clone();
        assert_eq!(
            engine.completions.pending.lock().unwrap()[0]["_record"]["id"],
            completion_id
        );
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "생성됨");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        {
            let tx = db.begin_read().unwrap();
            let table = tx
                .open_table(redb::TableDefinition::<&str, u8>::new(
                    "completion_commits_v1",
                ))
                .unwrap();
            assert_eq!(
                table.get(completion_id.as_str()).unwrap().unwrap().value(),
                1
            );
        }
        drop(db);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_panic_after_journal_keeps_same_record_for_retry() {
        let root =
            std::env::temp_dir().join(format!("magi-handoff-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = crate::Engine::open(&path).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "not-created".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run=serde_json::json!({"id":run_id,"project_id":project.id,"status":"generating","events":[],"spec_hash":project.draft_hash,"context_version":project.context_version}).to_string();
            table.insert(run_id.as_str(), run.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let draft = domain::SpecDraft {
            title: "생성됨".into(),
            ..Default::default()
        };
        let report: provider::sdd::SpecResult = serde_json::from_value(serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null})).unwrap();
        crate::save_generation_result(
            std::sync::Arc::clone(&engine.completions),
            std::sync::Arc::clone(&db),
            run_id.clone(),
            project.id.clone(),
            Ok(report),
            |manager, _, record| {
                publish_completion_record(manager.directory.as_ref().unwrap(), &record).unwrap();
                panic!("fake failure after journal")
            },
        )
        .await;
        drop(db);
        let status = engine.completions.status();
        assert_eq!(
            status["failures"].as_array().unwrap().len(),
            1,
            "completed result lost after handoff failure"
        );
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], true);
        assert_eq!(status["log_attempted"], true);
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "생성됨");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        drop(db);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_panic_after_commit_never_duplicates_event() {
        let root =
            std::env::temp_dir().join(format!("magi-handoff-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = crate::Engine::open(&path).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "not-created".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run=serde_json::json!({"id":run_id,"project_id":project.id,"status":"generating","events":[],"spec_hash":project.draft_hash,"context_version":project.context_version}).to_string();
            table.insert(run_id.as_str(), run.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let draft = domain::SpecDraft {
            title: "생성됨".into(),
            ..Default::default()
        };
        let report: provider::sdd::SpecResult = serde_json::from_value(serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null})).unwrap();
        crate::save_generation_result(
            std::sync::Arc::clone(&engine.completions),
            std::sync::Arc::clone(&db),
            run_id.clone(),
            project.id.clone(),
            Ok(report),
            |manager, db, record| {
                manager.register_generation_record(db, record);
                panic!("fake failure after commit")
            },
        )
        .await;
        drop(db);
        let status = engine.completions.status();
        assert_eq!(
            status["failures"].as_array().unwrap().len(),
            1,
            "completed result lost after handoff failure"
        );
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "생성됨");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        drop(db);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_panic_after_pending_registration_does_not_duplicate() {
        let root =
            std::env::temp_dir().join(format!("magi-handoff-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        let engine = crate::Engine::open(&path).unwrap();
        let project = match engine
            .handle(domain::Request {
                id: "create".into(),
                command: domain::Command::CreateProject {
                    name: "fixture".into(),
                    path: "not-created".into(),
                },
            })
            .result
            .unwrap()
        {
            domain::ResponseData::Project(project) => project,
            _ => panic!("project"),
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let db = engine.db.get().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let run=serde_json::json!({"id":run_id,"project_id":project.id,"status":"generating","events":[],"spec_hash":project.draft_hash,"context_version":project.context_version}).to_string();
            table.insert(run_id.as_str(), run.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let draft = domain::SpecDraft {
            title: "생성됨".into(),
            ..Default::default()
        };
        let report: provider::sdd::SpecResult = serde_json::from_value(serde_json::json!({"policy_version":1,"draft":draft,"assumptions":["추정"],"evidence":[],"verdicts":[],"issues":[],"revision_directives":[],"score_reason":"fixture","proposed_score":8,"score":8,"rubric_hash":"r","draft_hash":"d","question_review":null})).unwrap();
        crate::save_generation_result(std::sync::Arc::clone(&engine.completions), std::sync::Arc::clone(&db), run_id.clone(), project.id.clone(), Ok(report), |manager,_,record| { manager.pending.lock().unwrap().push(serde_json::json!({"run_id":record["run_id"],"project_id":record["project_id"],"stage":"generation","state":"retrying","attempts":0,"journaled":false,"message":"fixture","_record":record})); panic!("fake failure after pending") }).await;
        drop(db);
        let status = engine.completions.status();
        assert_eq!(
            status["failures"].as_array().unwrap().len(),
            1,
            "completed result lost after handoff failure"
        );
        assert_eq!(status["failures"][0]["stage"], "generation");
        assert_eq!(status["failures"][0]["journaled"], false);
        assert_eq!(status["log_attempted"], true);
        let response = engine.handle(domain::Request {
            id: "retry".into(),
            command: domain::Command::RetryCompletionSave {
                run_id: run_id.clone(),
            },
        });
        let value = match response.result.unwrap() {
            domain::ResponseData::Data(value) => value,
            _ => panic!("retry"),
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "generated");
        assert_eq!(run["report"]["draft"]["title"], "생성됨");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        drop(db);
        drop(engine);
        std::fs::remove_file(path).unwrap();
        let journal = root.join("state.redb.completion-journal");
        std::fs::remove_file(journal.join("events.log")).unwrap();
        std::fs::remove_dir(journal).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

#[cfg(test)]
mod agent_journal_tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn interrupted_completion_preserves_second_attempt_on_retry() {
        let root =
            std::env::temp_dir().join(format!("agent-attempt-event-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":2,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 2);
        manager.preserve_failed_handoff(record.clone());
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["events"][0]["attempt"], 2);
        manager.preserve_failed_handoff(record);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[cfg(windows)]
    #[test]
    fn cancelled_success_clears_worker_even_if_project_is_unavailable() {
        let root =
            std::env::temp_dir().join(format!("agent-cancel-missing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"cancelled","agent_active":true,"agent_attempt":1,"events":[]}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        manager.preserve_failed_handoff(CompletionManager::agent_record(
            &run_id,
            &project_id,
            true,
            1,
        ));
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "cancelled");
        assert_eq!(run["agent_active"], false);
        assert!(run["events"].as_array().unwrap().is_empty());
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_after_worker_finished_clears_same_attempt_on_retry() {
        use std::sync::Arc;
        let root =
            std::env::temp_dir().join(format!("agent-cancel-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = crate::Engine::open(&root.join("state.redb")).unwrap();
        let db = engine.db.get().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1,"custom":"preserved"}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        crate::save_agent_result(Arc::clone(&engine.completions), None, record).await;
        engine
            .handle(domain::Request {
                id: "cancel".into(),
                command: domain::Command::CancelRun {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let before = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(before["status"], "cancelled");
        assert_eq!(before["agent_active"], true);
        let response = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let domain::ResponseData::Data(value) = response else {
            panic!("retry")
        };
        assert_eq!(value["state"], "saved");
        let after = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(after["status"], "cancelled");
        assert_eq!(after["agent_active"], false);
        assert_eq!(after["events"], before["events"]);
        assert_eq!(after["custom"], "preserved");
    }
    #[cfg(windows)]
    #[test]
    fn older_agent_attempt_is_superseded_without_changes() {
        let root = std::env::temp_dir().join(format!("agent-old-attempt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":2,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        manager.preserve_failed_handoff(CompletionManager::agent_record(
            &run_id,
            &project_id,
            false,
            1,
        ));
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "superseded");
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["agent_attempt"], 2);
        assert_eq!(run["agent_active"], true);
        assert!(run["events"].as_array().unwrap().is_empty());
        let mut cancelled = run;
        cancelled["status"] = "cancelled".into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), cancelled.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        manager.preserve_failed_handoff(CompletionManager::agent_record(
            &run_id,
            &project_id,
            false,
            1,
        ));
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "superseded");
        let newer = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(newer["status"], "cancelled");
        assert_eq!(newer["agent_active"], true);
        assert_eq!(newer["agent_attempt"], 2);
    }
    #[cfg(windows)]
    #[test]
    fn cleanup_failure_does_not_duplicate_agent_completion_event() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("agent-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let journal = root.join("journal");
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        let path = publish_completion_record(&journal, &record).unwrap();
        let manager =
            CompletionManager::load(&journal, std::sync::Arc::new(std::sync::Mutex::new(())))
                .unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "cleanup_pending");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(locked);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_worker_panics_keep_same_agent_id_before_and_after_commit() {
        use std::sync::Arc;
        for after_commit in [false, true] {
            let root = std::env::temp_dir().join(format!("agent-panic-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let db = Arc::new(redb::Database::create(root.join("state.redb")).unwrap());
            let run_id = uuid::Uuid::new_v4().to_string();
            let project_id = uuid::Uuid::new_v4().to_string();
            seed_approved_project(&db, &project_id);
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx
                    .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                    .unwrap();
                table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
            }
            tx.commit().unwrap();
            let manager = Arc::new(CompletionManager::empty(
                root.join("journal"),
                Arc::new(std::sync::Mutex::new(())),
            ));
            let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
            crate::save_agent_result_with(
                Arc::clone(&manager),
                Some(Arc::clone(&db)),
                record.clone(),
                move |manager, db, record| {
                    if after_commit {
                        manager.register_agent_record(&db.unwrap(), record);
                    }
                    panic!("fixture handoff failure");
                },
            )
            .await;
            assert_eq!(manager.for_run(&run_id)["stage"], "agent");
            assert_eq!(
                manager.pending.lock().unwrap()[0]["_record"]["id"],
                record["id"]
            );
            assert_eq!(manager.status()["log_entries"].as_array().unwrap().len(), 1);
            assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
            assert_eq!(
                crate::runs::get(&db, &run_id).unwrap()["events"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    #[cfg(windows)]
    #[derive(Debug)]
    struct FailingBackend {
        file: redb::backends::FileBackend,
        broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    #[cfg(windows)]
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
                return Err(std::io::Error::other("fixture"));
            }
            self.file.sync_data(eventual)
        }
        fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
            if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("fixture"));
            }
            self.file.write(offset, data)
        }
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn previous_io_journals_agent_and_same_retry_reopens_original() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let root = std::env::temp_dir().join(format!("agent-io-{}", uuid::Uuid::new_v4()));
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
        let db = redb::Database::builder()
            .set_cache_size(0)
            .create_with_backend(FailingBackend {
                file: redb::backends::FileBackend::new(file).unwrap(),
                broken: Arc::clone(&broken),
            })
            .unwrap();
        crate::initialize_schema(&db).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, u64>::new("fixture"))
                .unwrap();
            table.insert("x", 1).unwrap();
        }
        broken.store(true, Ordering::SeqCst);
        assert!(tx.commit().is_err());
        assert!(matches!(
            db.begin_write(),
            Err(redb::TransactionError::Storage(
                redb::StorageError::PreviousIo
            ))
        ));
        let gate = Arc::new(std::sync::Mutex::new(()));
        let manager = Arc::new(CompletionManager::empty(
            root.join("state.redb.completion-journal"),
            Arc::clone(&gate),
        ));
        let engine = crate::Engine {
            db: crate::storage::StorageSlot::new(path.clone(), db),
            run_root: root.clone(),
            git_gate: Arc::new(std::sync::Mutex::new(())),
            mutation_gate: gate,
            slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_recovery_pending: std::sync::atomic::AtomicBool::new(false),
            completions: Arc::clone(&manager),
            cancellations: std::sync::Mutex::new(std::collections::HashMap::new()),
            credentials: std::sync::Mutex::new(std::collections::HashMap::new()),
            oauth: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        crate::save_agent_result(Arc::clone(&manager), engine.db.get(), record.clone()).await;
        assert_eq!(manager.for_run(&run_id)["journaled"], true);
        assert_eq!(
            manager.pending.lock().unwrap()[0]["_record"]["id"],
            record["id"]
        );
        broken.store(false, Ordering::SeqCst);
        let response = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let domain::ResponseData::Data(value) = response else {
            panic!("retry")
        };
        assert_eq!(value["state"], "saved");
        let reopened = engine.db.get().unwrap();
        let run = crate::runs::get(&reopened, &run_id).unwrap();
        assert_eq!(run["status"], "interrupted");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
    }
    #[cfg(windows)]
    fn seed_approved_project(db: &redb::Database, id: &str) {
        let project = serde_json::json!({"id":id,"name":"fixture","path":"fixture","draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":{"version":1,"hash":"fixture","base_revision":"base","scope":[],"requirements":[],"acceptance":[]},"context_version":1});
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::PROJECTS).unwrap();
            table.insert(id, project.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn published_agent_journal_replays_before_interruption_on_restart() {
        use std::os::windows::process::CommandExt;
        let root = std::env::temp_dir().join(format!("agent-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        drop(crate::Engine::open(&path).unwrap());
        let db = redb::Database::open(&path).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        std::fs::write(
            root.join("record.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .creation_flags(0x08000000)
            .env("MAGI_TEST_CRASH_DIRECTORY", &root)
            .args(["--exact", "completion::tests::crash_after_publish_child"])
            .status()
            .unwrap();
        assert_eq!(child.code(), Some(86));
        let engine = crate::Engine::open(&path).unwrap();
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "interrupted");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
        assert!(engine.completions.status()["failures"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[cfg(windows)]
    #[test]
    fn changed_project_context_supersedes_failed_agent_completion() {
        let root = std::env::temp_dir().join(format!("agent-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let project = serde_json::json!({"id":project_id,"name":"fixture","path":"fixture","draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":{"version":1,"hash":"fixture","base_revision":"base","scope":[],"requirements":[],"acceptance":[]},"context_version":2});
        let run = serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1});
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::PROJECTS).unwrap();
            table
                .insert(project_id.as_str(), project.to_string().as_str())
                .unwrap();
        }
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        manager.preserve_failed_handoff(record.clone());
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "superseded");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["agent_active"],
            false
        );
        manager.preserve_failed_handoff(record);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "superseded");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["agent_active"],
            false
        );
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        drop(db);
        let reopened = redb::Database::open(root.join("state.redb")).unwrap();
        let stored = crate::runs::get(&reopened, &run_id).unwrap();
        assert_eq!(stored["agent_active"], false);
        assert_eq!(stored["status"], "awaiting_implementation");
    }
    #[cfg(windows)]
    #[test]
    fn agent_retry_reopens_locked_existing_database() {
        let root = std::env::temp_dir().join(format!("agent-locked-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("state.redb");
        drop(crate::Engine::open(&path).unwrap());
        let db = redb::Database::open(&path).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table.insert(run_id.as_str(), serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1}).to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let mut name = path.as_os_str().to_os_string();
        name.push(".completion-journal");
        let journal = std::path::PathBuf::from(name);
        std::fs::create_dir(&journal).unwrap();
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        std::fs::write(
            journal.join(format!("{run_id}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let engine = crate::Engine::open(&path).unwrap();
        assert_eq!(engine.completions.for_run(&run_id)["stage"], "agent");
        drop(db);
        let result = engine
            .handle(domain::Request {
                id: "retry".into(),
                command: domain::Command::RetryCompletionSave {
                    run_id: run_id.clone(),
                },
            })
            .result
            .unwrap();
        let domain::ResponseData::Data(value) = result else {
            panic!("retry response")
        };
        assert_eq!(value["state"], "saved");
        let db = engine.db.get().unwrap();
        let run = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(run["status"], "interrupted");
        assert_eq!(run["events"].as_array().unwrap().len(), 1);
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn unavailable_database_keeps_agent_record_for_storage_only_retry() {
        let root = std::env::temp_dir().join(format!("agent-no-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let run = serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1});
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let manager = std::sync::Arc::new(CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        ));
        let record = CompletionManager::agent_record(&run_id, &project_id, false, 1);
        crate::save_agent_result(std::sync::Arc::clone(&manager), None, record.clone()).await;
        assert_eq!(manager.for_run(&run_id)["stage"], "agent");
        assert_eq!(manager.status()["log_entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            manager.pending.lock().unwrap()[0]["_record"]["id"],
            record["id"]
        );
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[cfg(windows)]
    #[test]
    fn cancelled_agent_is_not_revived_by_late_completion() {
        let root = std::env::temp_dir().join(format!("agent-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let run = serde_json::json!({"id":run_id,"project_id":project_id,"status":"cancelled","agent_active":true,"agent_attempt":1,"events":[]});
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"agent","result":{"success":false,"attempt":1}}});
        manager.preserve_failed_handoff(record);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        let stored = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(stored["status"], "cancelled");
        assert_eq!(stored["agent_active"], false);
        assert!(stored["events"].as_array().unwrap().is_empty());
    }
    #[cfg(windows)]
    #[test]
    fn retry_saves_interrupted_agent_without_reexecuting_it() {
        let root = std::env::temp_dir().join(format!("agent-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = redb::Database::create(root.join("state.redb")).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        seed_approved_project(&db, &project_id);
        let run = serde_json::json!({"id":run_id,"project_id":project_id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1,"custom":"preserved"});
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            table
                .insert(run_id.as_str(), run.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let manager = CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":run_id,"project_id":project_id,"outcome":{"stage":"agent","result":{"success":false,"attempt":1}}});
        manager.preserve_failed_handoff(record);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        let stored = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(stored["status"], "interrupted");
        assert_eq!(stored["agent_active"], false);
        assert_eq!(stored["events"].as_array().unwrap().len(), 1);
        assert_eq!(stored["custom"], "preserved");
    }
    #[cfg(windows)]
    #[test]
    fn failed_agent_handoff_is_reported_as_agent_with_original_id() {
        let directory =
            std::env::temp_dir().join(format!("magi-agent-handoff-{}", uuid::Uuid::new_v4()));
        let manager =
            CompletionManager::empty(directory, std::sync::Arc::new(std::sync::Mutex::new(())));
        let record = serde_json::json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"run_id":uuid::Uuid::new_v4().to_string(),"project_id":uuid::Uuid::new_v4().to_string(),"outcome":{"stage":"agent","result":{"success":true,"attempt":1}}});
        manager.preserve_failed_handoff(record.clone());
        let pending = manager.pending.lock().unwrap();
        assert_eq!(pending[0]["stage"], "agent");
        assert_eq!(pending[0]["_record"]["id"], record["id"]);
    }
    #[test]
    fn valid_agent_completion_is_accepted_but_invalid_payload_is_rejected() {
        let value = serde_json::json!({
            "version":1,
            "id":uuid::Uuid::new_v4().to_string(),
            "run_id":uuid::Uuid::new_v4().to_string(),
            "project_id":uuid::Uuid::new_v4().to_string(),
            "outcome":{"stage":"agent","result":{"success":true,"attempt":1}}
        });
        assert_eq!(
            parse_completion_record(&serde_json::to_vec(&value).unwrap()).unwrap(),
            value
        );
        let mut invalid = value;
        invalid["outcome"]["result"]["attempt"] = serde_json::json!("invalid");
        assert!(parse_completion_record(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
}
