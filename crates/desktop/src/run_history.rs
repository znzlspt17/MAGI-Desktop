use eframe::egui;
pub(crate) const PAGE_SIZE: usize = 20;
#[derive(Default)]
pub(crate) struct History {
    pub project: Option<String>,
    pub request: u64,
    pub runs: Vec<serde_json::Value>,
    pub offset: usize,
    pub total: usize,
    pub loaded: bool,
    pub error: Option<String>,
    pub selected: Option<String>,
    pub selection_request: u64,
    pub detail: Option<serde_json::Value>,
    pub events: Vec<domain::RunEvent>,
}
impl History {
    pub fn project(&mut self, id: &str) {
        if self.project.as_deref() != Some(id) {
            self.project = Some(id.into());
            self.request = self.request.wrapping_add(1);
            self.runs.clear();
            self.loaded = false;
            self.error = None;
            self.offset = 0;
            self.total = 0;
            self.selected = None;
            self.detail = None;
            self.events.clear();
            self.selection_request = self.selection_request.wrapping_add(1);
        }
    }
}
fn status(run: &serde_json::Value) -> &str {
    if !run["persistence"].is_null() && run["persistence"]["state"] != "cleanup_pending" {
        "결과 저장 대기"
    } else {
        run["status"].as_str().unwrap_or("알 수 없음")
    }
}
pub(crate) enum Action {
    Page(usize),
    Select(String),
}
pub(crate) fn render(ui: &mut egui::Ui, history: &History) -> Option<Action> {
    let mut action = ui
        .button("실행 기록 조회")
        .clicked()
        .then_some(Action::Page(0));
    if history.loaded {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(history.offset > 0, egui::Button::new("이전 페이지"))
                .clicked()
            {
                action = Some(Action::Page(history.offset.saturating_sub(PAGE_SIZE)));
            }
            if ui
                .add_enabled(
                    history.offset.saturating_add(PAGE_SIZE) < history.total,
                    egui::Button::new("다음 페이지"),
                )
                .clicked()
            {
                action = Some(Action::Page(history.offset.saturating_add(PAGE_SIZE)));
            }
        });
    }
    if let Some(error) = &history.error {
        ui.label(error);
    }
    if history.loaded && history.runs.is_empty() {
        ui.label("실행 기록이 없습니다.");
    }
    for run in &history.runs {
        if let Some(id) = run["id"].as_str() {
            if ui
                .selectable_label(
                    history.selected.as_deref() == Some(id),
                    format!("{}  {}", id, status(run)),
                )
                .clicked()
            {
                action = Some(Action::Select(id.into()));
            }
        }
    }
    if let Some(detail) = &history.detail {
        ui.label(format!("실행 상태: {}", status(detail)));
    }
    for event in &history.events {
        ui.label(format!(
            "{}  {}  {}",
            event.sequence, event.kind, event.message
        ));
    }
    action
}
