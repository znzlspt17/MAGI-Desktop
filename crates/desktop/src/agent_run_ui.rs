use domain::Project;
pub(crate) fn start_available(project: &Project, run: &serde_json::Value) -> bool {
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
        && run["status"] == "awaiting_implementation"
        && run["agent_active"] == false
        && run["pending"].is_null()
        && run["model_provider"] == "openai"
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
    ui.add_enabled(false, eframe::egui::Button::new("재개"));
    ui.label("재개는 이 화면에서 지원하지 않습니다.");
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
            "재개",
            "재개는 이 화면에서 지원하지 않습니다.",
            "Mock 실행에는 별도의 테스트 스크립트가 필요합니다.",
        ] {
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::epaint::Shape::Text(t) if t.galley.job.text==label)),"{label}");
        }
    }
}
