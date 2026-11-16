use std::collections::HashMap;
#[derive(Default)]
pub(crate) struct Form {
    pub endpoint: String,
    pub saved: Option<String>,
    pub save_pending: bool,
    pub list_pending: bool,
    pub sequence: u64,
    pub endpoint_generation: u64,
    pub tools: Vec<serde_json::Value>,
    pub message: Option<String>,
    pub error: Option<String>,
}
#[derive(Default)]
pub(crate) struct State {
    pub forms: HashMap<String, Form>,
    pub active: Option<String>,
    pub epoch: u64,
}
impl State {
    pub fn reselect(&mut self, id: &str) {
        self.active = Some(id.into());
        self.epoch = self.epoch.wrapping_add(1);
    }
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
    pub fn edit(&mut self, id: &str, text: &str) {
        let form = self.form(id);
        if form.endpoint != text {
            form.endpoint = text.into();
            form.sequence = form.sequence.wrapping_add(1);
            form.endpoint_generation = form.endpoint_generation.wrapping_add(1);
            form.tools.clear();
            form.message = None;
            form.error = None;
        }
    }
    pub fn form(&mut self, id: &str) -> &mut Form {
        self.forms.entry(id.into()).or_default()
    }
}
impl Form {
    pub fn can_use_endpoint(&self) -> bool {
        self.saved
            .as_ref()
            .is_some_and(|saved| saved == &self.endpoint)
            || (self.saved.is_none() && self.endpoint.is_empty())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_edits_are_project_scoped_and_invalidate_old_results() {
        let mut s = State::default();
        s.select("a");
        let epoch = s.epoch;
        s.edit("a", "https://example.test/a");
        s.form("a").saved = Some("https://example.test/a".into());
        s.form("a").tools = vec![serde_json::json!({"name":"old"})];
        let seq = s.form("a").sequence;
        s.edit("a", "https://example.test/new");
        assert!(s.form("a").tools.is_empty());
        assert!(s.form("a").sequence > seq);
        assert_eq!(s.form("a").saved.as_deref(), Some("https://example.test/a"));
        s.select("b");
        s.edit("b", "https://example.test/b");
        s.select("a");
        assert!(s.epoch > epoch);
        assert_eq!(s.form("a").endpoint, "https://example.test/new");
        assert_eq!(s.form("b").endpoint, "https://example.test/b");
    }
}
pub(crate) enum Action {
    Save(String),
    List,
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &mut State, id: &str) -> Option<Action> {
    ui.heading("MCP 연결과 도구");
    let mut endpoint = state.form(id).endpoint.clone();
    ui.label("엔드포인트");
    if ui.text_edit_singleline(&mut endpoint).changed() {
        state.edit(id, &endpoint);
    }
    let form = state.form(id);
    let busy = form.save_pending || form.list_pending;
    let mut action = None;
    if form.saved.is_none() {
        ui.label("서버에 저장된 주소는 이 화면에서 조회할 수 없습니다.");
    }
    if form
        .saved
        .as_ref()
        .is_some_and(|saved| saved != &form.endpoint)
    {
        ui.label("편집한 주소는 아직 저장되지 않았습니다.");
    }
    ui.label("주소를 저장하면 기존 도구 허용 정책이 초기화됩니다.");
    if ui
        .add_enabled(
            !busy && !form.endpoint.trim().is_empty(),
            eframe::egui::Button::new("엔드포인트 저장"),
        )
        .clicked()
    {
        action = Some(Action::Save(form.endpoint.clone()));
    }
    let can_list = form.can_use_endpoint();
    if ui
        .add_enabled(
            !busy && can_list,
            eframe::egui::Button::new("도구 목록 조회"),
        )
        .clicked()
    {
        action = Some(Action::List);
    }
    if form.save_pending {
        ui.label("주소 저장 중");
    }
    if form.list_pending {
        ui.label("도구 목록 조회 중");
    }
    if let Some(message) = &form.message {
        ui.label(message);
    }
    if let Some(error) = &form.error {
        ui.label(error);
    }
    if !form.tools.is_empty() {
        ui.label("서버가 제공한 정보");
        for tool in &form.tools {
            ui.group(|ui| {
                ui.label(serde_json::to_string_pretty(tool).unwrap_or_default());
            });
        }
    }
    action
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn shows_raw_tool_schema_and_server_annotations_without_safety_claims() {
        let mut s = State::default();
        s.select("a");
        s.form("a").tools = vec![
            serde_json::json!({"name":"fake","description":"테스트","inputSchema":{"type":"object","properties":{"arg":{"type":"string"}}},"annotations":{"readOnlyHint":true}}),
        ];
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut s, "a");
            });
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
        for text in [
            "엔드포인트 저장",
            "도구 목록 조회",
            "fake",
            "inputSchema",
            "readOnlyHint",
            "서버가 제공한 정보",
        ] {
            assert!(labels.iter().any(|s| s.contains(text)), "{text}");
        }
        assert!(!labels.iter().any(|s| s.contains("부작용 없음")));
    }
}
