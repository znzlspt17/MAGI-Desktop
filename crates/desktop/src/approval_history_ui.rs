use domain::Project;
#[derive(Default)]
pub(crate) struct State {
    selections: std::collections::HashMap<String, (u64, String)>,
}
impl State {
    pub fn select(&mut self, project: &str, version: u64, hash: &str) {
        self.selections
            .insert(project.into(), (version, hash.into()));
    }
    pub fn selected<'a>(&self, project: &'a Project) -> Option<&'a domain::ApprovedSpec> {
        let (version, hash) = self.selections.get(&project.id)?;
        project
            .approval_history
            .iter()
            .find(|item| item.version == *version && item.hash == *hash)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(id: &str) -> Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"current","approved":{"version":2,"hash":"current","base_revision":"rev2","scope":[],"requirements":["현재"],"acceptance":[]},"approval_history":[{"version":1,"hash":"old","base_revision":"rev1","scope":[],"requirements":["이전"],"acceptance":[],"markdown":"첫 줄\n둘째 줄","source_links":["출처"]},{"version":2,"hash":"current","base_revision":"rev2","scope":[],"requirements":["현재"],"acceptance":[]}]})).unwrap()
    }
    #[test]
    fn history_selection_is_project_scoped_and_resolves_actual_saved_copy() {
        let a = fixture("a");
        let b = fixture("b");
        let mut state = State::default();
        state.select("a", 1, "old");
        assert_eq!(state.selected(&a).unwrap().markdown, "첫 줄\n둘째 줄");
        assert!(state.selected(&b).is_none());
        state.select("b", 2, "current");
        assert_eq!(state.selected(&a).unwrap().version, 1);
        assert_eq!(state.selected(&b).unwrap().version, 2);
    }
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &mut State, project: &Project) {
    ui.heading("승인 이력");
    if let Some(current) = &project.approved {
        ui.label(format!(
            "현재 승인: 버전 {} / {}",
            current.version, current.hash
        ));
    } else {
        ui.label("현재 승인된 명세가 없습니다.");
    }
    if project.approval_history.is_empty() {
        ui.label("저장된 승인 이력이 없습니다.");
    }
    for item in &project.approval_history {
        let selected = state
            .selections
            .get(&project.id)
            .is_some_and(|(v, h)| *v == item.version && h == &item.hash);
        if ui
            .selectable_label(selected, format!("버전 {} / {}", item.version, item.hash))
            .clicked()
        {
            state.select(&project.id, item.version, &item.hash);
        }
    }
    if let Some(item) = state.selected(project) {
        ui.label(format!(
            "선택한 승인 사본: 버전 {} / {}",
            item.version, item.hash
        ));
        ui.label(format!("기준 revision: {}", item.base_revision));
        for (label, values) in [
            ("범위", &item.scope),
            ("요구사항", &item.requirements),
            ("승인 기준", &item.acceptance),
            ("출처", &item.source_links),
            ("제약", &item.constraints),
            ("수용한 선택 항목", &item.accepted_optional),
            ("재사용 결정", &item.reused_decisions),
        ] {
            ui.label(label);
            for value in values {
                ui.label(value);
            }
        }
        for task in &item.tasks {
            ui.label(format!("작업: {}", task.id));
            ui.label(format!("의존: {:?}", task.dependencies));
            ui.label(format!("경로: {:?}", task.owned_paths));
            ui.label(format!("검증: {:?}", task.validation));
        }
        for binding in &item.task_bindings {
            ui.label(format!("작업 연결: {} / {}", binding.task_id, binding.goal));
            ui.label(format!("요구사항: {:?}", binding.requirement_ids));
            ui.label(format!("승인 기준: {:?}", binding.acceptance_ids));
        }
        ui.label("승인 문서");
        let lines: Vec<&str> = item.markdown.split('\n').collect();
        eframe::egui::ScrollArea::vertical()
            .id_salt("approval_history_document")
            .max_height(240.0)
            .show_rows(ui, 20.0, lines.len(), |ui, range| {
                for i in range {
                    ui.label(lines[i]);
                }
            });
    } else if state.selections.contains_key(&project.id) {
        ui.label("선택한 승인 사본을 현재 이력에서 찾을 수 없습니다.");
    }
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn shows_selected_saved_snapshot_and_current_approval_separately() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"new","approved":{"version":2,"hash":"new","base_revision":"r2","scope":[],"requirements":["new req"],"acceptance":[]},"approval_history":[{"version":1,"hash":"old","base_revision":"r1","scope":[],"requirements":["old req"],"acceptance":["old ac"],"source_links":["source"],"markdown":"old doc","accepted_optional":["opt"],"reused_decisions":["decision"],"tasks":[{"id":"task","dependencies":[],"owned_paths":[],"validation":[]}],"task_bindings":[]}]})).unwrap();
        let mut state = State::default();
        state.select("a", 1, "old");
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| render(ui, &mut state, &p));
        });
        let labels: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|s| {
                if let eframe::egui::epaint::Shape::Text(t) = &s.shape {
                    Some(t.galley.job.text.clone())
                } else {
                    None
                }
            })
            .collect();
        for v in [
            "현재 승인: 버전 2",
            "버전 1",
            "old",
            "old req",
            "old ac",
            "source",
            "old doc",
            "opt",
            "decision",
            "task",
        ] {
            assert!(labels.iter().any(|s| s.contains(v)), "{v}");
        }
    }
}
