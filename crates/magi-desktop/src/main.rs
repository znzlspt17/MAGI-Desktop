#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use magi_core::Project;
use magi_core::{Command, ResponseData};
use std::sync::mpsc::{Receiver, SyncSender};
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
}

impl App {
    fn send(&mut self, work: worker::Work) -> bool {
        if self.tx.try_send(work).is_err() {
            self.state.status = "요청 대기열이 가득 찼거나 엔진 연결이 종료됐습니다".into();
            return false;
        }
        true
    }

    fn apply(&mut self, reply: worker::Reply) {
        use worker::Kind;
        match reply.kind {
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
    }

    fn render(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.rx.try_recv() {
            self.apply(reply);
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MAGI Desktop");
                ui.separator();
                ui.label(&self.state.status);
            });
        });
        egui::SidePanel::left("sidebar")
            .default_width(230.0)
            .show(ctx, |ui| {
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
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(project) = &self.state.project {
                ui.heading(&project.name);
                ui.label(&project.path);
            } else { ui.label("프로젝트를 선택하세요."); }
            ui.separator();
            ui.label("현재 프로젝트 목록과 생성 기능을 사용할 수 있습니다. 다른 작업 기능은 아직 제공되지 않습니다.");
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
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
            let (tx, rx) = worker::start(cc.egui_ctx.clone());
            let mut app = App {
                state: State {
                    status: "엔진 연결 중".into(),
                    ..Default::default()
                },
                tx,
                rx,
            };
            app.send(worker::Work::new(worker::Kind::Status, Command::Status));
            app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
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
    fn shell_renders_headlessly_at_multiple_scales() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
        };
        for scale in [1.0, 2.0, 3.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        }
    }
}
