use eframe::egui;
#[derive(Default)]
pub(crate) struct State {
    pub forms: std::collections::HashMap<String, Form>,
    pub active: Option<String>,
    pub epoch: u64,
    pub pending: std::collections::HashSet<String>,
    pub list_request: u64,
    pub refresh_needed: std::collections::HashSet<String>,
}
impl State {
    pub fn changed(&mut self, id: &str) {
        if let Some(form) = self.forms.get_mut(id) {
            form.records.clear();
            form.loaded = false;
        }
        if self.active.as_deref() == Some(id) {
            self.list_request = self.list_request.wrapping_add(1);
        }
    }
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
}
#[derive(Default)]
pub(crate) struct Form {
    text: String,
    id: String,
    version: String,
    pub records: Vec<serde_json::Value>,
    pub loaded: bool,
    pub error: Option<String>,
}
pub(crate) enum Action {
    Save(String),
    List,
    Revise(String, String),
    Select(String, u64),
    Refresh,
}
pub(crate) fn render(
    ui: &mut egui::Ui,
    state: &mut State,
    project: &domain::Project,
) -> Option<Action> {
    let project_id = project.id.as_str();
    let form = state.forms.entry(project_id.into()).or_default();
    ui.heading("프로젝트 공통 결정");
    let refresh =
        state.refresh_needed.contains(project_id) && ui.button("프로젝트 상태 재조회").clicked();
    ui.label("결정 내용");
    ui.text_edit_multiline(&mut form.text);
    ui.label("결정 ID");
    ui.text_edit_singleline(&mut form.id);
    ui.label("결정 버전");
    ui.text_edit_singleline(&mut form.version);
    let idle = !state.pending.contains(project_id);
    let valid_version = form.version.parse::<u64>().ok().filter(|v| *v > 0);
    let save = ui
        .add_enabled(
            idle && !form.text.trim().is_empty(),
            egui::Button::new("새 결정 저장"),
        )
        .clicked();
    let revise = ui
        .add_enabled(
            idle && !form.id.trim().is_empty() && !form.text.trim().is_empty(),
            egui::Button::new("결정 개정"),
        )
        .clicked();
    let list = ui.button("결정 목록 조회").clicked();
    let select = ui
        .add_enabled(
            idle && !form.id.trim().is_empty() && valid_version.is_some(),
            egui::Button::new("이 버전 선택"),
        )
        .clicked();
    if let Some(error) = &form.error {
        ui.label(error);
    }
    ui.label("선택 당시 사본");
    for raw in &project.draft.reused_decisions {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let (Some(id), Some(version), Some(text)) = (
                value["id"].as_str(),
                value["version"].as_u64(),
                value["text"].as_str(),
            ) {
                ui.label(format!("{id} v{version}"));
                ui.label(text);
            } else {
                ui.label(raw);
            }
        } else {
            ui.label(raw);
        }
    }
    ui.label("저장된 결정 버전");
    if form.loaded && form.records.is_empty() {
        ui.label("저장된 결정이 없습니다.");
    }
    let mut chosen = None;
    for item in &form.records {
        if let (Some(id), Some(version), Some(text)) = (
            item["id"].as_str(),
            item["version"].as_u64(),
            item["text"].as_str(),
        ) {
            ui.push_id((id, version), |ui| {
                ui.label(format!("결정 {id} v{version}"));
                ui.label(text);
                if ui.button("이 버전 지정").clicked() {
                    chosen = Some((id.to_owned(), version));
                }
            });
        }
    }
    if let Some((id, version)) = chosen {
        form.id = id;
        form.version = version.to_string();
    }
    if refresh {
        Some(Action::Refresh)
    } else if save {
        Some(Action::Save(form.text.clone()))
    } else if list {
        Some(Action::List)
    } else if revise {
        Some(Action::Revise(form.id.clone(), form.text.clone()))
    } else if select {
        Some(Action::Select(form.id.clone(), valid_version.unwrap()))
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn project(id: &str) -> domain::Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap()
    }
    fn click(app: &mut crate::App, label: &str) {
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 7000.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let out = frame(app, vec![]);
        let pos = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text == label => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
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
    }
    #[test]
    fn new_decision_sends_unchanged_multiline_text() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.forms.insert(
            "a".into(),
            Form {
                text: "첫 줄\n  둘째 줄".into(),
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 7000.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let out = frame(&mut app, vec![]);
        let pos = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text == "새 결정 저장" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        frame(
            &mut app,
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
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::SaveDecision{project_id,decision} if project_id=="a" && decision=="첫 줄\n  둘째 줄")
        );
    }
    #[test]
    fn list_requests_selected_project() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        click(&mut app, "결정 목록 조회");
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::ListDecisions{project_id} if project_id=="a")
        );
    }
    #[test]
    fn revise_and_select_send_explicit_identity_and_version() {
        for label in ["결정 개정", "이 버전 선택"] {
            let (tx, work) = std::sync::mpsc::sync_channel(8);
            let (_rt, rx) = std::sync::mpsc::sync_channel(1);
            let mut app = crate::App {
                state: crate::State {
                    selected: Some("a".into()),
                    project: Some(project("a")),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.state.decisions.forms.insert(
                "a".into(),
                Form {
                    id: "decision".into(),
                    version: "3".into(),
                    text: "개정\n내용".into(),
                    ..Default::default()
                },
            );
            click(&mut app, label);
            let command = work.try_recv().unwrap().command;
            if label == "결정 개정" {
                assert!(
                    matches!(command,domain::Command::ReviseDecision{project_id,id,text} if project_id=="a" && id=="decision" && text=="개정\n내용")
                );
            } else {
                assert!(
                    matches!(command,domain::Command::SelectDecision{project_id,id,version:3} if project_id=="a" && id=="decision")
                );
            }
        }
    }
    #[test]
    fn full_queue_allows_retrying_project_refresh_without_unblocking() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.select("a");
        let epoch = app.state.decisions.epoch;
        app.state.decisions.pending.insert("a".into());
        assert!(app.send(crate::worker::Work::new(
            crate::worker::Kind::List,
            domain::Command::ListProjects
        )));
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::DecisionSelect {
                project_id: "a".into(),
                epoch,
                id: "d".into(),
                version: 1,
            },
            result: Ok(domain::ResponseData::Data(
                serde_json::json!({"id":"d","version":1,"text":"선택"}),
            )),
        });
        assert!(app.state.decisions.pending.contains("a"));
        work.try_recv().unwrap();
        click(&mut app, "프로젝트 상태 재조회");
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::GetProject{project_id} if project_id=="a")
        );
        assert!(app.state.decisions.pending.contains("a"));
    }
    #[test]
    fn choosing_list_version_fills_identity_without_replacing_edit() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.forms.insert(
            "a".into(),
            Form {
                text: "편집 중\n내용".into(),
                records: vec![serde_json::json!({"id":"d","version":2,"text":"기존"})],
                loaded: true,
                ..Default::default()
            },
        );
        click(&mut app, "이 버전 지정");
        let form = &app.state.decisions.forms["a"];
        assert_eq!(form.id, "d");
        assert_eq!(form.version, "2");
        assert_eq!(form.text, "편집 중\n내용");
    }
    #[test]
    fn failed_mutation_preserves_edit_and_reconciles_project() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.select("a");
        let epoch = app.state.decisions.epoch;
        app.state.decisions.pending.insert("a".into());
        app.state.decisions.forms.insert(
            "a".into(),
            Form {
                text: "편집\n보존".into(),
                ..Default::default()
            },
        );
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::DecisionSave {
                project_id: "a".into(),
                epoch,
            },
            result: Err("server error".into()),
        });
        assert_eq!(app.state.decisions.forms["a"].text, "편집\n보존");
        assert!(app.state.decisions.forms["a"]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("server error"));
        assert!(app.state.decisions.pending.contains("a"));
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::GetProject{project_id} if project_id=="a")
        );
    }
    #[test]
    fn saved_new_decision_marks_previous_list_stale() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.select("a");
        let epoch = app.state.decisions.epoch;
        app.state.decisions.pending.insert("a".into());
        app.state.decisions.forms.insert(
            "a".into(),
            Form {
                loaded: true,
                records: vec![serde_json::json!({"id":"old","version":1,"text":"old"})],
                ..Default::default()
            },
        );
        let mut updated = project("a");
        updated.context_version = 1;
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::DecisionSave {
                project_id: "a".into(),
                epoch,
            },
            result: Ok(domain::ResponseData::Project(updated)),
        });
        assert!(!app.state.decisions.forms["a"].loaded);
    }
    #[test]
    fn full_queue_preserves_unsent_decision_edit() {
        let (tx, _work) = std::sync::mpsc::sync_channel(1);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.decisions.forms.insert(
            "a".into(),
            Form {
                text: "남겨둘\n편집".into(),
                ..Default::default()
            },
        );
        assert!(app.send(crate::worker::Work::new(
            crate::worker::Kind::List,
            domain::Command::ListProjects
        )));
        click(&mut app, "새 결정 저장");
        assert_eq!(app.state.decisions.forms["a"].text, "남겨둘\n편집");
        assert!(!app.state.decisions.pending.contains("a"));
        assert!(app.state.status.contains("가득"));
    }
}
