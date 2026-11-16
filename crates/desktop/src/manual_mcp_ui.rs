use domain::{Command, Project};
pub(crate) struct Form {
    pub endpoint: String,
    pub name: String,
    pub arguments: String,
    pub cwd: String,
}
impl Default for Form {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            name: String::new(),
            arguments: "{}".into(),
            cwd: ".".into(),
        }
    }
}
#[derive(Default)]
pub(crate) struct State {
    pub forms: std::collections::HashMap<String, Form>,
    pub pending: std::collections::HashSet<String>,
    pub binding: Option<Binding>,
}
pub(crate) struct Binding {
    pub project: String,
    pub run: String,
    pub generation: u64,
    pub selection: u64,
    pub endpoint_generation: u64,
    pub approval_id: String,
    pub arguments: serde_json::Value,
}
impl State {
    pub fn form(&mut self, project: &str) -> &mut Form {
        self.forms.entry(project.into()).or_default()
    }
    pub fn permits(
        &self,
        project: &str,
        run: &str,
        generation: u64,
        selection: u64,
        endpoint_generation: u64,
        pending: &serde_json::Value,
    ) -> bool {
        self.binding.as_ref().is_some_and(|b| {
            b.project == project
                && b.run == run
                && b.generation == generation
                && b.selection == selection
                && b.endpoint_generation == endpoint_generation
                && pending["id"] == b.approval_id
                && pending["arguments"] == b.arguments
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Request,
    Call,
}
pub(crate) fn render(
    ui: &mut eframe::egui::Ui,
    project: &Project,
    run: &serde_json::Value,
    selected: &str,
    form: &mut Form,
    busy: bool,
    call_allowed: bool,
) -> Option<Action> {
    ui.heading("수동 MCP 승인 요청");
    ui.label("이 수동 요청은 작업·역할 연결 없음. 승인 후 명시적으로 호출해야 합니다.");
    for (label, value) in [
        ("엔드포인트", &mut form.endpoint),
        ("도구 이름", &mut form.name),
        ("작업 경로", &mut form.cwd),
    ] {
        ui.label(label);
        ui.add_enabled(!busy, eframe::egui::TextEdit::singleline(value));
    }
    ui.label("인수 JSON 객체");
    ui.add_enabled(
        !busy,
        eframe::egui::TextEdit::multiline(&mut form.arguments),
    );
    let mut action = None;
    if ui
        .add_enabled(
            !busy && request(project, run, selected, form).is_some(),
            eframe::egui::Button::new("승인 요청"),
        )
        .clicked()
    {
        action = Some(Action::Request);
    }
    if approved_call(project, run, selected).is_some() {
        ui.label(format!(
            "승인 ID: {}",
            run["pending"]["id"].as_str().unwrap_or("")
        ));
        ui.label("서버의 승인된 요청 전체");
        ui.label(serde_json::to_string_pretty(&run["pending"]["arguments"]).unwrap_or_default());
        if ui
            .add_enabled(
                !busy && call_allowed,
                eframe::egui::Button::new("승인된 호출 실행"),
            )
            .clicked()
        {
            action = Some(Action::Call);
        }
        if !call_allowed {
            ui.label("요청 선택이나 주소가 변경되어 이 화면에서 호출할 수 없습니다.");
        }
    }
    if busy {
        ui.label("수동 MCP 요청 처리 중");
    }
    action
}
pub(crate) fn request(
    project: &Project,
    run: &serde_json::Value,
    selected: &str,
    form: &Form,
) -> Option<Command> {
    let arguments: serde_json::Value = serde_json::from_str(&form.arguments).ok()?;
    if run["id"] != selected
        || run["project_id"] != project.id
        || run["status"] != "awaiting_implementation"
        || run["agent_active"] != false
        || !run["pending"].is_null()
        || !crate::tool_approval_ui::approval_matches(project, run)
        || form.endpoint.trim().is_empty()
        || form.name.trim().is_empty()
        || form.cwd != "."
        || !arguments.is_object()
    {
        return None;
    }
    Some(Command::RequestMcpApproval {
        run_id: selected.into(),
        endpoint: form.endpoint.clone(),
        name: form.name.clone(),
        arguments,
        cwd: form.cwd.clone(),
    })
}
pub(crate) fn approved_call(
    project: &Project,
    run: &serde_json::Value,
    selected: &str,
) -> Option<Command> {
    let pending = run.get("pending")?;
    let args = pending.get("arguments")?;
    if run["id"] != selected
        || run["project_id"] != project.id
        || run["status"] != "approved_mcp"
        || run["agent_active"] != false
        || !crate::tool_approval_ui::approval_matches(project, run)
        || pending["tool"] != "mcp"
        || pending["authorized"] != true
        || pending.get("call_id") != Some(&serde_json::Value::Null)
        || args["endpoint"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
        || args["name"].as_str().is_none_or(|s| s.trim().is_empty())
        || !args["arguments"].is_object()
        || args["cwd"] != "."
    {
        return None;
    }
    let id = pending["id"].as_str().filter(|id| !id.is_empty())?;
    Some(Command::CallMcpApproved {
        run_id: selected.into(),
        approval_id: id.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Project, serde_json::Value, Form) {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":["src"],"requirements":[],"acceptance":[]}})).unwrap();
        let run = serde_json::json!({"id":"r","project_id":"a","spec_hash":"h","spec_version":1,"scope":["src"],"status":"awaiting_implementation","agent_active":false,"pending":null});
        let form = Form {
            endpoint: "https://example.test/mcp".into(),
            name: "fake".into(),
            arguments: "{\"value\":\"한글\"}".into(),
            cwd: ".".into(),
        };
        (p, run, form)
    }
    #[test]
    fn request_is_explicit_exact_and_never_calls_tool() {
        let (p, mut r, f) = fixture();
        assert!(
            matches!(request(&p,&r,"r",&f),Some(Command::RequestMcpApproval{run_id,endpoint,name,arguments,cwd}) if run_id=="r" && endpoint=="https://example.test/mcp" && name=="fake" && arguments["value"]=="한글" && cwd==".")
        );
        assert!(request(&p, &r, "other", &f).is_none());
        r["agent_active"] = true.into();
        assert!(request(&p, &r, "r", &f).is_none());
    }
    #[test]
    fn call_requires_current_approved_manual_snapshot_and_real_id() {
        let (p, mut r, _) = fixture();
        r["status"] = "approved_mcp".into();
        r["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":true,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{},"cwd":"."}});
        assert!(
            matches!(approved_call(&p,&r,"r"),Some(Command::CallMcpApproved{run_id,approval_id}) if run_id=="r" && approval_id=="actual")
        );
        r["pending"]["authorized"] = false.into();
        assert!(approved_call(&p, &r, "r").is_none());
        r["pending"]["authorized"] = true.into();
        r["pending"]["call_id"] = "bound-agent-call".into();
        assert!(approved_call(&p, &r, "r").is_none());
        r["pending"]["call_id"] = serde_json::Value::Null;
        r["spec_hash"] = "stale".into();
        assert!(approved_call(&p, &r, "r").is_none());
    }
    #[test]
    fn approved_manual_call_displays_full_snapshot_but_never_executes_automatically() {
        let (p, mut r, mut f) = fixture();
        r["status"] = "approved_mcp".into();
        r["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":true,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{"arg":"한글"},"cwd":"."}});
        let mut action = None;
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                action = render(ui, &p, &r, "r", &mut f, false, true);
            });
        });
        let labels: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                eframe::egui::Shape::Text(t) => Some(t.galley.job.text.as_str()),
                _ => None,
            })
            .collect();
        for text in [
            "작업·역할 연결 없음",
            "actual",
            "example.test",
            "fake",
            "한글",
            "승인된 호출 실행",
        ] {
            assert!(labels.iter().any(|s| s.contains(text)), "{text}");
        }
        assert_eq!(action, None);
    }
    #[test]
    fn only_explicit_request_sends_approval_then_refreshes_run() {
        let (p, r, f) = fixture();
        let (tx, work) = std::sync::mpsc::sync_channel(3);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.history.detail = Some(r);
        app.state.manual_mcp.forms.insert("a".into(), f);
        assert!(work.try_recv().is_err());
        app.manual_mcp_action(&p, Action::Request);
        let sent = work.try_recv().expect("approval request must be sent");
        assert!(matches!(sent.command, Command::RequestMcpApproval { .. }));
        app.manual_mcp_action(&p, Action::Request);
        assert!(work.try_recv().is_err());
        app.apply(crate::worker::Reply {
            kind: sent.kind,
            result: Ok(domain::ResponseData::Data(
                serde_json::json!({"approval_id":"actual"}),
            )),
        });
        assert!(matches!(work.try_recv().unwrap().command,Command::GetRun{run_id} if run_id=="r"));
    }
    #[test]
    fn explicit_call_uses_current_server_id_and_blocks_duplicate_or_late_error() {
        let (p, mut r, f) = fixture();
        r["status"] = "approved_mcp".into();
        r["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":true,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{},"cwd":"."}});
        let (tx, work) = std::sync::mpsc::sync_channel(3);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.manual_mcp.binding = Some(Binding {
            project: "a".into(),
            run: "r".into(),
            generation: app.state.generation,
            selection: app.state.history.selection_request,
            endpoint_generation: 0,
            approval_id: "actual".into(),
            arguments: r["pending"]["arguments"].clone(),
        });
        app.state.history.detail = Some(r);
        app.state.manual_mcp.forms.insert("a".into(), f);
        app.manual_mcp_action(&p, Action::Call);
        let sent = work.try_recv().expect("explicit call must be sent");
        assert!(
            matches!(sent.command,Command::CallMcpApproved{ref approval_id,..} if approval_id=="actual")
        );
        app.manual_mcp_action(&p, Action::Call);
        assert!(work.try_recv().is_err());
        app.state.select("b");
        app.state.status = "새 선택".into();
        app.apply(crate::worker::Reply {
            kind: sent.kind,
            result: Err("unknown".into()),
        });
        assert_eq!(app.state.status, "새 선택");
        assert!(work.try_recv().is_err());
    }
    #[test]
    fn selected_run_renders_manual_panel_without_network_effect() {
        let (p, r, _) = fixture();
        let (tx, work) = std::sync::mpsc::sync_channel(2);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.history.detail = Some(r);
        let out = eframe::egui::Context::default().run(
            eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1600.0, 4000.0),
                )),
                ..Default::default()
            },
            |ctx| app.render(ctx),
        );
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::Shape::Text(t) if t.galley.job.text=="수동 MCP 승인 요청")));
        assert!(work.try_recv().is_err());
    }
    #[test]
    fn queue_full_preserves_request_inputs_and_unapproved_call_is_not_sent() {
        let (p, r, f) = fixture();
        let (tx, work) = std::sync::mpsc::sync_channel(0);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.history.detail = Some(r);
        app.state.manual_mcp.forms.insert("a".into(), f);
        app.manual_mcp_action(&p, Action::Call);
        assert!(work.try_recv().is_err());
        app.manual_mcp_action(&p, Action::Request);
        assert!(work.try_recv().is_err());
        assert_eq!(app.state.manual_mcp.form("a").name, "fake");
        assert!(app.state.status.contains("대기열"));
        assert!(!app.state.manual_mcp.pending.contains("r"));
    }
    #[test]
    fn approved_call_is_blocked_without_current_request_binding_after_endpoint_change() {
        let (p, mut r, f) = fixture();
        r["status"] = "approved_mcp".into();
        r["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":true,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{},"cwd":"."}});
        let (tx, work) = std::sync::mpsc::sync_channel(2);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.history.detail = Some(r);
        app.state.manual_mcp.forms.insert("a".into(), f);
        app.state.mcp.select("a");
        app.state.mcp.edit("a", "https://example.test/changed");
        app.manual_mcp_action(&p, Action::Call);
        assert!(work.try_recv().is_err());
    }
    #[test]
    fn request_then_approve_then_explicit_call_preserves_exact_binding() {
        let (p, mut run, form) = fixture();
        let (tx, work) = std::sync::mpsc::sync_channel(4);
        let (_reply, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(p.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        app.state.history.detail = Some(run.clone());
        app.state.manual_mcp.forms.insert("a".into(), form);
        app.manual_mcp_action(&p, Action::Request);
        let request = work.try_recv().unwrap();
        assert!(matches!(
            request.command,
            Command::RequestMcpApproval { .. }
        ));
        app.apply(crate::worker::Reply {
            kind: request.kind,
            result: Ok(domain::ResponseData::Data(
                serde_json::json!({"approval_id":"actual"}),
            )),
        });
        let refresh = work.try_recv().unwrap();
        run["status"] = "awaiting_approval".into();
        run["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":false,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{"value":"한글"},"cwd":"."}});
        app.apply(crate::worker::Reply {
            kind: refresh.kind,
            result: Ok(domain::ResponseData::Data(run.clone())),
        });
        let item = crate::tool_approval_ui::pending(&p, &run, "r").unwrap();
        app.answer_tool(&p, item, true);
        let approve = work.try_recv().unwrap();
        assert!(matches!(
            approve.command,
            Command::ApproveTool { approved: true, .. }
        ));
        app.apply(crate::worker::Reply {
            kind: approve.kind,
            result: Ok(domain::ResponseData::Data(serde_json::json!({}))),
        });
        let refresh = work.try_recv().unwrap();
        run["status"] = "approved_mcp".into();
        run["pending"]["authorized"] = true.into();
        app.apply(crate::worker::Reply {
            kind: refresh.kind,
            result: Ok(domain::ResponseData::Data(run)),
        });
        assert!(work.try_recv().is_err());
        app.manual_mcp_action(&p, Action::Call);
        assert!(
            matches!(work.try_recv().unwrap().command,Command::CallMcpApproved{approval_id,..} if approval_id=="actual")
        );
    }
    #[test]
    fn approved_binding_is_invalidated_by_endpoint_or_selection_generation_change() {
        let (p, mut r, f) = fixture();
        r["status"] = "approved_mcp".into();
        r["pending"] = serde_json::json!({"id":"actual","tool":"mcp","call_id":null,"authorized":true,"arguments":{"endpoint":"https://example.test/mcp","name":"fake","arguments":{},"cwd":"."}});
        for change_endpoint in [true, false] {
            let (tx, work) = std::sync::mpsc::sync_channel(2);
            let (_reply, rx) = std::sync::mpsc::sync_channel(1);
            let mut app = crate::App {
                state: crate::State {
                    selected: Some("a".into()),
                    project: Some(p.clone()),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.state.mcp.select("a");
            app.state.history.project("a");
            app.state.history.selected = Some("r".into());
            app.state.history.detail = Some(r.clone());
            app.state.manual_mcp.binding = Some(Binding {
                project: "a".into(),
                run: "r".into(),
                generation: app.state.generation,
                selection: app.state.history.selection_request,
                endpoint_generation: 0,
                approval_id: "actual".into(),
                arguments: r["pending"]["arguments"].clone(),
            });
            app.state.manual_mcp.forms.insert(
                "a".into(),
                Form {
                    endpoint: f.endpoint.clone(),
                    name: f.name.clone(),
                    arguments: f.arguments.clone(),
                    cwd: f.cwd.clone(),
                },
            );
            if change_endpoint {
                app.state.mcp.edit("a", "https://example.test/new");
            } else {
                app.state.history.selection_request =
                    app.state.history.selection_request.wrapping_add(1);
            }
            app.manual_mcp_action(&p, Action::Call);
            assert!(work.try_recv().is_err());
        }
    }
}
