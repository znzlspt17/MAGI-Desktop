use domain::{Command, ResponseData, SecretString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Discover,
    Begin,
    Complete,
}

#[derive(Default)]
pub(crate) struct State {
    pub project: Option<String>,
    pub binding: (u64, u64),
    pub nonce: u64,
    pub pending: Option<(u64, Action)>,
    pub metadata_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub callback: Option<SecretString>,
    pub authorization_url: Option<SecretString>,
    pub metadata: Option<serde_json::Value>,
    pub complete: bool,
    pub error: Option<String>,
}
impl State {
    pub fn bind(&mut self, project: &str, epoch: u64, sequence: u64) {
        if self.project.as_deref() != Some(project) || self.binding != (epoch, sequence) {
            let nonce = self.nonce;
            *self = Self::default();
            self.nonce = nonce;
            self.project = Some(project.into());
            self.binding = (epoch, sequence);
        }
    }
    pub fn command(&self, action: Action) -> Option<Command> {
        let project_id = self.project.clone()?;
        if self.pending.is_some() {
            return None;
        }
        match action {
            Action::Discover => Some(Command::DiscoverMcpOAuth {
                project_id,
                metadata_url: self.metadata_url.clone(),
            }),
            Action::Begin
                if !self.client_id.trim().is_empty() && !self.redirect_uri.trim().is_empty() =>
            {
                Some(Command::BeginMcpOAuth {
                    project_id,
                    metadata_url: self.metadata_url.clone(),
                    client_id: self.client_id.clone(),
                    redirect_uri: self.redirect_uri.clone(),
                })
            }
            Action::Complete => Some(Command::CompleteMcpOAuth {
                project_id,
                callback: self
                    .callback
                    .as_ref()
                    .filter(|v| !v.0.trim().is_empty())?
                    .clone(),
            }),
            _ => None,
        }
    }
    pub fn sent(&mut self, action: Action) -> u64 {
        self.nonce = self.nonce.wrapping_add(1);
        self.pending = Some((self.nonce, action));
        self.error = None;
        if action == Action::Begin {
            self.authorization_url = None;
            self.complete = false;
        }
        if action == Action::Complete {
            self.callback = None;
            self.complete = false;
        }
        self.nonce
    }
    pub fn reply(&mut self, nonce: u64, action: Action, result: Result<ResponseData, String>) {
        if self.pending != Some((nonce, action)) {
            return;
        }
        self.pending = None;
        match (action, result) {
            (Action::Discover, Ok(ResponseData::Data(data))) if data.is_object() => {
                self.metadata = Some(data)
            }
            (Action::Begin, Ok(ResponseData::Data(data))) => {
                match data["authorization_url"].as_str().filter(|s| !s.is_empty()) {
                    Some(url) => self.authorization_url = Some(SecretString(url.into())),
                    None => self.error = Some("인증 시작 응답이 올바르지 않습니다.".into()),
                }
            }
            (Action::Complete, Ok(ResponseData::Empty)) => {
                self.complete = true;
                self.authorization_url = None;
            }
            _ => self.error = Some("OAuth 요청이 실패했거나 응답이 올바르지 않습니다.".into()),
        }
    }
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &mut State) -> Option<Action> {
    use eframe::egui;
    ui.heading("MCP OAuth 인증");
    let mut changed = false;
    let mut action = None;
    ui.add_enabled_ui(state.pending.is_none(), |ui| {
        for (label, value) in [
            ("메타데이터 URL (선택)", &mut state.metadata_url),
            ("클라이언트 ID", &mut state.client_id),
            ("리디렉션 URI", &mut state.redirect_uri),
        ] {
            ui.label(label);
            changed |= ui.text_edit_singleline(value).changed();
        }
    });
    if changed {
        state.authorization_url = None;
        state.callback = None;
        state.complete = false;
        state.metadata = None;
        state.error = None;
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                state.command(Action::Discover).is_some(),
                egui::Button::new("인증 정보 탐색"),
            )
            .clicked()
        {
            action = Some(Action::Discover);
        }
        if ui
            .add_enabled(
                state.command(Action::Begin).is_some(),
                egui::Button::new("인증 시작"),
            )
            .clicked()
        {
            action = Some(Action::Begin);
        }
    });
    if let Some(metadata) = &state.metadata {
        ui.label(serde_json::to_string_pretty(metadata).unwrap_or_default());
    }
    if let Some(url) = &state.authorization_url {
        ui.hyperlink(&url.0);
    }
    ui.label("리디렉션된 전체 callback URL");
    let callback = state
        .callback
        .get_or_insert_with(|| SecretString(String::new()));
    ui.add_enabled(
        state.pending.is_none(),
        egui::TextEdit::singleline(&mut callback.0).password(true),
    );
    if ui
        .add_enabled(
            state.command(Action::Complete).is_some(),
            egui::Button::new("인증 완료 요청"),
        )
        .clicked()
    {
        action = Some(Action::Complete);
    }
    if state.pending.is_some() {
        ui.label("OAuth 요청 처리 중");
    }
    if state.complete {
        ui.label("인증 응답을 받았습니다. 이 엔진 실행 중에 유지됩니다.");
    }
    if let Some(error) = &state.error {
        ui.label(error);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_flow_rejects_duplicates_and_ignores_superseded_replies() {
        let mut s = State::default();
        s.bind("a", 1, 3);
        s.client_id = "client".into();
        s.redirect_uri = "https://example.test/cb".into();
        assert!(
            matches!(s.command(Action::Begin), Some(Command::BeginMcpOAuth{project_id,..}) if project_id=="a")
        );
        let old = s.sent(Action::Begin);
        assert!(s.command(Action::Discover).is_none());
        s.bind("b", 2, 0);
        s.bind("a", 3, 3);
        s.reply(
            old,
            Action::Begin,
            Ok(ResponseData::Data(
                serde_json::json!({"authorization_url":"https://example.test/old"}),
            )),
        );
        assert!(s.authorization_url.is_none());
        assert!(!s.complete);
    }
    #[test]
    fn endpoint_change_discards_runtime_secret_and_callback_is_sent_only_explicitly() {
        let mut s = State::default();
        s.bind("a", 1, 1);
        s.callback = Some(SecretString("fake-callback".into()));
        assert!(matches!(
            s.command(Action::Complete),
            Some(Command::CompleteMcpOAuth { .. })
        ));
        s.bind("a", 1, 2);
        assert!(s.callback.is_none());
        assert!(s.command(Action::Complete).is_none());
    }
    #[test]
    fn begin_and_complete_display_only_real_acknowledgements() {
        let mut s = State::default();
        s.bind("a", 1, 1);
        s.client_id = "client".into();
        s.redirect_uri = "https://example.test/cb".into();
        let request = s.sent(Action::Begin);
        s.reply(
            request,
            Action::Begin,
            Ok(ResponseData::Data(
                serde_json::json!({"authorization_url":"https://example.test/auth"}),
            )),
        );
        assert_eq!(
            s.authorization_url.as_ref().map(|v| v.0.as_str()),
            Some("https://example.test/auth")
        );
        assert!(!s.complete);
        s.callback = Some(SecretString("fake-callback".into()));
        let request = s.sent(Action::Complete);
        assert!(s.callback.is_none());
        s.reply(request, Action::Complete, Err("fake error".into()));
        assert!(!s.complete);
        assert!(s.error.is_some());
        s.callback = Some(SecretString("fake-callback".into()));
        let request = s.sent(Action::Complete);
        s.reply(request, Action::Complete, Ok(ResponseData::Empty));
        assert!(s.complete);
    }
    #[test]
    fn oauth_panel_masks_callback_and_does_not_submit_without_click() {
        let mut s = State::default();
        s.bind("a", 1, 1);
        s.callback = Some(SecretString("fake-callback-value".into()));
        let mut action = None;
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                action = render(ui, &mut s);
            });
        });
        let labels: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                eframe::egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|s| s.contains("OAuth 인증")));
        assert!(!labels.iter().any(|s| s.contains("fake-callback-value")));
        assert_eq!(action, None);
    }
}
