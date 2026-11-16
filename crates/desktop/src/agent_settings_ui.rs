use domain::AgentConfig;
pub(crate) fn parse_agents(text: &str) -> Result<Vec<AgentConfig>, String> {
    serde_json::from_str(text).map_err(|_| "에이전트 설정 형식이 올바르지 않습니다.".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_explicit_models_tools_and_limits_without_guessing_capabilities() {
        let text = r#"[{"role":"reviewer","model":"example","tools":["read","search"],"limits":{"max_rounds":8,"max_tokens":100,"max_output_tokens":50}}]"#;
        let agents = parse_agents(text).unwrap();
        assert_eq!(agents[0].model, "example");
        assert_eq!(agents[0].tools, vec!["read", "search"]);
        assert_eq!(agents[0].limits["max_output_tokens"], 50);
        assert!(parse_agents("bad json").is_err());
    }
}
#[derive(Default)]
pub(crate) struct State {
    forms: std::collections::HashMap<String, Form>,
    pub pending: std::collections::HashSet<String>,
    pub refresh: std::collections::HashSet<String>,
    pub refreshing: std::collections::HashSet<String>,
}
#[derive(Default)]
struct Form {
    agents: String,
    mode: String,
}
impl State {
    pub fn blocked(&self, id: &str) -> bool {
        self.pending.contains(id) || self.refresh.contains(id)
    }
    fn form(&mut self, project: &domain::Project) -> &mut Form {
        self.forms
            .entry(project.id.clone())
            .or_insert_with(|| Form {
                agents: serde_json::to_string_pretty(&project.agent_configs).unwrap_or_default(),
                mode: project.autonomy_mode.clone(),
            })
    }
}
#[cfg(test)]
mod state_tests {
    use super::*;
    fn project(id: &str, mode: &str) -> domain::Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null,"agent_configs":[],"autonomy_mode":mode})).unwrap()
    }
    #[test]
    fn approvals_and_execution_block_until_refresh_succeeds() {
        let mut state = State::default();
        assert!(!state.blocked("a"));
        state.pending.insert("a".into());
        assert!(state.blocked("a"));
        assert!(!state.blocked("b"));
        state.pending.clear();
        state.refresh.insert("a".into());
        assert!(state.blocked("a"));
        state.refresh.clear();
        assert!(!state.blocked("a"));
    }
    #[test]
    fn forms_preserve_project_edits_and_initialize_from_server() {
        let mut state = State::default();
        let a = project("a", "step");
        let b = project("b", "approved_scope");
        assert_eq!(state.form(&a).mode, "step");
        state.form(&a).agents = "사용자 편집".into();
        assert_eq!(state.form(&b).mode, "approved_scope");
        assert_eq!(state.form(&a).agents, "사용자 편집");
    }
}
pub(crate) enum Action {
    Agents(Result<Vec<AgentConfig>, String>),
    Mode(String),
    Refresh,
}
pub(crate) fn render(
    ui: &mut eframe::egui::Ui,
    state: &mut State,
    project: &domain::Project,
) -> Option<Action> {
    let blocked = state.blocked(&project.id);
    let needs_refresh = state.refresh.contains(&project.id);
    let form = state.form(project);
    let mut action = None;
    ui.heading("역할별 모델, 도구와 한도");
    if ui.button("기본 역할 양식 만들기").clicked() {
        let model = project
            .model_settings
            .as_ref()
            .map(|v| v.model.as_str())
            .unwrap_or("");
        if let Ok(text) = serde_json::to_string_pretty(&default_agents(model)) {
            form.agents = text;
        }
    }
    if let Ok(mut agents) = parse_agents(&form.agents) {
        let mut changed = false;
        for agent in &mut agents {
            ui.group(|ui| {
                ui.label(format!("역할: {}", agent.role));
                ui.horizontal(|ui| {
                    ui.label("모델");
                    changed |= ui.text_edit_singleline(&mut agent.model).changed();
                });
                for tool in ["read", "search", "patch", "exec", "git", "mcp"] {
                    let mut selected = agent.tools.iter().any(|v| v == tool);
                    if ui.checkbox(&mut selected, tool).changed() {
                        changed = true;
                        agent.tools.retain(|v| v != tool);
                        if selected {
                            agent.tools.push(tool.into());
                        }
                    }
                }
                for field in ["max_rounds", "max_tokens", "max_output_tokens"] {
                    let mut value = agent
                        .limits
                        .get(field)
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    ui.horizontal(|ui| {
                        ui.label(field);
                        if ui.add(eframe::egui::DragValue::new(&mut value)).changed() {
                            agent.limits[field] = value.into();
                            changed = true;
                        }
                    });
                }
            });
        }
        if changed {
            if let Ok(text) = serde_json::to_string_pretty(&agents) {
                form.agents = text;
            }
        }
    }
    ui.label("설정 JSON");
    ui.text_edit_multiline(&mut form.agents);
    if ui
        .add_enabled(!blocked, eframe::egui::Button::new("에이전트 설정 저장"))
        .clicked()
    {
        action = Some(Action::Agents(parse_agents(&form.agents)));
    }
    ui.horizontal(|ui| {
        ui.label("자율성");
        for mode in ["step", "approved_scope"] {
            ui.selectable_value(&mut form.mode, mode.to_owned(), mode);
        }
    });
    if ui
        .add_enabled(!blocked, eframe::egui::Button::new("자율성 저장"))
        .clicked()
    {
        action = Some(Action::Mode(form.mode.clone()));
    }
    if needs_refresh {
        ui.label("변경된 프로젝트 상태 확인이 필요합니다.");
        if ui.button("프로젝트 상태 다시 확인").clicked() {
            action = Some(Action::Refresh);
        }
    }
    action
}
#[cfg(test)]
mod render_tests {
    use super::*;
    #[test]
    fn settings_screen_has_explicit_save_controls_and_modes() {
        let project:domain::Project=serde_json::from_value(serde_json::json!({"id":"p","name":"p","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null,"agent_configs":[],"autonomy_mode":"step"})).unwrap();
        let mut state = State::default();
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, &project);
            });
        });
        for label in [
            "기본 역할 양식 만들기",
            "에이전트 설정 저장",
            "step",
            "approved_scope",
            "자율성 저장",
        ] {
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::epaint::Shape::Text(t) if t.galley.job.text==label)),"{label}");
        }
    }
}
fn default_agents(model: &str) -> Vec<AgentConfig> {
    ["coordinator", "implementer", "reviewer"]
        .iter()
        .map(|role| AgentConfig {
            role: (*role).into(),
            model: model.into(),
            tools: vec!["read".into(), "search".into()],
            limits: serde_json::json!({"max_rounds":8,"max_tokens":32768}),
        })
        .collect()
}
#[cfg(test)]
mod defaults_tests {
    use super::*;
    #[test]
    fn default_form_uses_explicit_selected_model_and_original_roles() {
        let agents = default_agents("명시 모델");
        assert_eq!(
            agents.iter().map(|a| a.role.as_str()).collect::<Vec<_>>(),
            vec!["coordinator", "implementer", "reviewer"]
        );
        assert!(agents.iter().all(|a| a.model == "명시 모델"
            && a.tools == vec!["read", "search"]
            && a.limits["max_rounds"] == 8
            && a.limits["max_tokens"] == 32768));
    }
}
