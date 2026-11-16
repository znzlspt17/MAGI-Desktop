use domain::{Command, Request, ResponseData};
use eframe::egui;
use std::sync::{
    atomic::AtomicBool,
    mpsc::{self, Receiver, SyncSender},
    Arc,
};

pub enum Kind {
    McpSave {
        project_id: String,
        endpoint: String,
        epoch: u64,
        sequence: u64,
    },
    McpList {
        project_id: String,
        epoch: u64,
        sequence: u64,
    },
    AgentSettingsSave {
        project_id: String,
        generation: u64,
    },
    AgentSettingsRefresh {
        project_id: String,
        generation: u64,
    },
    CancelExecution {
        project_id: String,
        run_id: String,
        request: u64,
    },
    StartExecution {
        project_id: String,
        generation: u64,
    },
    GraphPage {
        project_id: String,
        epoch: u64,
        request: u64,
        offset: usize,
    },
    DecisionSave {
        project_id: String,
        epoch: u64,
    },
    DecisionList {
        project_id: String,
        epoch: u64,
        request: u64,
    },
    DecisionRevise {
        project_id: String,
        epoch: u64,
        id: String,
    },
    DecisionSelect {
        project_id: String,
        epoch: u64,
        id: String,
        version: u64,
    },
    DecisionRefresh {
        project_id: String,
        epoch: u64,
        generation: u64,
    },
    KnowledgePut {
        project_id: String,
        epoch: u64,
    },
    KnowledgeSearch {
        project_id: String,
        epoch: u64,
        request: u64,
        query: String,
        offset: usize,
    },
    KnowledgeSource {
        project_id: String,
        epoch: u64,
        request: u64,
        document_id: String,
    },
    HistoryPage {
        project_id: String,
        request: u64,
        offset: usize,
    },
    HistoryDetail {
        project_id: String,
        run_id: String,
        request: u64,
    },
    HistoryEvents {
        project_id: String,
        run_id: String,
        request: u64,
    },
    SaveDraft {
        project_id: String,
        base_hash: String,
        context: u64,
        generation: u64,
    },
    ApproveDraft {
        project_id: String,
        context: u64,
        generation: u64,
    },
    Storage,
    RetrySave {
        run_id: String,
    },
    Starting,
    Status,
    List,
    Get {
        generation: u64,
    },
    Interview {
        project_id: String,
        message: String,
    },
    GenerateSpec {
        project_id: String,
    },
    SpecRun {
        project_id: String,
        run_id: String,
    },
    ModelSettings {
        project_id: String,
        submitted: crate::model_settings::Form,
    },
    Credential {
        project_id: String,
    },
    Create {
        name: String,
        path: String,
    },
}

pub struct Work {
    pub kind: Kind,
    pub command: Command,
}
impl Work {
    pub fn new(kind: Kind, command: Command) -> Self {
        Self { kind, command }
    }
}
pub struct Reply {
    pub kind: Kind,
    pub result: Result<ResponseData, String>,
}

fn should_launch(error: &anyhow::Error) -> bool {
    transport::is_pipe_open_not_found(error)
}

#[derive(Debug)]
struct ProbeTimeout;
impl std::fmt::Display for ProbeTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("엔진 응답 시간 초과")
    }
}
impl std::error::Error for ProbeTimeout {}

fn connect_or_launch(
    runtime: &tokio::runtime::Runtime,
    pipe: &str,
    cancel: &std::sync::atomic::AtomicBool,
    wait_limit: std::time::Duration,
    on_start: impl FnOnce(),
    launch: impl FnOnce() -> Result<std::process::Child, String>,
) -> Result<(ResponseData, Option<std::process::Child>), String> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let request = Request {
        id: "startup".into(),
        command: Command::Status,
    };
    let probe = || {
        #[cfg(test)]
        let began = Instant::now();
        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(400),
                transport::request(pipe, &request),
            )
            .await
            .map_err(|_| anyhow::Error::new(ProbeTimeout))?
        });
        #[cfg(test)]
        eprintln!(
            "기동 probe: {:?}, {}",
            began.elapsed(),
            match &result {
                Ok(_) => "응답",
                Err(error) if error.is::<ProbeTimeout>() => "400ms 응답 제한",
                Err(error) if should_launch(error) => "pipe 미생성",
                Err(_) => "기타 연결 오류",
            }
        );
        result
    };
    if cancel.load(Ordering::Acquire) {
        return Err("엔진 시작 대기가 취소됐습니다 (엔진은 계속 실행될 수 있습니다)".into());
    }
    match probe() {
        Ok(response) => return response.result.map(|value| (value, None)),
        Err(error) if !should_launch(&error) => return Err(format!("엔진 연결 실패: {error:#}")),
        Err(_) => {}
    }
    if cancel.load(Ordering::Acquire) {
        return Err("엔진 시작 대기가 취소됐습니다 (엔진은 계속 실행될 수 있습니다)".into());
    }
    on_start();
    if cancel.load(Ordering::Acquire) {
        return Err("엔진 시작 대기가 취소됐습니다 (엔진은 계속 실행될 수 있습니다)".into());
    }
    let mut child = match launch() {
        Ok(child) => child,
        Err(error) => {
            return match probe() {
                Ok(response) => response.result.map(|value| (value, None)),
                Err(probe_error) if !should_launch(&probe_error) => {
                    Err(format!("엔진 연결 실패: {probe_error:#}"))
                }
                Err(_) => Err(format!("엔진 시작 실패: {error}")),
            }
        }
    };
    let deadline = Instant::now() + wait_limit;
    let mut exited = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("엔진 시작 대기가 취소됐습니다 (엔진은 계속 실행될 수 있습니다)".into());
        }
        match probe() {
            Ok(response) => return response.result.map(|value| (value, Some(child))),
            Err(error) if error.is::<ProbeTimeout>() => {}
            Err(error) if !should_launch(&error) => {
                return Err(format!("엔진 연결 실패: {error:#}"))
            }
            Err(_) => {}
        }
        if exited.is_none() {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("엔진 상태 확인 실패: {error}"))?
            {
                exited = Some((status, Instant::now()));
            }
        }
        if let Some((status, since)) = exited {
            if since.elapsed() >= Duration::from_secs(2) {
                return Err(format!("엔진이 시작 중 종료됐습니다: {status}"));
            }
        }
        if Instant::now() >= deadline {
            return Err("엔진 준비 시간이 초과됐습니다".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn start(ctx: egui::Context) -> (SyncSender<Work>, Receiver<Reply>, Arc<AtomicBool>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = start_with_pipe(ctx, transport::default_pipe_name(), Some(cancel.clone()));
    (tx, rx, cancel)
}
fn start_with_pipe(
    ctx: egui::Context,
    pipe: String,
    startup: Option<Arc<AtomicBool>>,
) -> (SyncSender<Work>, Receiver<Reply>) {
    let (tx, work) = mpsc::sync_channel::<Work>(32);
    let (reply_tx, rx) = mpsc::sync_channel::<Reply>(64);
    let (regular_tx, regular_rx) = mpsc::sync_channel::<Work>(16);
    let (control_tx, control_rx) = mpsc::sync_channel::<Work>(8);
    start_lane(
        ctx.clone(),
        pipe.clone(),
        startup,
        regular_rx,
        reply_tx.clone(),
    );
    start_lane(ctx.clone(), pipe, None, control_rx, reply_tx.clone());
    std::thread::spawn(move || {
        while let Ok(work) = work.recv() {
            let lane = if matches!(&work.kind, Kind::Storage | Kind::RetrySave { .. }) {
                &control_tx
            } else {
                &regular_tx
            };
            if let Err(error) = lane.try_send(work) {
                let work = match error {
                    mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work) => work,
                };
                if reply_tx
                    .send(Reply {
                        kind: work.kind,
                        result: Err("요청 처리 대기열이 가득 찼거나 종료됐습니다".into()),
                    })
                    .is_err()
                {
                    break;
                }
                ctx.request_repaint();
            }
        }
    });
    (tx, rx)
}

fn start_lane(
    ctx: egui::Context,
    pipe: String,
    startup: Option<Arc<AtomicBool>>,
    work: Receiver<Work>,
    reply_tx: SyncSender<Reply>,
) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                while let Ok(work) = work.recv() {
                    if reply_tx
                        .send(Reply {
                            kind: work.kind,
                            result: Err(format!("IPC 실행기 시작 실패: {e}")),
                        })
                        .is_err()
                    {
                        break;
                    }
                    ctx.request_repaint();
                }
                return;
            }
        };
        let mut id = 0u64;
        while let Ok(work) = work.recv() {
            id = id.wrapping_add(1);
            let request = Request {
                id: id.to_string(),
                command: work.command,
            };
            let result = if let (Kind::Status, Some(cancel)) = (&work.kind, &startup) {
                let progress_tx = reply_tx.clone();
                let progress_ctx = ctx.clone();
                connect_or_launch(
                    &runtime,
                    &pipe,
                    cancel,
                    std::time::Duration::from_secs(8),
                    || {
                        let _ = progress_tx.send(Reply {
                            kind: Kind::Starting,
                            result: Ok(ResponseData::Status {
                                message: "엔진 시작 대기 중".into(),
                            }),
                        });
                        progress_ctx.request_repaint();
                    },
                    launch_engine,
                )
                .map(|(response, _child)| response)
            } else {
                match runtime.block_on(transport::request(&pipe, &request)) {
                    Ok(reply) => reply.result,
                    Err(error) => Err(format!("엔진 연결 실패: {error:#}")),
                }
            };
            if reply_tx
                .send(Reply {
                    kind: work.kind,
                    result,
                })
                .is_err()
            {
                break;
            }
            ctx.request_repaint();
        }
    });
}

fn launch_engine() -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA가 없습니다")?;
    let dir = std::path::PathBuf::from(base).join("MAGI Desktop");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    std::process::Command::new(exe.with_file_name("engine.exe"))
        .arg("--database")
        .arg(dir.join("magi.redb"))
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    #[ignore = "검증 대상 engine.exe 경로를 지정해 실행하는 저장 복구 통합 검사"]
    fn storage_ui_recovers_real_engine_journal_after_database_unlock() {
        storage_ui_recovers_real_engine_journal(false);
    }

    #[test]
    #[ignore = "Agent 완료 저장 기능이 포함된 engine.exe 경로를 지정해 실행하는 통합 검사"]
    fn agent_storage_ui_recovers_real_engine_journal_after_database_unlock() {
        storage_ui_recovers_real_engine_journal(true);
    }

    fn storage_ui_recovers_real_engine_journal(agent: bool) {
        use std::os::windows::process::CommandExt;

        struct Fixture {
            dir: std::path::PathBuf,
            run: String,
            child: Option<std::process::Child>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Some(mut child) = self.child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let journal = self.dir.join("state.redb.completion-journal");
                let _ = std::fs::remove_file(journal.join(format!("{}.json", self.run)));
                let _ = std::fs::remove_file(journal.join("events.log"));
                let _ = std::fs::remove_dir(&journal);
                let _ = std::fs::remove_file(self.dir.join("state.redb"));
                let _ = std::fs::remove_dir(&self.dir);
            }
        }
        let exe = std::path::PathBuf::from(
            std::env::var_os("MAGI_TEST_RECOVERY_ENGINE").expect("검증 대상 엔진 경로 필요"),
        );
        assert!(exe.is_absolute() && exe.is_file());
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = temp.join(format!("magi-ui-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        assert!(dir.starts_with(&temp) && dir != temp);
        let database = dir.join("state.redb");
        let run = uuid::Uuid::new_v4().to_string();
        let mut fixture = Fixture {
            dir,
            run: run.clone(),
            child: None,
        };
        let engine = engine::Engine::open(&database).unwrap();
        let project = match engine
            .handle(Request {
                id: "create".into(),
                command: Command::CreateProject {
                    name: "임시 프로젝트".into(),
                    path: "C:/test".into(),
                },
            })
            .result
            .unwrap()
        {
            ResponseData::Project(project) => project,
            _ => panic!("프로젝트 생성 응답"),
        };
        drop(engine);
        let locked = redb::Database::open(&database).unwrap();
        let tx = locked.begin_write().unwrap();
        {
            if agent {
                let mut projects = tx
                    .open_table(redb::TableDefinition::<&str, &str>::new("projects"))
                    .unwrap();
                let mut value = serde_json::to_value(&project).unwrap();
                value["draft_hash"] = "fixture".into();
                value["context_version"] = 1.into();
                value["approved"] = serde_json::json!({"version":1,"hash":"fixture","base_revision":"base","scope":[],"requirements":[],"acceptance":[]});
                projects
                    .insert(project.id.as_str(), value.to_string().as_str())
                    .unwrap();
            }
            let mut table = tx
                .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
                .unwrap();
            let value = if agent {
                serde_json::json!({"id":run,"project_id":project.id,"status":"awaiting_implementation","agent_active":true,"agent_attempt":1,"events":[],"spec_hash":"fixture","spec_version":1,"context_version":1})
            } else {
                serde_json::json!({"id":run,"project_id":project.id,"status":"preparing","events":[]})
            }.to_string();
            table.insert(run.as_str(), value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        let journal = fixture.dir.join("state.redb.completion-journal");
        std::fs::create_dir(&journal).unwrap();
        let completion_id = uuid::Uuid::new_v4().to_string();
        let outcome = if agent {
            serde_json::json!({"stage":"agent","result":{"success":false,"attempt":1}})
        } else {
            serde_json::json!({"stage":"worktree","result":{"Ok":null}})
        };
        let record = serde_json::json!({"version":1,"id":completion_id,"run_id":run,"project_id":project.id,"outcome":outcome});
        std::fs::write(
            journal.join(format!("{run}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let pipe = format!(r"\\.\pipe\magi-ui-recovery-{}", uuid::Uuid::new_v4());
        fixture.child = Some(
            std::process::Command::new(exe)
                .arg("--database")
                .arg(&database)
                .arg("--pipe")
                .arg(&pipe)
                .creation_flags(0x08000000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let request = Request {
                id: "ready".into(),
                command: Command::Status,
            };
            if runtime
                .block_on(transport::request(&pipe, &request))
                .is_ok()
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "엔진 준비 시간 초과");
            std::thread::sleep(Duration::from_millis(30));
        }
        let ctx = egui::Context::default();
        let (tx, rx) = start_with_pipe(ctx.clone(), pipe.clone(), None);
        let mut app = crate::App {
            state: crate::State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let frame = |app: &mut crate::App, events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        app.poll_storage();
        loop {
            frame(&mut app, vec![]);
            if app.state.storage.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .failures
                    .iter()
                    .any(|failure| failure.run_id == run)
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "저장 진단 응답 시간 초과"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = frame(&mut app, vec![]);
        let find_text = |output: &egui::FullOutput, value: &str| {
            output.shapes.iter().find_map(|shape| {
                if let egui::epaint::Shape::Text(text) = &shape.shape {
                    let center = text.visual_bounding_rect().center();
                    if text.galley.job.text.contains(value) && shape.clip_rect.contains(center) {
                        return Some(center);
                    }
                }
                None
            })
        };
        assert!(find_text(&output, "아직 저장되지 않음").is_some());
        assert!(find_text(&output, "events.log").is_some());
        assert!(find_text(&output, &run).is_some());
        if agent {
            assert!(find_text(&output, "단계: agent").is_some());
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .events
                .push(egui::ViewportEvent::Close);
            let close = ctx.run(input, |ctx| app.render(ctx));
            assert!(close.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose)));
            loop {
                let output = frame(&mut app, vec![]);
                assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .iter()
                    .any(|command| matches!(command, egui::ViewportCommand::Close)));
                if app.state.close_confirmation {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "종료 전 저장 진단 시간 초과"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = frame(&mut app, vec![]);
            let keep = find_text(&frame(&mut app, vec![]), "창 유지").expect("창 유지 선택");
            frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(keep),
                    egui::Event::PointerButton {
                        pos: keep,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                &mut app,
                vec![egui::Event::PointerButton {
                    pos: keep,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert!(!app.state.close_allowed && !app.state.close_confirmation);
        }
        let retry = find_text(&output, "완료 저장 재시도").expect("재시도 버튼");
        drop(locked);
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(retry),
                egui::Event::PointerButton {
                    pos: retry,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            &mut app,
            vec![egui::Event::PointerButton {
                pos: retry,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            frame(&mut app, vec![]);
            if app.state.storage.known
                && !app.state.storage.pending_poll
                && app
                    .state
                    .storage
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.failures.is_empty())
                && app
                    .state
                    .storage
                    .notice
                    .as_deref()
                    .is_some_and(|n| n.contains("저장을 복구했습니다"))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "저장 복구 응답 시간 초과"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = frame(&mut app, vec![]);
        assert!(find_text(&output, "아직 저장되지 않음").is_none());
        assert!(find_text(&output, "완료 결과 저장을 복구했습니다").is_some());
        drop(app);
        let mut child = fixture.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let db = redb::Database::open(&database).unwrap();
        let tx = db.begin_read().unwrap();
        let table = tx
            .open_table(redb::TableDefinition::<&str, &str>::new("runs"))
            .unwrap();
        let value = table.get(run.as_str()).unwrap().unwrap();
        let stored: serde_json::Value = serde_json::from_str(value.value()).unwrap();
        assert_eq!(stored["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            stored["status"],
            if agent {
                "interrupted"
            } else {
                "awaiting_implementation"
            }
        );
        if agent {
            assert_eq!(stored["agent_active"], false);
        }
        assert_eq!(
            stored["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["kind"]
                    == if agent {
                        "interrupted"
                    } else {
                        "worktree_ready"
                    })
                .count(),
            1
        );
        drop(value);
        drop(table);
        if agent {
            let markers = tx
                .open_table(redb::TableDefinition::<&str, u8>::new(
                    "completion_commits_v1",
                ))
                .unwrap();
            assert_eq!(
                markers
                    .get(completion_id.as_str())
                    .unwrap()
                    .unwrap()
                    .value(),
                1
            );
        }
        drop(tx);
        drop(db);
    }

    #[test]
    fn storage_poll_is_not_blocked_by_slow_project_request() {
        let pipe = format!(r"\\.\pipe\magi-storage-priority-{}", std::process::id());
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let name = pipe.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (requests, mut rx) = tokio::sync::mpsc::channel::<(
                    Request,
                    tokio::sync::oneshot::Sender<domain::Response>,
                )>(16);
                tokio::spawn(async move {
                    while let Some((request, reply)) = rx.recv().await {
                        tokio::spawn(async move {
                            if matches!(request.command, Command::ListProjects) {
                                tokio::time::sleep(Duration::from_millis(1200)).await;
                            }
                            let _ = reply.send(domain::Response {
                                id: request.id,
                                result: Ok(ResponseData::Data(serde_json::json!({"failures":[]}))),
                            });
                        });
                    }
                });
                let server_name = name.clone();
                let task =
                    tokio::spawn(async move { transport::serve(&server_name, requests).await });
                let probe = Request {
                    id: "probe".into(),
                    command: Command::Status,
                };
                for _ in 0..100 {
                    if transport::request(&name, &probe).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                ready_tx.send(()).unwrap();
                let _ = stop_rx.await;
                task.abort();
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (tx, rx) = start_with_pipe(egui::Context::default(), pipe, None);
        tx.send(Work::new(Kind::List, Command::ListProjects))
            .unwrap();
        tx.send(Work::new(Kind::Storage, Command::ListStorageFailures))
            .unwrap();
        let first = rx.recv_timeout(Duration::from_millis(600));
        drop(tx);
        let _ = stop_tx.send(());
        server.join().unwrap();
        assert!(matches!(
            first,
            Ok(Reply {
                kind: Kind::Storage,
                result: Ok(_)
            })
        ));
    }

    #[test]
    fn starts_hidden_child_and_connects_to_its_real_pipe() {
        use std::os::windows::process::CommandExt;
        use std::sync::atomic::AtomicBool;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pipe = format!(r"\\.\pipe\magi-auto-{}-{unique}", std::process::id());
        let dir = std::env::temp_dir().join(format!("magi-auto-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = connect_or_launch(
            &runtime,
            &pipe,
            &AtomicBool::new(false),
            Duration::from_secs(8),
            || {},
            || {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "worker::tests::fake_engine_process",
                        "--nocapture",
                    ])
                    .env("MAGI_TEST_AUTO_PIPE", &pipe)
                    .env("MAGI_TEST_AUTO_DB", dir.join("test.redb"))
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())
            },
        );
        if result.is_err() {
            std::fs::remove_dir(&dir).unwrap();
        }
        let (response, child) = result.expect("별도 프로세스 엔진 연결");
        assert!(matches!(response, ResponseData::Status { .. }));
        let mut child = child.expect("엔진을 기동해야 함");
        let (response, reused) = connect_or_launch(
            &runtime,
            &pipe,
            &AtomicBool::new(false),
            Duration::from_secs(8),
            || panic!("재사용 시 시작 알림 금지"),
            || panic!("기존 엔진을 다시 기동하면 안 됨"),
        )
        .unwrap();
        assert!(matches!(response, ResponseData::Status { .. }));
        assert!(reused.is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_file(dir.join("test.redb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn cancel_stops_waiting_without_starting_again() {
        use std::os::windows::process::CommandExt;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let pipe = format!(r"\\.\pipe\magi-cancel-{}", std::process::id());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            signal.store(true, Ordering::Release);
        });
        let count = AtomicUsize::new(0);
        let result = connect_or_launch(
            &runtime,
            &pipe,
            &cancel,
            Duration::from_secs(8),
            || {},
            || {
                count.fetch_add(1, Ordering::Relaxed);
                std::process::Command::new("cmd.exe")
                    .args(["/C", "exit", "0"])
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())
            },
        );
        notifier.join().unwrap();
        assert!(result.err().unwrap().contains("취소"));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn startup_failure_and_timeout_are_visible() {
        use std::os::windows::process::CommandExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pipe = format!(r"\\.\pipe\magi-timeout-{}", std::process::id());
        let cancel = AtomicBool::new(false);
        let result = connect_or_launch(
            &runtime,
            &pipe,
            &cancel,
            Duration::from_millis(200),
            || {},
            || Err("실행 파일 없음".into()),
        );
        assert!(result.err().unwrap().contains("실행 파일 없음"));
        let result = connect_or_launch(
            &runtime,
            &pipe,
            &cancel,
            Duration::from_millis(200),
            || {},
            || {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "worker::tests::fake_engine_process",
                        "--nocapture",
                    ])
                    .env("MAGI_TEST_AUTO_SLEEP", "1")
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())
            },
        );
        assert!(result.err().unwrap().contains("시간이 초과"));
        std::thread::sleep(Duration::from_millis(600));
    }

    #[test]
    fn invalid_pipe_never_launches_a_child() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = connect_or_launch(
            &runtime,
            r"\\remote\pipe\magi",
            &std::sync::atomic::AtomicBool::new(false),
            Duration::from_secs(8),
            || panic!("시작 알림 금지"),
            || panic!("원격 이름으로 시작 금지"),
        );
        assert!(result.err().unwrap().contains("invalid local pipe name"));
    }

    #[test]
    fn fake_engine_process() {
        if std::env::var_os("MAGI_TEST_AUTO_SLEEP").is_some() {
            std::thread::sleep(Duration::from_millis(500));
            return;
        }
        let Some(pipe) = std::env::var_os("MAGI_TEST_AUTO_PIPE") else {
            return;
        };
        let database = std::env::var_os("MAGI_TEST_AUTO_DB").unwrap();
        let engine = Arc::new(engine::Engine::open(std::path::Path::new(&database)).unwrap());
        eprintln!("테스트 엔진 DB 준비됨");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let slow_first = std::env::var_os("MAGI_TEST_AUTO_SLOW").is_some();
        runtime.block_on(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(
                Request,
                tokio::sync::oneshot::Sender<domain::Response>,
            )>(32);
            tokio::spawn(async move {
                let mut first = slow_first;
                while let Some((req, reply)) = rx.recv().await {
                    if first {
                        eprintln!("테스트 엔진 첫 요청 수신");
                        first = false;
                        tokio::time::sleep(Duration::from_millis(550)).await;
                    }
                    let _ = reply.send(engine.clone().handle_async(req).await);
                    eprintln!("테스트 엔진 응답 처리됨");
                }
            });
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                transport::serve(&pipe.to_string_lossy(), tx),
            )
            .await;
        });
    }

    #[test]
    fn startup_waits_through_a_slow_initial_status_response() {
        use std::os::windows::process::CommandExt;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pipe = format!(r"\\.\pipe\magi-slow-{}-{unique}", std::process::id());
        let dir = std::env::temp_dir().join(format!("magi-slow-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = connect_or_launch(
            &runtime,
            &pipe,
            &AtomicBool::new(false),
            Duration::from_secs(3),
            || {},
            || {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "worker::tests::fake_engine_process",
                        "--nocapture",
                    ])
                    .env("MAGI_TEST_AUTO_PIPE", &pipe)
                    .env("MAGI_TEST_AUTO_DB", dir.join("test.redb"))
                    .env("MAGI_TEST_AUTO_SLOW", "1")
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .map_err(|e| e.to_string())
            },
        );
        if result.is_err() {
            std::thread::sleep(Duration::from_secs(5));
            let _ = std::fs::remove_file(dir.join("test.redb"));
            let _ = std::fs::remove_dir(&dir);
        }
        let (_, child) = result.expect("첫 응답 지연 뒤 준비된 엔진 연결");
        let mut child = child.unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_file(dir.join("test.redb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn simultaneous_startups_connect_to_winning_engine() {
        use std::os::windows::process::CommandExt;
        use std::sync::{atomic::AtomicBool, Barrier};
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pipe = format!(r"\\.\pipe\magi-race-{}-{unique}", std::process::id());
        let dir = std::env::temp_dir().join(format!("magi-race-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let pipe = pipe.clone();
                let database = dir.join("test.redb");
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    connect_or_launch(
                        &runtime,
                        &pipe,
                        &AtomicBool::new(false),
                        Duration::from_secs(4),
                        || {
                            barrier.wait();
                        },
                        || {
                            std::process::Command::new(std::env::current_exe().unwrap())
                                .args([
                                    "--exact",
                                    "worker::tests::fake_engine_process",
                                    "--nocapture",
                                ])
                                .env("MAGI_TEST_AUTO_PIPE", &pipe)
                                .env("MAGI_TEST_AUTO_DB", &database)
                                .creation_flags(0x08000000)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                                .map_err(|e| e.to_string())
                        },
                    )
                })
            })
            .collect();
        let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
        for result in results {
            let (response, child) = result.expect("경합 중 실행된 엔진에 연결");
            assert!(matches!(response, ResponseData::Status { .. }));
            if let Some(mut child) = child {
                if child.try_wait().unwrap().is_none() {
                    child.kill().unwrap();
                }
                child.wait().unwrap();
            }
        }
        std::fs::remove_file(dir.join("test.redb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    #[ignore = "빌드된 engine.exe 경로를 지정해 별도 실행하는 Windows 통합 검사"]
    fn starts_real_engine_with_temporary_database_and_pipe() {
        use std::os::windows::process::CommandExt;
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let exe = std::env::var_os("MAGI_TEST_ENGINE_EXE").expect("테스트용 engine.exe 경로 필요");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pipe = format!(r"\\.\pipe\magi-real-engine-{}-{unique}", std::process::id());
        let dir =
            std::env::temp_dir().join(format!("magi-real-engine-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let database = dir.join("test.redb");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (response, child) = connect_or_launch(
            &runtime,
            &pipe,
            &AtomicBool::new(false),
            Duration::from_secs(8),
            || {},
            || {
                std::process::Command::new(&exe)
                    .arg("--database")
                    .arg(&database)
                    .arg("--pipe")
                    .arg(&pipe)
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|error| error.to_string())
            },
        )
        .expect("실제 엔진에 연결");
        let guard = ChildGuard(child.expect("테스트 소유 엔진 프로세스"));
        assert!(matches!(response, ResponseData::Status { .. }));
        let request = Request {
            id: "test-create".into(),
            command: Command::CreateProject {
                name: "테스트".into(),
                path: "C:/test".into(),
            },
        };
        let reply = runtime
            .block_on(transport::request(&pipe, &request))
            .unwrap();
        assert!(
            matches!(reply.result.unwrap(), ResponseData::Project(project) if project.name == "테스트")
        );
        drop(guard);
        std::fs::remove_file(database).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn only_an_actual_open_absence_triggers_launch() {
        let pipe = format!(r"\\.\pipe\magi-absent-{}", std::process::id());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(transport::request(
                &pipe,
                &Request {
                    id: "probe".into(),
                    command: Command::Status,
                },
            ))
            .unwrap_err();
        assert!(should_launch(&error));
        assert!(!should_launch(&anyhow::anyhow!(
            "pipe server user mismatch"
        )));
        assert!(!should_launch(&anyhow::anyhow!(
            "unsupported protocol version"
        )));
        assert!(!should_launch(&anyhow::anyhow!("request timed out")));
        assert!(!should_launch(&std::io::Error::from_raw_os_error(2).into()));
        assert!(!should_launch(&std::io::Error::from_raw_os_error(5).into()));
    }

    #[test]
    fn reconnects_and_creates_then_reads_project_through_real_pipe() {
        let name = format!(r"\\.\pipe\magi-desktop-test-{}", std::process::id());
        let (tx, rx) = start_with_pipe(egui::Context::default(), name.clone(), None);
        tx.send(Work::new(Kind::Status, Command::Status)).unwrap();
        assert!(rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .result
            .is_err());
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("magi-desktop-test-{}-{suffix}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let engine = Arc::new(engine::Engine::open(&dir.join("test.redb")).unwrap());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (requests, mut pending) = tokio::sync::mpsc::channel::<(Request, tokio::sync::oneshot::Sender<domain::Response>)>(32);
                tokio::spawn(async move {
                    while let Some((request, reply)) = pending.recv().await {
                        let _ = reply.send(engine.clone().handle_async(request).await);
                    }
                });
                tokio::select! { result = transport::serve(&name, requests) => { result.unwrap(); }, _ = stop_rx => {} }
            });
        });
        let mut connected = false;
        for _ in 0..50 {
            tx.send(Work::new(Kind::Status, Command::Status)).unwrap();
            if rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .result
                .is_ok()
            {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(connected);
        tx.send(Work::new(
            Kind::Create {
                name: "테스트".into(),
                path: "C:/work".into(),
            },
            Command::CreateProject {
                name: "테스트".into(),
                path: "C:/work".into(),
            },
        ))
        .unwrap();
        let project = match rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .result
            .unwrap()
        {
            ResponseData::Project(p) => p,
            _ => panic!("unexpected response"),
        };
        tx.send(Work::new(Kind::List, Command::ListProjects))
            .unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap().result.unwrap(), ResponseData::Projects(p) if p.iter().any(|p| p.id == project.id))
        );
        tx.send(Work::new(
            Kind::Get { generation: 1 },
            Command::GetProject {
                project_id: project.id.clone(),
            },
        ))
        .unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap().result.unwrap(), ResponseData::Project(p) if p.id == project.id)
        );
        drop(tx);
        let _ = stop_tx.send(());
        thread.join().unwrap();
        std::fs::remove_file(dir.join("test.redb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
