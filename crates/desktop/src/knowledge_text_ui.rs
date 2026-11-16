use eframe::egui;
#[derive(Default)]
pub(crate) struct State {
    pub forms: std::collections::HashMap<String, Form>,
    pub active: Option<String>,
    pub epoch: u64,
    pub put_pending: std::collections::HashSet<String>,
    pub search_request: u64,
    pub source_request: u64,
}
impl State {
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
}
pub(crate) struct Form {
    id: String,
    version: String,
    source: String,
    text: String,
    query: String,
    pub results: Vec<serde_json::Value>,
    pub result_query: String,
    pub offset: usize,
    pub loaded: bool,
    pub error: Option<String>,
    pub source_document: Option<serde_json::Value>,
}
impl Default for Form {
    fn default() -> Self {
        Self {
            id: String::new(),
            version: "1".into(),
            source: String::new(),
            text: String::new(),
            query: String::new(),
            results: vec![],
            result_query: String::new(),
            offset: 0,
            loaded: false,
            error: None,
            source_document: None,
        }
    }
}
pub(crate) enum Action {
    Put(Result<serde_json::Value, String>),
    Search(String, usize),
    Source(String),
}
pub(crate) fn render(ui: &mut egui::Ui, state: &mut State, project_id: &str) -> Option<Action> {
    let form = state.forms.entry(project_id.into()).or_default();
    ui.heading("지식 문서");
    ui.label("문서 ID");
    ui.text_edit_singleline(&mut form.id);
    ui.label("버전");
    ui.text_edit_singleline(&mut form.version);
    ui.label("출처");
    ui.text_edit_singleline(&mut form.source);
    ui.label("원문");
    ui.text_edit_multiline(&mut form.text);
    let save = ui
        .add_enabled(
            !state.put_pending.contains(project_id),
            egui::Button::new("원문 저장"),
        )
        .clicked();
    ui.label("검색어");
    ui.text_edit_singleline(&mut form.query);
    let search = ui
        .add_enabled(
            !form.query.trim().is_empty(),
            egui::Button::new("텍스트 검색"),
        )
        .clicked();
    if let Some(error) = &form.error {
        ui.label(error);
    }
    if let Some(doc) = &form.source_document {
        ui.label(format!(
            "원문: {} / 버전 {}",
            doc["id"].as_str().unwrap_or(""),
            doc["version"]
        ));
        ui.label(doc["source"].as_str().unwrap_or(""));
        ui.label(doc["text"].as_str().unwrap_or(""));
    }
    let mut page = None;
    let mut source_id = None;
    if form.loaded {
        ui.label(format!("검색 결과: {}", form.result_query));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(form.offset > 0, egui::Button::new("이전 검색 페이지"))
                .clicked()
            {
                page = Some(form.offset.saturating_sub(20));
            }
            if ui
                .add_enabled(
                    form.results.len() == 20 && form.offset.checked_add(20).is_some(),
                    egui::Button::new("다음 검색 페이지"),
                )
                .clicked()
            {
                page = form.offset.checked_add(20);
            }
        });
        if form.results.is_empty() {
            ui.label("검색 결과가 없습니다.");
        }
        for doc in &form.results {
            if let Some(id) = doc["id"].as_str() {
                ui.push_id(id, |ui| {
                    ui.label(format!("{}  {}", id, doc["source"].as_str().unwrap_or("")));
                    if ui.button("원문 보기").clicked() {
                        source_id = Some(id.to_owned());
                    }
                });
            }
        }
    }
    if save {
        let version = form.version.parse::<u64>().ok().filter(|v| *v > 0);
        if form.id.trim().is_empty() || form.source.trim().is_empty() || version.is_none() {
            return Some(Action::Put(Err(
                "문서 ID와 출처, 양의 버전을 입력하세요.".into()
            )));
        }
        return Some(Action::Put(Ok(
            serde_json::json!({"id":form.id,"version":version.unwrap(),"source":form.source,"text":form.text}),
        )));
    }
    if search {
        return Some(Action::Search(form.query.clone(), 0));
    }
    if let Some(offset) = page {
        return Some(Action::Search(form.result_query.clone(), offset));
    }
    if let Some(id) = source_id {
        return Some(Action::Source(id));
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    fn project(id: &str) -> domain::Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap()
    }
    #[test]
    fn save_sends_explicit_document_without_changing_multiline_text() {
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
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                id: "doc".into(),
                version: "2".into(),
                source: " 출처 ".into(),
                text: "첫 줄\n  둘째 줄  ".into(),
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 5000.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "원문 저장" => {
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
        let sent = work.try_recv().unwrap();
        assert!(
            matches!(sent.command,domain::Command::PutKnowledge{project_id,document} if project_id=="a" && document["id"]=="doc" && document["version"]==2 && document["source"]==" 출처 " && document["text"]=="첫 줄\n  둘째 줄  ")
        );
        app.apply(crate::worker::Reply {
            kind: sent.kind,
            result: Err("오류".into()),
        });
        assert!(app.send(crate::worker::Work::new(
            crate::worker::Kind::List,
            domain::Command::ListProjects
        )));
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
        assert!(app.state.status.contains("가득"));
        assert_eq!(app.state.knowledge.forms["a"].text, "첫 줄\n  둘째 줄  ");
        assert!(!app.state.knowledge.put_pending.contains("a"));
    }
    #[test]
    fn failed_save_preserves_input_and_reports_server_error() {
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
        app.state.knowledge.select("a");
        let epoch = app.state.knowledge.epoch;
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                id: "doc".into(),
                version: "2".into(),
                source: "source".into(),
                text: "내용\n두 줄".into(),
                ..Default::default()
            },
        );
        app.state.knowledge.put_pending.insert("a".into());
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::KnowledgePut {
                project_id: "a".into(),
                epoch,
            },
            result: Err("version conflict".into()),
        });
        assert!(!app.state.knowledge.put_pending.contains("a"));
        assert_eq!(app.state.knowledge.forms["a"].text, "내용\n두 줄");
        assert!(app.state.status.contains("version conflict"));
    }
    #[test]
    fn text_search_sends_query_for_current_project() {
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
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                query: "검색어".into(),
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 5000.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "텍스트 검색" => {
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
            matches!(work.try_recv().unwrap().command,domain::Command::SearchKnowledgePage{project_id,query,offset:0,limit:20} if project_id=="a" && query=="검색어")
        );
    }
    #[test]
    fn next_search_page_uses_submitted_query() {
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
        let results = (0..20)
            .map(|n| serde_json::json!({"id":format!("d{n}"),"source":"s","version":1,"text":"t"}))
            .collect();
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                query: "새 입력".into(),
                result_query: "이전 검색".into(),
                results,
                loaded: true,
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 5000.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "다음 검색 페이지" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("페이지 버튼");
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
            matches!(work.try_recv().unwrap().command,domain::Command::SearchKnowledgePage{query,offset:20,limit:20,..} if query=="이전 검색")
        );
    }
    #[test]
    fn selected_search_hit_requests_its_original_source() {
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
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                result_query: "q".into(),
                results: vec![
                    serde_json::json!({"id":"doc-id","source":"출처","version":2,"text":"원문"}),
                ],
                loaded: true,
                source_document: Some(
                    serde_json::json!({"id":"old","version":1,"source":"기존","text":"old"}),
                ),
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 5000.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "원문 보기" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("원문 보기");
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
            matches!(work.try_recv().unwrap().command,domain::Command::GetKnowledgeSource{project_id,document_id} if project_id=="a" && document_id=="doc-id")
        );
        assert!(app.state.knowledge.forms["a"].source_document.is_none());
    }
    #[test]
    fn project_round_trip_preserves_input_and_ignores_old_results() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.select("a");
        app.state.knowledge.forms.insert(
            "a".into(),
            Form {
                text: "남길 편집\n두 줄".into(),
                results: vec![
                    serde_json::json!({"id":"current","version":1,"source":"s","text":"t"}),
                ],
                loaded: true,
                ..Default::default()
            },
        );
        let epoch = app.state.knowledge.epoch;
        let request = app.state.knowledge.search_request;
        app.state.select("b");
        app.state.select("a");
        app.state.project = Some(project("a"));
        app.apply(crate::worker::Reply{kind:crate::worker::Kind::KnowledgeSearch{project_id:"a".into(),epoch,request,query:"old".into(),offset:0},result:Ok(domain::ResponseData::Data(serde_json::json!({"mode":"text","results":[{"id":"old","version":1,"source":"s","text":"t"}]})))});
        assert_eq!(app.state.knowledge.forms["a"].text, "남길 편집\n두 줄");
        assert_eq!(app.state.knowledge.forms["a"].results[0]["id"], "current");
    }
}
