use domain::Project;
fn snapshot_matches(project: &Project, run: &serde_json::Value) -> bool {
    let Some(spec) = project.approved.as_ref() else {
        return false;
    };
    run["project_id"] == project.id
        && run["spec_hash"] == spec.hash
        && project.draft_hash == spec.hash
        && run["spec_version"].as_u64() == Some(spec.version)
        && run["context_version"].as_u64() == Some(project.context_version)
        && run["revision"] == spec.base_revision
        && run["scope"] == serde_json::json!(spec.scope)
        && run["agent_configs"] == serde_json::json!(project.agent_configs)
        && run["model_provider"] == "openai"
}
pub(crate) fn start_available(project: &Project, run: &serde_json::Value) -> bool {
    snapshot_matches(project, run)
        && run["status"] == "awaiting_implementation"
        && run["agent_active"] == false
        && run["pending"].is_null()
}
pub(crate) fn resume_available(project: &Project, run: &serde_json::Value) -> bool {
    let status = run["status"].as_str().unwrap_or("");
    snapshot_matches(project, run)
        && run["agent_active"] == false
        && run["resources_unknown"] != true
        && [
            "awaiting_implementation",
            "interrupted",
            "awaiting_approval",
            "approved_mcp",
            "approved_exec",
        ]
        .contains(&status)
        && (run["pending"].is_null()
            || ["awaiting_approval", "approved_mcp", "approved_exec"].contains(&status))
}
pub(crate) fn render_resume(
    ui: &mut eframe::egui::Ui,
    project: &Project,
    run: &serde_json::Value,
    busy: bool,
) -> bool {
    let clicked = ui
        .add_enabled(
            !busy && resume_available(project, run),
            eframe::egui::Button::new("재개 요청"),
        )
        .clicked();
    ui.label("저장된 재개 상태는 서버가 확인합니다. 요청 접수와 실행 완료는 다릅니다.");
    clicked
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Project, serde_json::Value) {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":["src"],"requirements":[],"acceptance":[]},"context_version":2,"agent_configs":[],"autonomy_mode":"step"})).unwrap();
        let run = serde_json::json!({"id":"r","project_id":"a","spec_hash":"h","spec_version":1,"context_version":2,"revision":"rev","scope":["src"],"agent_configs":[],"model_provider":"openai","status":"awaiting_implementation","agent_active":false,"pending":null});
        (p, run)
    }
    #[test]
    fn start_requires_selected_run_approval_snapshot_and_supported_provider() {
        let (p, mut r) = fixture();
        assert!(start_available(&p, &r));
        r["context_version"] = 3.into();
        assert!(!start_available(&p, &r));
        r["context_version"] = 2.into();
        r["status"] = "preparing".into();
        assert!(!start_available(&p, &r));
        r["status"] = "awaiting_implementation".into();
        r["agent_active"] = true.into();
        assert!(!start_available(&p, &r));
        r["agent_active"] = false.into();
        r["model_provider"] = "mock".into();
        assert!(!start_available(&p, &r));
    }
    #[test]
    fn resume_requires_matching_approval_snapshot_inactive_run_and_supported_provider() {
        let (p, mut r) = fixture();
        r["status"] = "interrupted".into();
        r["resources_unknown"] = false.into();
        assert!(resume_available(&p, &r));
        r["agent_active"] = true.into();
        assert!(!resume_available(&p, &r));
        r["agent_active"] = false.into();
        r["resources_unknown"] = true.into();
        assert!(!resume_available(&p, &r));
        r["resources_unknown"] = false.into();
        r["context_version"] = 3.into();
        assert!(!resume_available(&p, &r));
        r["context_version"] = 2.into();
        r["model_provider"] = "mock".into();
        assert!(!resume_available(&p, &r));
        r["model_provider"] = "openai".into();
        r["status"] = "completed".into();
        assert!(!resume_available(&p, &r));
        r["status"] = "awaiting_approval".into();
        r["pending"] = serde_json::json!({"id":"pending"});
        assert!(resume_available(&p, &r));
    }
    #[test]
    fn resume_request_is_explicit_selected_and_duplicate_protected() {
        let (p, mut run) = fixture();
        run["status"] = "interrupted".into();
        run["resources_unknown"] = false.into();
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
        app.state.history.detail = Some(run);
        assert!(work.try_recv().is_err());
        app.resume_agent(&p);
        let sent = work.try_recv().expect("explicit request must be sent");
        assert!(
            matches!(sent.command,domain::Command::ResumeAgent { run_id, mock_script } if run_id=="r" && mock_script.is_empty())
        );
        app.resume_agent(&p);
        assert!(work.try_recv().is_err());
        app.apply(crate::worker::Reply {
            kind: sent.kind,
            result: Ok(domain::ResponseData::Empty),
        });
        assert!(app.state.status.contains("요청을 접수"));
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::GetRun{run_id} if run_id=="r")
        );
    }
    #[test]
    fn selected_run_displays_resume_control_without_dispatching() {
        let (p, mut run) = fixture();
        run["status"] = "interrupted".into();
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
        app.state.history.detail = Some(run);
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
        assert!(out.shapes.iter().any(
            |s| matches!(&s.shape,eframe::egui::Shape::Text(t) if t.galley.job.text=="재개 요청")
        ));
        assert!(work.try_recv().is_err());
    }
    #[test]
    fn resume_queue_full_and_late_error_preserve_current_selection() {
        for capacity in [0, 2] {
            let (p, mut run) = fixture();
            run["status"] = "interrupted".into();
            let (tx, work) = std::sync::mpsc::sync_channel(capacity);
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
            app.resume_agent(&p);
            if capacity == 0 {
                assert!(app.state.status.contains("대기열"));
                assert_eq!(app.state.history.detail, Some(run));
                assert!(!app.state.agent_pending.contains("r"));
            } else {
                let sent = work.try_recv().unwrap();
                app.state.select("b");
                app.state.status = "새 선택".into();
                app.apply(crate::worker::Reply {
                    kind: sent.kind,
                    result: Err("old".into()),
                });
                assert_eq!(app.state.status, "새 선택");
                assert!(work.try_recv().is_err());
            }
        }
    }
}
pub(crate) fn render(
    ui: &mut eframe::egui::Ui,
    project: &Project,
    run: &serde_json::Value,
    busy: bool,
) -> bool {
    let start = ui
        .add_enabled(
            !busy && start_available(project, run),
            eframe::egui::Button::new("에이전트 시작"),
        )
        .clicked();
    if run["model_provider"] == "mock" {
        ui.label("Mock 실행에는 별도의 테스트 스크립트가 필요합니다.");
    }
    if run["model_provider"] != "mock" && run["model_provider"] != "openai" {
        ui.label("선택한 실행의 모델 제공자는 지원하지 않습니다.");
    }
    if let Some(spec) = &project.approved {
        if run["spec_hash"] == spec.hash && run["spec_version"].as_u64() == Some(spec.version) {
            ui.label(format!("현재 승인된 명세 버전: {}", spec.version));
        } else {
            ui.label("선택한 실행의 승인 명세와 현재 명세가 다릅니다.");
        }
    }
    start
}
#[cfg(test)]
mod ui_tests {
    use super::*;
    #[test]
    fn resume_panel_explains_server_checkpoint_check_without_auto_start() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":[],"requirements":[],"acceptance":[]}})).unwrap();
        let r = serde_json::json!({"project_id":"a","spec_hash":"h","spec_version":1,"context_version":0,"revision":"rev","scope":[],"agent_configs":[],"model_provider":"openai","status":"interrupted","agent_active":false,"pending":null});
        let mut action = true;
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                action = render_resume(ui, &p, &r, false);
            });
        });
        assert!(!action);
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::Shape::Text(t) if t.galley.job.text.contains("저장된 재개 상태는 서버가 확인"))));
    }
    #[test]
    fn unsupported_resume_and_mock_are_explained_without_claiming_execution() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let r = serde_json::json!({"model_provider":"mock"});
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &p, &r, false);
            });
        });
        for label in [
            "에이전트 시작",
            "Mock 실행에는 별도의 테스트 스크립트가 필요합니다.",
        ] {
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::epaint::Shape::Text(t) if t.galley.job.text==label)),"{label}");
        }
    }
}
