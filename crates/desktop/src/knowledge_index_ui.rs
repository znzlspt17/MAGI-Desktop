use std::collections::HashMap;
#[derive(Default)]
pub(crate) struct Form {
    pub endpoint: String,
    pub model: String,
    pub version: String,
    pub dimensions: String,
    pub document: String,
    pub sequence: u64,
    pub pending: bool,
    pub mutation_request: Option<(u64, u64)>,
    pub control_request: Option<(u64, u64, String)>,
    pub status: Option<String>,
    pub released: Option<bool>,
    pub message: Option<String>,
    pub error: Option<String>,
}
#[derive(Default)]
pub(crate) struct State {
    pub forms: HashMap<String, Form>,
    pub active: Option<String>,
    pub epoch: u64,
    pub secret: Option<(String, domain::SecretString)>,
}
impl State {
    pub fn reselect(&mut self, id: &str) {
        self.secret = None;
        self.active = Some(id.into());
        self.epoch = self.epoch.wrapping_add(1);
    }
    pub fn form(&mut self, id: &str) -> &mut Form {
        self.forms.entry(id.into()).or_default()
    }
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.secret = None;
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
    pub fn set_secret(&mut self, id: &str, secret: domain::SecretString) {
        if self.active.as_deref() == Some(id) {
            self.secret = Some((id.into(), secret));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_switch_preserves_public_input_but_discards_other_projects_runtime_secret() {
        let mut s = State::default();
        s.select("a");
        s.form("a").endpoint = "https://example.test".into();
        s.set_secret("a", domain::SecretString("fake-a".into()));
        assert_eq!(s.secret.as_ref().unwrap().0, "a");
        let epoch = s.epoch;
        s.select("b");
        assert!(s.secret.is_none());
        s.set_secret("b", domain::SecretString("fake-b".into()));
        s.select("a");
        assert!(s.secret.is_none());
        assert_eq!(s.form("a").endpoint, "https://example.test");
        assert!(s.epoch > epoch);
    }
}
#[derive(Debug, PartialEq)]
pub(crate) struct Status {
    pub phase: String,
    pub released: bool,
}
pub(crate) fn parse_status(data: &serde_json::Value) -> Result<Status, String> {
    let phase = data["status"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("색인 상태 응답이 올바르지 않습니다.")?;
    let released = data["resources_released"]
        .as_bool()
        .ok_or("색인 자원 상태 응답이 올바르지 않습니다.")?;
    Ok(Status {
        phase: phase.into(),
        released,
    })
}
#[cfg(test)]
mod status_tests {
    use super::*;
    #[test]
    fn reports_only_actual_status_and_resource_release() {
        assert_eq!(
            parse_status(&serde_json::json!({"status":"cancelling","resources_released":false}))
                .unwrap(),
            Status {
                phase: "cancelling".into(),
                released: false
            }
        );
        assert!(parse_status(&serde_json::json!({"status":"running"})).is_err());
        assert!(parse_status(&serde_json::json!({"resources_released":true})).is_err());
        assert!(
            parse_status(&serde_json::json!({"status":"running","resources_released":"yes"}))
                .is_err()
        );
    }
}
pub(crate) fn configuration(state: &State, id: &str) -> Option<domain::Command> {
    if state.active.as_deref() != Some(id) {
        return None;
    }
    let f = state.forms.get(id)?;
    let (owner, secret) = state.secret.as_ref()?;
    if owner != id
        || f.endpoint.trim().is_empty()
        || f.model.trim().is_empty()
        || f.version.trim().is_empty()
        || secret.0.trim().is_empty()
    {
        return None;
    }
    let dimensions = f.dimensions.parse::<usize>().ok().filter(|v| *v > 0)?;
    Some(domain::Command::ConfigureEmbedding {
        project_id: id.into(),
        endpoint: f.endpoint.clone(),
        model: f.model.clone(),
        version: f.version.clone(),
        dimensions,
        credential: secret.clone(),
    })
}
#[cfg(test)]
mod config_tests {
    use super::*;
    #[test]
    fn configuration_uses_only_own_runtime_credential_and_valid_public_fields() {
        let mut s = State::default();
        s.select("a");
        let f = s.form("a");
        f.endpoint = "https://example.test".into();
        f.model = "fake-model".into();
        f.version = "v1".into();
        f.dimensions = "3".into();
        s.set_secret("a", domain::SecretString("fake-key".into()));
        assert!(
            matches!(configuration(&s,"a"),Some(domain::Command::ConfigureEmbedding{project_id,dimensions:3,..}) if project_id=="a")
        );
        assert!(configuration(&s, "b").is_none());
        s.form("a").dimensions = "0".into();
        assert!(configuration(&s, "a").is_none());
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Action {
    Configure,
    Start,
    Status,
    Cancel,
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &mut State, id: &str) -> Option<Action> {
    state.select(id);
    if state.secret.as_ref().is_none_or(|(owner, _)| owner != id) {
        state.set_secret(id, domain::SecretString(String::new()));
    }
    let State { forms, secret, .. } = state;
    let form = forms.entry(id.into()).or_default();
    let (_, secret) = secret.as_mut()?;
    let mut changed = false;
    let mut action = None;
    ui.heading("임베딩 설정과 문서 색인");
    ui.label("저장된 런타임 설정은 이 화면에서 조회할 수 없습니다.");
    for (label, value) in [
        ("엔드포인트", &mut form.endpoint),
        ("모델", &mut form.model),
        ("버전", &mut form.version),
        ("차원", &mut form.dimensions),
    ] {
        ui.label(label);
        changed |= ui.text_edit_singleline(value).changed();
    }
    ui.label("런타임 자격정보");
    changed |= ui
        .add_enabled(
            !form.pending,
            eframe::egui::TextEdit::singleline(&mut secret.0).password(true),
        )
        .changed();
    if ui
        .add_enabled(
            !form.pending && form.control_request.is_none(),
            eframe::egui::Button::new("임베딩 설정"),
        )
        .clicked()
    {
        action = Some(Action::Configure);
    }
    ui.label("문서 ID");
    changed |= ui.text_edit_singleline(&mut form.document).changed();
    let has_document = !form.document.trim().is_empty();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !form.pending && form.control_request.is_none() && has_document,
                eframe::egui::Button::new("색인 시작"),
            )
            .clicked()
        {
            action = Some(Action::Start);
        }
        if ui
            .add_enabled(
                form.control_request.is_none() && has_document,
                eframe::egui::Button::new("색인 상태 조회"),
            )
            .clicked()
        {
            action = Some(Action::Status);
        }
        if ui
            .add_enabled(
                form.control_request.is_none() && has_document,
                eframe::egui::Button::new("색인 취소 요청"),
            )
            .clicked()
        {
            action = Some(Action::Cancel);
        }
    });
    if changed {
        form.sequence = form.sequence.wrapping_add(1);
        form.status = None;
        form.released = None;
        form.message = None;
        form.error = None;
    }
    if form.pending {
        ui.label("설정 또는 색인 시작 요청 처리 중");
    }
    if let Some((_, _, document)) = &form.control_request {
        ui.label(format!("문서 {document} 제어 요청 처리 중"));
    }
    if let Some(message) = &form.message {
        ui.label(message);
    }
    if let Some(error) = &form.error {
        ui.label(error);
    }
    if let Some(status) = &form.status {
        ui.label(format!("마지막 응답 상태: {status}"));
    }
    if let Some(released) = form.released {
        ui.label(if released {
            "서버 보고 자원 반환: 완료"
        } else {
            "서버 보고 자원 반환: 아직 안 됨"
        });
    } else if form.status.is_some() {
        ui.label("자원 반환 여부: 확인되지 않음");
    }
    action
}
#[cfg(test)]
mod ui_tests {
    use super::*;
    #[test]
    fn config_and_document_actions_are_visible_and_secret_is_masked() {
        let mut state = State::default();
        state.select("a");
        state.set_secret("a", domain::SecretString("fake-secret-value".into()));
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "a");
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
        for label in [
            "임베딩 설정",
            "저장된 런타임 설정은 이 화면에서 조회할 수 없습니다.",
            "문서 ID",
            "색인 시작",
            "색인 상태 조회",
            "색인 취소 요청",
        ] {
            assert!(labels.iter().any(|s| s.contains(label)), "{label}");
        }
        assert!(!labels.iter().any(|s| s.contains("fake-secret-value")));
    }
}
#[cfg(test)]
mod status_display_tests {
    use super::*;
    #[test]
    fn index_status_is_last_report_and_false_does_not_mean_released() {
        let mut s = State::default();
        s.select("a");
        let f = s.form("a");
        f.status = Some("cancelling".into());
        f.released = Some(false);
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
        assert!(labels.iter().any(|s| s == "마지막 응답 상태: cancelling"));
        assert!(labels
            .iter()
            .any(|s| s == "서버 보고 자원 반환: 아직 안 됨"));
        assert!(!labels.iter().any(|s| s == "서버 보고 자원 반환: 완료"));
    }
}
