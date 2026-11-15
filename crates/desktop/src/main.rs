#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use domain::Project;
use domain::{Command, ResponseData};
use eframe::egui;
use std::sync::mpsc::{Receiver, SyncSender};
mod storage;
mod worker;

#[derive(Default)]
struct State {
    selected: Option<String>,
    project: Option<Project>,
    generation: u64,
    name: String,
    path: String,
    status: String,
    projects: Vec<Project>,
    creating: bool,
    starting: bool,
    status_pending: bool,
    storage: storage::Storage,
    close_check: bool,
    close_allowed: bool,
    close_confirmation: bool,
    interview_inputs: std::collections::HashMap<String, String>,
    interview_pending: std::collections::HashSet<String>,
}

impl State {
    fn select(&mut self, id: &str) -> u64 {
        self.selected = Some(id.to_owned());
        self.project = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn receive_project(&mut self, generation: u64, project: Project) {
        if self.generation == generation && self.selected.as_deref() == Some(&project.id) {
            self.project = Some(project);
        }
    }
    fn created(&mut self, name: &str, path: &str, result: Result<Project, String>) {
        match result {
            Ok(project) => {
                if self.name == name && self.path == path {
                    self.name.clear();
                    self.path.clear();
                }
                if !self.projects.iter().any(|p| p.id == project.id) {
                    self.projects.push(project);
                }
                self.status = "프로젝트 생성 완료".into();
            }
            Err(error) => self.status = format!("오류: {error}"),
        }
    }
}

struct App {
    state: State,
    tx: SyncSender<worker::Work>,
    rx: Receiver<worker::Reply>,
    cancel_startup: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl App {
    fn poll_storage(&mut self) {
        if self.state.storage.pending_poll {
            self.state.storage.poll_again = true;
            return;
        }
        if self.send(worker::Work::new(
            worker::Kind::Storage,
            Command::ListStorageFailures,
        )) {
            self.state.storage.pending_poll = true;
            self.state.storage.last_poll = std::time::Instant::now();
        } else {
            self.state.storage.known = false;
            self.state.storage.error =
                Some("저장 상태 요청을 보내지 못했습니다. 다시 확인해 주세요.".into());
            self.state.close_check = false;
        }
    }

    fn retry_storage(&mut self, run_id: &str) {
        if self.state.storage.retries.contains(run_id) {
            return;
        }
        if self.send(worker::Work::new(
            worker::Kind::RetrySave {
                run_id: run_id.to_owned(),
            },
            Command::RetryCompletionSave {
                run_id: run_id.to_owned(),
            },
        )) {
            self.state.storage.notice = None;
            self.state.storage.retries.insert(run_id.to_owned());
            self.state.storage.protected.insert(run_id.to_owned());
        }
    }

    fn send(&mut self, work: worker::Work) -> bool {
        let status = matches!(&work.kind, worker::Kind::Status);
        if status && self.state.status_pending {
            return false;
        }
        if status {
            self.cancel_startup
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if self.tx.try_send(work).is_err() {
            self.state.status = "요청 대기열이 가득 찼거나 엔진 연결이 종료됐습니다".into();
            return false;
        }
        if status {
            self.state.status_pending = true;
        }
        true
    }

    fn apply(&mut self, reply: worker::Reply) {
        use worker::Kind;
        let is_status = matches!(&reply.kind, Kind::Status);
        match reply.kind {
            Kind::Storage => {
                self.state.storage.pending_poll = false;
                let snapshot = match reply.result {
                    Ok(ResponseData::Data(value)) => serde_json::from_value::<storage::Snapshot>(value).map_err(|_| "저장 진단 응답을 읽을 수 없습니다".to_owned()).and_then(|snapshot| if snapshot.valid() { Ok(snapshot) } else { Err("저장 진단 상태가 올바르지 않습니다. 엔진을 확인하고 다시 시도하세요.".into()) }),
                    Ok(_) => Err("저장 진단 응답을 읽을 수 없습니다".into()),
                    Err(error) if error.contains("requested feature is not available") => Err("현재 엔진은 저장 복구 진단을 지원하지 않습니다. 엔진을 업데이트한 뒤 다시 확인하세요.".into()),
                    Err(error) => Err(format!("저장 진단에 연결하지 못했습니다: {error}. 엔진 연결을 확인하고 다시 시도하세요.")),
                };
                match snapshot {
                    Ok(mut snapshot) => {
                        if let Some(previous) = &self.state.storage.snapshot {
                            for failure in &previous.failures {
                                if self.state.storage.protected.contains(&failure.run_id)
                                    && (self.state.storage.poll_again
                                        || self.state.storage.retries.contains(&failure.run_id))
                                    && !snapshot
                                        .failures
                                        .iter()
                                        .any(|item| item.run_id == failure.run_id)
                                {
                                    snapshot.failures.push(failure.clone());
                                }
                            }
                        }
                        self.state.storage.snapshot = Some(snapshot);
                        self.state.storage.known = true;
                        self.state.storage.error = None;
                        if !self.state.storage.poll_again {
                            self.state
                                .storage
                                .protected
                                .retain(|run| self.state.storage.retries.contains(run));
                        }
                    }
                    Err(error) => {
                        self.state.storage.known = false;
                        self.state.storage.error = Some(error);
                    }
                }
                if std::mem::take(&mut self.state.storage.poll_again) {
                    self.poll_storage();
                } else if self.state.close_check {
                    self.state.close_check = false;
                    if self.state.storage.may_close() {
                        if self.state.storage.has_pending_result() {
                            self.state.close_confirmation = true;
                        } else {
                            self.state.close_allowed = true;
                        }
                    } else {
                        self.state.status = "완료 저장 상태를 확인한 뒤 다시 닫아 주세요".into();
                    }
                }
            }
            Kind::RetrySave { run_id } => {
                self.state.storage.retries.remove(&run_id);
                let storage_only = self
                    .state
                    .storage
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.failures.iter().find(|f| f.run_id == run_id))
                    .is_some_and(|f| matches!(f.stage.as_str(), "unknown" | "startup"));
                match reply.result {
                    Ok(ResponseData::Data(value)) if value["state"] == "saved" => self.state.storage.notice = Some(if storage_only { "저장소 복구를 완료했습니다.".into() } else { format!("실행 {run_id}: 완료 결과 저장을 복구했습니다.") }),
                    Ok(ResponseData::Data(value)) if value["state"] == "superseded" => self.state.storage.notice = Some(format!("실행 {run_id}: 새 상태가 먼저 확정되어 이전 결과를 적용하지 않았습니다. 현재 상태를 확인하세요.")),
                    Ok(ResponseData::Data(value)) if value["state"] == "pending" || value["state"] == "cleanup_pending" => {},
                    Ok(_) => self.state.storage.notice = Some(format!("실행 {run_id}: 저장 결과를 확인할 수 없습니다.")),
                    Err(error) => self.state.storage.notice = Some(format!("실행 {run_id}: 저장 재시도 실패: {error}")),
                }
                self.poll_storage();
            }
            Kind::Starting => {
                self.state.starting = true;
                self.state.status = "엔진 시작 대기 중".into();
            }
            Kind::Status => match reply.result {
                Ok(ResponseData::Status { message }) => {
                    self.state.status = if message
                        == "engine ready; project storage only; workflow and models unavailable"
                    {
                        "엔진 연결됨 — 프로젝트 저장 기능 사용 가능".into()
                    } else {
                        message
                    }
                }
                Ok(_) => self.state.status = "오류: 엔진 상태 응답이 올바르지 않습니다".into(),
                Err(e) => self.state.status = format!("오류: {e}"),
            },
            Kind::List => match reply.result {
                Ok(ResponseData::Projects(projects)) => self.state.projects = projects,
                Ok(_) => self.state.status = "오류: 프로젝트 목록 응답이 올바르지 않습니다".into(),
                Err(e) if !self.state.status.starts_with("오류:") => {
                    self.state.status = format!("오류: {e}")
                }
                Err(_) => {}
            },
            Kind::Get { generation } => match reply.result {
                Ok(ResponseData::Project(project)) => {
                    self.state.receive_project(generation, project)
                }
                Err(e) if generation == self.state.generation => {
                    self.state.status = format!("오류: {e}")
                }
                Ok(_) if generation == self.state.generation => {
                    self.state.status = "오류: 프로젝트 응답이 올바르지 않습니다".into()
                }
                _ => {}
            },
            Kind::Interview {
                project_id,
                message,
            } => {
                self.state.interview_pending.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Project(project)) if project.id == project_id => {
                        if self.state.interview_inputs.get(&project_id) == Some(&message) {
                            self.state.interview_inputs.remove(&project_id);
                        }
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.generation = self.state.generation.wrapping_add(1);
                            self.state.project = Some(project);
                            self.state.status =
                                "인터뷰 답변을 저장했습니다. 기존 명세 승인이 해제되었습니다."
                                    .into();
                        }
                    }
                    Ok(_) => self.state.status = "오류: 인터뷰 응답이 올바르지 않습니다".into(),
                    Err(e) => self.state.status = format!("오류: {e}"),
                }
            }
            Kind::Create { name, path } => {
                self.state.creating = false;
                let result = match reply.result {
                    Ok(ResponseData::Project(project)) => Ok(project),
                    Ok(_) => Err("프로젝트 생성 응답이 올바르지 않습니다".into()),
                    Err(e) => Err(e),
                };
                self.state.created(&name, &path, result);
            }
        }
        if is_status {
            self.state.starting = false;
            self.state.status_pending = false;
        }
    }

    fn render(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.rx.try_recv() {
            self.apply(reply);
        }
        if self.state.close_allowed && !self.state.storage.may_close() {
            self.state.close_allowed = false;
        }
        if self.state.close_confirmation && !self.state.storage.may_close() {
            self.state.close_confirmation = false;
        }
        if self.state.close_allowed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.state.close_allowed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.status = "완료 저장 상태를 확인한 뒤 다시 닫아 주세요".into();
            self.state.close_check = true;
            self.poll_storage();
        }
        if !self.state.close_allowed
            && self.state.storage.last_poll.elapsed() >= std::time::Duration::from_secs(2)
            && !self.state.storage.pending_poll
        {
            self.poll_storage();
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MAGI Desktop");
                ui.separator();
                ui.label(&self.state.status);
                if self.state.starting
                    && ui
                        .button("시작 대기 취소")
                        .on_hover_text("대기만 중단하며 이미 시작된 엔진은 종료하지 않습니다.")
                        .clicked()
                {
                    self.cancel_startup
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.state.status = "엔진 시작 대기 취소 중".into();
                }
            });
        });
        self.storage_panel(ctx);
        if self.state.close_confirmation {
            egui::Window::new("완료 저장 대기")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("완료 결과가 저장 대기 중입니다. 복구 기록은 보존되며 엔진은 계속 실행됩니다.");
                    ui.horizontal(|ui| {
                        if ui.button("창 유지").clicked() {
                            self.state.close_confirmation = false;
                        }
                        if ui.button("창만 닫기").clicked() && self.state.storage.may_close() {
                            self.state.close_confirmation = false;
                            self.state.close_allowed = true;
                        }
                    });
                });
        }
        egui::SidePanel::left("sidebar")
            .default_width(230.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("프로젝트");
                    if self.state.status.starts_with("오류:") {
                        ui.label("엔진이 실행 중인지 확인한 뒤 새로고침하세요.");
                    }
                    if ui.button("새로고침").clicked() {
                        self.send(worker::Work::new(worker::Kind::Status, Command::Status));
                        self.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
                    }
                    for project in self.state.projects.clone() {
                        if ui
                            .selectable_label(
                                self.state.selected.as_deref() == Some(&project.id),
                                &project.name,
                            )
                            .clicked()
                        {
                            let generation = self.state.select(&project.id);
                            self.send(worker::Work::new(
                                worker::Kind::Get { generation },
                                Command::GetProject {
                                    project_id: project.id,
                                },
                            ));
                        }
                    }
                    ui.separator();
                    ui.label("새 프로젝트");
                    ui.text_edit_singleline(&mut self.state.name);
                    ui.text_edit_singleline(&mut self.state.path);
                    if ui
                        .add_enabled(
                            !self.state.creating
                                && !self.state.name.trim().is_empty()
                                && !self.state.path.trim().is_empty(),
                            egui::Button::new("생성"),
                        )
                        .clicked()
                    {
                        let name = self.state.name.clone();
                        let path = self.state.path.clone();
                        if self.send(worker::Work::new(
                            worker::Kind::Create {
                                name: name.clone(),
                                path: path.clone(),
                            },
                            Command::CreateProject { name, path },
                        )) {
                            self.state.creating = true;
                            self.state.status = "프로젝트 생성 중".into();
                        }
                    }
                });
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(project) = self.state.project.clone() {
                ui.heading(&project.name);
                ui.label(&project.path);
                ui.heading("인터뷰");
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for message in &project.interview {
                            ui.label(message);
                        }
                    });
                let input = self
                    .state
                    .interview_inputs
                    .entry(project.id.clone())
                    .or_default();
                ui.add(egui::TextEdit::multiline(input).desired_rows(4));
                let message = input.clone();
                if ui
                    .add_enabled(
                        !message.trim().is_empty()
                            && !self.state.interview_pending.contains(&project.id),
                        egui::Button::new("응답 저장"),
                    )
                    .clicked()
                {
                    let id = project.id.clone();
                    if self.send(worker::Work::new(
                        worker::Kind::Interview {
                            project_id: id.clone(),
                            message: message.clone(),
                        },
                        Command::SubmitInterview {
                            project_id: id.clone(),
                            message,
                        },
                    )) {
                        self.state.interview_pending.insert(id);
                        self.state.generation = self.state.generation.wrapping_add(1);
                    }
                }
                ui.label("답변을 저장하면 기존 명세 승인이 해제됩니다.");
            } else {
                ui.label("프로젝트를 선택하세요.");
            }
            ui.separator();
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_startup
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "MAGI Desktop",
        eframe::NativeOptions::default(),
        Box::new(|cc| {
            let font_ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                if let Some(path) = std::env::var_os("WINDIR").map(|dir| {
                    std::path::PathBuf::from(dir)
                        .join("Fonts")
                        .join("malgun.ttf")
                }) {
                    if let Ok(bytes) = std::fs::read(path) {
                        let mut fonts = egui::FontDefinitions::default();
                        fonts
                            .font_data
                            .insert("korean".into(), egui::FontData::from_owned(bytes).into());
                        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                        {
                            if let Some(fonts) = fonts.families.get_mut(&family) {
                                fonts.push("korean".into());
                            }
                        }
                        font_ctx.set_fonts(fonts);
                        font_ctx.request_repaint();
                    }
                }
            });
            let (tx, rx, cancel_startup) = worker::start(cc.egui_ctx.clone());
            let mut app = App {
                state: State {
                    status: "엔진 연결 중".into(),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup,
            };
            app.send(worker::Work::new(worker::Kind::Status, Command::Status));
            app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
            app.poll_storage();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str) -> Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"C:/work","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap()
    }

    #[test]
    fn delayed_project_response_does_not_replace_new_selection() {
        let mut state = State::default();
        let old = state.select("a");
        let new = state.select("b");
        state.receive_project(new, project("b"));
        state.receive_project(old, project("a"));
        assert_eq!(state.project.unwrap().id, "b");
    }

    #[test]
    fn create_reply_preserves_newly_edited_input() {
        let mut state = State {
            name: "다음 이름".into(),
            path: "다음 경로".into(),
            ..Default::default()
        };
        state.created("이전 이름", "이전 경로", Ok(project("new")));
        assert_eq!(state.name, "다음 이름");
        assert_eq!(state.path, "다음 경로");
    }

    #[test]
    fn creation_failure_preserves_inputs_and_reports_reason() {
        let mut state = State {
            name: "이름".into(),
            path: "경로".into(),
            ..Default::default()
        };
        state.created("이름", "경로", Err("저장 실패".into()));
        assert_eq!(state.name, "이름");
        assert_eq!(state.path, "경로");
        assert!(state.status.contains("저장 실패"));
    }

    #[test]
    fn queue_full_does_not_clear_input_or_start_creation() {
        let (tx, _work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                name: "이름".into(),
                path: "경로".into(),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        assert!(app.send(worker::Work::new(worker::Kind::Status, Command::Status)));
        assert!(!app.send(worker::Work::new(worker::Kind::List, Command::ListProjects)));
        assert_eq!(app.state.name, "이름");
        assert!(!app.state.creating);
        assert!(app.state.status.contains("가득"));
    }

    #[test]
    fn startup_error_is_not_hidden_by_the_following_list_failure() {
        let (tx, _work) = std::sync::mpsc::sync_channel(2);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Status,
            result: Err("데이터베이스를 열 수 없음".into()),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::List,
            result: Err("연결 실패".into()),
        });
        assert!(app.state.status.contains("데이터베이스를 열 수 없음"));
    }

    #[test]
    fn unknown_storage_status_cancels_window_close() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
    }

    #[test]
    fn unsupported_storage_and_memory_only_results_block_close() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: Err("requested feature is not available".into()),
        });
        assert!(!app.state.storage.known);
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(app
            .state
            .storage
            .error
            .as_deref()
            .unwrap()
            .contains("지원하지 않습니다"));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("지원하지 않습니다"))));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":1,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
    }

    #[test]
    fn journaled_pending_requires_confirmation_before_close() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"worktree","state":"pending","attempts":1,"journaled":true,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close)),
            "저장 대기를 확인하기 전에 창이 닫혔습니다"
        );
        fn click(ctx: &egui::Context, app: &mut App, label: &str) {
            let input = || egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input(), |ctx| app.render(ctx));
            let output = ctx.run(input(), |ctx| app.render(ctx));
            let pos = output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::epaint::Shape::Text(text) if text.galley.job.text == label => {
                        Some(text.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .expect("선택 버튼이 보여야 합니다");
            let mut down = input();
            down.events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = ctx.run(down, |ctx| app.render(ctx));
            let mut up = input();
            up.events = vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }];
            let _ = ctx.run(up, |ctx| app.render(ctx));
        }
        click(&ctx, &mut app, "창 유지");
        assert!(!app.state.close_allowed && !app.state.close_confirmation);
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"worktree","state":"retrying","attempts":1,"journaled":true,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        click(&ctx, &mut app, "창만 닫기");
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn close_confirmation_cannot_bypass_new_unresolved_storage_state() {
        for (stage, journaled, local_retry) in [
            ("agent", false, false),
            ("unknown", true, false),
            ("agent", true, true),
        ] {
            let (tx, _requests) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.state.close_confirmation = true;
            if local_retry {
                app.state.storage.retries.insert("r".into());
            }
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
                "failures":[{"run_id":"r","project_id":"p","stage":stage,"state":"pending","attempts":1,"journaled":journaled,"message":"대기"}],
                "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
            }))) });
            let ctx = egui::Context::default();
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(!app.state.close_confirmation && !app.state.close_allowed);
            assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close)));
        }
    }

    #[test]
    fn closing_checks_fresh_storage_state_before_allowing_exit() {
        let (tx, requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
        assert!(requests
            .try_iter()
            .any(|work| matches!(work.command, Command::ListStorageFailures)));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn close_waits_for_new_poll_when_older_poll_was_in_flight() {
        let (tx, requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let empty = || {
            Ok(ResponseData::Data(
                serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}),
            ))
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: empty(),
        });
        app.poll_storage();
        assert!(matches!(
            requests.try_recv().unwrap().command,
            Command::ListStorageFailures
        ));
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: empty(),
        });
        assert!(matches!(
            requests.try_recv().unwrap().command,
            Command::ListStorageFailures
        ));
        assert!(!app.state.close_allowed);
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":2,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn storage_warning_and_retry_are_visible_across_project_switches() {
        let (tx, requests) = std::sync::mpsc::sync_channel(16);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"run-7","project_id":"project-a","stage":"agent","state":"pending","attempts":3,"journaled":false,"message":"저장 대기"}],
            "log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]
        }))) });
        app.state.select("project-b");
        let ctx = egui::Context::default();
        let mut frame = |events| {
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
        frame(vec![]);
        let output = frame(vec![]);
        let visible = |text: &str| {
            output.shapes.iter().find_map(|clipped| {
                if let egui::epaint::Shape::Text(shape) = &clipped.shape {
                    let center = shape.visual_bounding_rect().center();
                    if shape.galley.job.text.contains(text) && clipped.clip_rect.contains(center) {
                        Some(center)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        };
        assert!(visible("아직 저장되지 않음").is_some());
        assert!(visible("run-7").is_some());
        assert!(visible("C:/temp/events.log").is_some());
        assert!(visible("진단 로그를 파일에 기록할 수 없습니다").is_some());
        let retry = visible("완료 저장 재시도").unwrap();
        frame(vec![
            egui::Event::PointerMoved(retry),
            egui::Event::PointerButton {
                pos: retry,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerButton {
            pos: retry,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(vec![]);
        frame(vec![
            egui::Event::PointerMoved(retry),
            egui::Event::PointerButton {
                pos: retry,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerButton {
            pos: retry,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let commands: Vec<_> = requests.try_iter().map(|work| work.command).collect();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(&commands[0], Command::RetryCompletionSave { run_id } if run_id == "run-7")
        );
        drop(frame);
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]}))) });
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장 재시도 중"))), "미확정 재시도 경고는 이전 진단으로 사라지면 안 됨");
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "run-7".into(),
            },
            result: Ok(ResponseData::Data(serde_json::json!({"state":"saved"}))),
        });
        assert!(requests
            .try_iter()
            .any(|work| matches!(work.command, Command::ListStorageFailures)));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]}))) });
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("완료 결과 저장을 복구했습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("아직 저장되지 않음"))));
    }

    #[test]
    fn superseded_retry_does_not_claim_saved() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "run-old".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"state":"superseded"}),
            )),
        });
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("이전 결과를 적용하지 않았습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장을 복구했습니다"))));
    }

    #[test]
    fn startup_recovery_does_not_claim_a_computed_run_was_saved() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"diagnostic","project_id":"","stage":"startup","state":"pending","attempts":1,"journaled":false,"message":"저장소 초기화 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "diagnostic".into(),
            },
            result: Ok(ResponseData::Data(serde_json::json!({"state":"saved"}))),
        });
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장소 복구를 완료했습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("완료 결과 저장을 복구했습니다"))));
    }

    #[test]
    fn storage_stage_and_logging_states_are_rendered_distinctly() {
        for (stage, state, label) in [
            ("agent", "retrying", "저장 재시도 중"),
            ("worktree", "cleanup_pending", "복구 기록 정리 대기"),
            (
                "unknown",
                "pending",
                "완료 복구 기록 또는 저장소를 확인할 수 없음",
            ),
            (
                "startup",
                "pending",
                "완료 복구 기록 또는 저장소를 확인할 수 없음",
            ),
        ] {
            let (tx, _work) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
                "failures":[{"run_id":"r","project_id":"p","stage":stage,"state":state,"attempts":1,"journaled":true,"message":"진단"}],
                "log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,
                "log_entries":[{"timestamp_ms":1,"run_id":"r","stage":stage,"event":"storage_pending"}]
            }))) });
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains(label))), "{stage} {state}");
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("storage_pending"))));
            if matches!(stage, "unknown" | "startup") {
                assert!(!app.state.storage.may_close());
            }
        }
    }

    #[test]
    fn older_diagnostic_after_pending_retry_cannot_remove_warning() {
        for failed in [false, true] {
            let (tx, requests) = std::sync::mpsc::sync_channel(16);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":1,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
            app.poll_storage();
            app.retry_storage("r");
            requests.try_iter().for_each(drop);
            let result = if failed {
                Err("저장 실패".into())
            } else {
                Ok(ResponseData::Data(serde_json::json!({"state":"pending"})))
            };
            app.apply(worker::Reply {
                kind: worker::Kind::RetrySave { run_id: "r".into() },
                result,
            });
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("아직 저장되지 않음"))));
            assert!(!app.state.storage.may_close());
            assert!(requests
                .try_iter()
                .any(|work| matches!(work.command, Command::ListStorageFailures)));
        }
    }

    #[test]
    fn startup_progress_and_cancellation_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(2);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Starting,
            result: Ok(ResponseData::Status {
                message: "엔진 시작 대기 중".into(),
            }),
        });
        assert!(app.state.starting);
        app.apply(worker::Reply {
            kind: worker::Kind::Status,
            result: Err("엔진 시작 대기가 취소됐습니다".into()),
        });
        assert!(!app.state.starting);
        assert!(app.state.status.contains("취소"));
    }

    #[test]
    fn selected_project_interview_is_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.interview.push("기존 인터뷰 답변".into());
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == "기존 인터뷰 답변")));
    }

    #[test]
    fn interview_input_and_submit_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == "응답 저장")));
    }

    #[test]
    fn interview_response_preserves_new_input_and_other_selection() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state
            .interview_inputs
            .insert("a".into(), "새 답변".into());
        app.state.interview_pending.insert("a".into());
        let generation = app.state.select("b");
        app.state.receive_project(generation, project("b"));
        let mut updated = project("a");
        updated.interview.push("이전 답변".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "이전 답변".into(),
            },
            result: Ok(ResponseData::Project(updated)),
        });
        assert_eq!(app.state.project.as_ref().unwrap().id, "b");
        assert_eq!(app.state.interview_inputs["a"], "새 답변");
        assert!(!app.state.interview_pending.contains("a"));
    }

    #[test]
    fn interview_failure_retains_input_and_success_invalidates_old_fetch() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let generation = app.state.select("a");
        app.state.interview_inputs.insert("a".into(), "답변".into());
        app.state.interview_pending.insert("a".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "답변".into(),
            },
            result: Err("저장 실패".into()),
        });
        assert_eq!(app.state.interview_inputs["a"], "답변");
        assert!(app.state.status.contains("저장 실패"));
        let mut updated = project("a");
        updated.interview.push("답변".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "답변".into(),
            },
            result: Ok(ResponseData::Project(updated)),
        });
        app.state.receive_project(generation, project("a"));
        assert_eq!(app.state.project.as_ref().unwrap().interview, ["답변"]);
        assert!(!app.state.interview_inputs.contains_key("a"));
        assert!(app.state.status.contains("승인"));
    }

    #[test]
    fn interview_button_sends_selected_project_and_preserves_input_when_queue_full() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.interview_inputs.insert("a".into(), "답변".into());
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let output = frame(&mut app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.job.text == "응답 저장" => {
                    Some(text.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        let click = |app: &mut App| {
            frame(
                app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        };
        click(&mut app);
        assert!(
            matches!(work.try_recv().unwrap().command, Command::SubmitInterview { project_id, message } if project_id == "a" && message == "답변")
        );
        app.state.interview_pending.clear();
        app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
        click(&mut app);
        assert_eq!(app.state.interview_inputs["a"], "답변");
        assert!(!app.state.interview_pending.contains("a"));
        assert!(app.state.status.contains("가득"));
    }

    #[test]
    fn shell_renders_headlessly_at_multiple_scales() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        for scale in [1.0, 2.0, 3.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        }
    }

    #[test]
    fn long_project_list_allows_selecting_last_and_creating_project() {
        for size in [egui::vec2(800.0, 600.0), egui::vec2(320.0, 240.0)] {
            let (tx, work) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
            let mut app = App {
                state: State {
                    projects: (0..40).map(|i| project(&format!("project-{i}"))).collect(),
                    name: "new".into(),
                    path: "C:/work".into(),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":false,"log_entries":[]}))) });
            let ctx = egui::Context::default();
            let mut time = 0.0;
            let mut frame = |events: Vec<egui::Event>, app: &mut App| {
                time += 0.1;
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ctx| app.render(ctx),
                )
            };
            frame(
                vec![egui::Event::PointerMoved(egui::pos2(80.0, 100.0))],
                &mut app,
            );
            let mut output = frame(
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -3000.0),
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            for _ in 0..30 {
                output = frame(vec![], &mut app);
            }
            let visible = |output: &egui::FullOutput, label: &str| -> Option<egui::Pos2> {
                output.shapes.iter().find_map(|clipped| {
                    if let egui::epaint::Shape::Text(text) = &clipped.shape {
                        let center = text.visual_bounding_rect().center();
                        if text.galley.job.text == label
                            && clipped.clip_rect.contains(center)
                            && center.x >= 0.0
                            && center.x < size.x
                            && center.y >= 0.0
                            && center.y < size.y
                        {
                            return Some(center);
                        }
                    }
                    None
                })
            };
            let last =
                visible(&output, "project-39").expect("마지막 프로젝트가 보이고 선택 가능해야 함");
            frame(
                vec![
                    egui::Event::PointerMoved(last),
                    egui::Event::PointerButton {
                        pos: last,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            frame(
                vec![egui::Event::PointerButton {
                    pos: last,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            assert_eq!(app.state.selected.as_deref(), Some("project-39"));
            frame(
                vec![
                    egui::Event::PointerMoved(egui::pos2(80.0, 100.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -3000.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            for _ in 0..30 {
                output = frame(vec![], &mut app);
            }
            let create =
                visible(&output, "생성").expect("프로젝트 생성 버튼이 보이고 클릭 가능해야 함");
            frame(
                vec![
                    egui::Event::PointerMoved(create),
                    egui::Event::PointerButton {
                        pos: create,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            frame(
                vec![egui::Event::PointerButton {
                    pos: create,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            assert_eq!(app.state.selected.as_deref(), Some("project-39"));
            assert!(matches!(
                work.try_recv().unwrap().command,
                Command::GetProject { .. }
            ));
            assert!(matches!(
                work.try_recv().unwrap().command,
                Command::CreateProject { .. }
            ));
        }
    }
}
