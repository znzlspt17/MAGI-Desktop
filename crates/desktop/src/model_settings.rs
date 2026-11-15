use domain::Project;
use eframe::egui;

pub(crate) struct CredentialForm {
    endpoint: String,
    secret: domain::SecretString,
}
impl Default for CredentialForm {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            secret: domain::SecretString(String::new()),
        }
    }
}

pub(crate) fn credentials(
    ui: &mut egui::Ui,
    project: &Project,
    forms: &mut std::collections::HashMap<String, CredentialForm>,
    pending: bool,
) -> Option<Result<domain::Command, String>> {
    let form = forms.entry(project.id.clone()).or_default();
    ui.label("OpenAI 호환 endpoint");
    ui.add_enabled(!pending, egui::TextEdit::singleline(&mut form.endpoint));
    ui.label("실행 중에만 사용하는 키");
    ui.add_enabled(
        !pending,
        egui::TextEdit::singleline(&mut form.secret.0).password(true),
    );
    if ui
        .add_enabled(!pending, egui::Button::new("런타임 키 설정"))
        .clicked()
    {
        if form.endpoint.trim().is_empty() || form.secret.0.trim().is_empty() {
            return Some(Err("endpoint와 키를 입력하세요".into()));
        }
        return Some(Ok(domain::Command::SetProviderCredential {
            project_id: project.id.clone(),
            endpoint: form.endpoint.clone(),
            credential: form.secret.clone(),
        }));
    }
    None
}

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Form {
    provider: String,
    model: String,
    streaming: bool,
    tools: bool,
    context: String,
}

impl Form {
    pub(crate) fn from_project(project: &Project) -> Self {
        project
            .model_settings
            .as_ref()
            .map(|settings| Self {
                provider: settings.provider.clone(),
                model: settings.model.clone(),
                streaming: settings.capabilities.streaming,
                tools: settings.capabilities.tools,
                context: settings
                    .capabilities
                    .context_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            })
            .unwrap_or_else(|| Self {
                provider: "mock".into(),
                ..Default::default()
            })
    }
}

pub(crate) fn render(
    ui: &mut egui::Ui,
    project: &Project,
    forms: &mut std::collections::HashMap<String, Form>,
    baselines: &mut std::collections::HashMap<String, Form>,
    pending: bool,
) -> Option<Result<(Form, domain::Command), String>> {
    let current = Form::from_project(project);
    let previous = baselines
        .entry(project.id.clone())
        .or_insert_with(|| current.clone());
    let form = forms
        .entry(project.id.clone())
        .or_insert_with(|| current.clone());
    if *form == *previous {
        *form = current.clone();
    }
    *previous = current;
    ui.heading("모델 설정");
    egui::ComboBox::from_label("제공자")
        .selected_text(&form.provider)
        .show_ui(ui, |ui| {
            for provider in ["mock", "local", "openai"] {
                ui.selectable_value(&mut form.provider, provider.into(), provider);
            }
        });
    ui.label("모델 이름 또는 경로");
    ui.text_edit_singleline(&mut form.model);
    ui.checkbox(&mut form.streaming, "스트리밍 지원");
    ui.checkbox(&mut form.tools, "도구 호출 지원");
    ui.label("최대 컨텍스트 토큰 (선택)");
    ui.text_edit_singleline(&mut form.context);
    if form.provider == "local" {
        ui.label("로컬 모델 추론은 아직 사용할 수 없습니다.");
    }
    if ui
        .add_enabled(!pending, egui::Button::new("모델 설정 저장"))
        .clicked()
    {
        let context_tokens = if form.context.trim().is_empty() {
            None
        } else {
            match form.context.parse::<u64>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Some(Err("컨텍스트 크기는 양의 정수여야 합니다".into())),
            }
        };
        if form.model.trim().is_empty() {
            return Some(Err("모델 이름 또는 경로를 입력하세요".into()));
        }
        return Some(Ok((
            form.clone(),
            domain::Command::ConfigureModel {
                project_id: project.id.clone(),
                provider: form.provider.clone(),
                model: form.model.clone(),
                capabilities: domain::ModelCapabilities {
                    streaming: form.streaming,
                    tools: form.tools,
                    context_tokens,
                },
            },
        )));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_masked_and_submitted_without_exposure() {
        let project: Project = serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.credential_forms.insert(
            "a".into(),
            CredentialForm {
                endpoint: "http://localhost".into(),
                secret: domain::SecretString("fake-sensitive-value".into()),
            },
        );
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900., 900.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let out = frame(&mut app, vec![]);
        assert!(!out.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("fake-sensitive-value"))));
        let pos = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text == "런타임 키 설정" => {
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
        let command = work.try_recv().unwrap().command;
        assert!(!format!("{command:?}").contains("fake-sensitive-value"));
        assert!(
            matches!(command, domain::Command::SetProviderCredential { project_id, .. } if project_id == "a")
        );
    }

    #[test]
    fn refreshed_model_replaces_only_untouched_form() {
        let mut project: Project = serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        project.model_settings = Some(domain::ModelSettings {
            provider: "mock".into(),
            model: "새 저장 모델".into(),
            capabilities: domain::ModelCapabilities {
                streaming: false,
                tools: false,
                context_tokens: None,
            },
        });
        app.state.project = Some(project);
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert_eq!(app.state.model_forms["a"].model, "새 저장 모델");
    }

    #[test]
    fn credential_error_is_redacted_and_input_is_retained() {
        let project: Project = serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.credential_forms.insert(
            "a".into(),
            CredentialForm {
                endpoint: "http://localhost".into(),
                secret: domain::SecretString("fake-sensitive-value".into()),
            },
        );
        app.state.credential_pending.insert("a".into());
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::Credential {
                project_id: "a".into(),
            },
            result: Err("fake-sensitive-value".into()),
        });
        assert!(!app.state.status.contains("fake-sensitive-value"));
        assert_eq!(
            app.state.credential_forms["a"].secret.0,
            "fake-sensitive-value"
        );
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!out.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("fake-sensitive-value"))));
    }

    #[test]
    fn model_failure_and_late_success_preserve_edits_and_project_selection() {
        let project: Project = serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = crate::App {
            state: crate::State {
                selected: Some("a".into()),
                project: Some(project.clone()),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        app.state.model_forms.get_mut("a").unwrap().model = "제출 모델".into();
        let submitted = app.state.model_forms["a"].clone();
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::ModelSettings {
                project_id: "a".into(),
                submitted: submitted.clone(),
            },
            result: Err("저장 실패".into()),
        });
        assert_eq!(app.state.model_forms["a"].model, "제출 모델");
        assert!(app.state.status.contains("저장 실패"));
        app.state.model_forms.get_mut("a").unwrap().model = "다음 편집".into();
        let mut other = project.clone();
        other.id = "b".into();
        app.state.select("b");
        app.state.project = Some(other);
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::ModelSettings {
                project_id: "a".into(),
                submitted,
            },
            result: Ok(domain::ResponseData::Project(project)),
        });
        assert_eq!(app.state.model_forms["a"].model, "다음 편집");
        assert_eq!(app.state.project.as_ref().unwrap().id, "b");
    }
}
