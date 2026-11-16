#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Project,
    Run(String),
}
#[derive(Default)]
pub(crate) struct Form {
    pub project_destination: String,
    pub run_destination: String,
    pub result: Option<serde_json::Value>,
    pub result_target: Option<Target>,
    pub result_destination: String,
}
#[derive(Default)]
pub(crate) struct State {
    pub forms: std::collections::HashMap<String, Form>,
    pub pending: std::collections::HashSet<String>,
}
impl State {
    pub fn form(&mut self, project: &str) -> &mut Form {
        self.forms.entry(project.into()).or_default()
    }
}
pub(crate) fn command(
    project: &str,
    target: &Target,
    destination: &str,
) -> Option<domain::Command> {
    if destination.trim().is_empty() {
        return None;
    }
    Some(match target {
        Target::Project => domain::Command::ExportPackage {
            project_id: project.into(),
            destination: destination.into(),
        },
        Target::Run(run_id) if !run_id.is_empty() => domain::Command::ExportRunPackage {
            run_id: run_id.clone(),
            destination: destination.into(),
        },
        Target::Run(_) => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_uses_only_explicit_destination_and_correct_contract() {
        assert!(
            matches!(command("a",&Target::Project,"C:/out.zip"),Some(domain::Command::ExportPackage{project_id,destination}) if project_id=="a" && destination=="C:/out.zip")
        );
        assert!(
            matches!(command("a",&Target::Run("r1".into()),"C:/run"),Some(domain::Command::ExportRunPackage{run_id,destination}) if run_id=="r1" && destination=="C:/run")
        );
        assert!(command("a", &Target::Project, "   ").is_none());
        let mut state = State::default();
        state.form("a").project_destination = "a".into();
        state.form("b").project_destination = "b".into();
        assert_eq!(state.form("a").project_destination, "a");
    }
}
pub(crate) fn render(
    ui: &mut eframe::egui::Ui,
    form: &mut Form,
    run: Option<&str>,
    busy: bool,
) -> Option<Target> {
    ui.heading("자료 내보내기");
    ui.label("현재 승인 명세를 내보냅니다. 과거 이력 선택과 무관합니다.");
    ui.label("명세 저장 경로");
    ui.text_edit_singleline(&mut form.project_destination);
    let mut action = ui
        .add_enabled(
            !busy && !form.project_destination.trim().is_empty(),
            eframe::egui::Button::new("현재 승인 명세 내보내기"),
        )
        .clicked()
        .then_some(Target::Project);
    if let Some(id) = run {
        ui.label(format!("선택 실행: {id}"));
        ui.label("실행 당시 승인 사본을 사용하며 일부 근거만 포함될 수 있습니다.");
        ui.label("실행 저장 경로");
        ui.text_edit_singleline(&mut form.run_destination);
        if ui
            .add_enabled(
                !busy && !form.run_destination.trim().is_empty(),
                eframe::egui::Button::new("선택 실행 묶음 내보내기"),
            )
            .clicked()
        {
            action = Some(Target::Run(id.into()));
        }
    }
    if let Some((manifest, target)) = form
        .result
        .as_ref()
        .zip(form.result_target.as_ref())
        .filter(|(_, target)| {
            matches!(target, Target::Project) || matches!(target,Target::Run(id) if run==Some(id))
        })
    {
        ui.label(match target {
            Target::Project => "완료 대상: 요청 당시 승인 명세".to_owned(),
            Target::Run(id) => format!("완료 대상 실행: {id}"),
        });
        ui.label(format!("요청 경로: {}", form.result_destination));
        show_manifest(ui, manifest);
    }
    action
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn clearly_distinguishes_current_project_from_selected_run() {
        let mut form = Form::default();
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut form, Some("r1"), false);
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
        for v in [
            "현재 승인 명세 내보내기",
            "선택 실행 묶음 내보내기",
            "과거 이력 선택과 무관",
            "일부 근거만 포함될 수 있습니다.",
            "r1",
        ] {
            assert!(labels.iter().any(|s| s.contains(v)), "{v}");
        }
    }
}
pub(crate) fn show_manifest(ui: &mut eframe::egui::Ui, manifest: &serde_json::Value) {
    ui.label("엔진이 반환한 내보내기 정보");
    ui.label("출처 검증 여부는 제공되지 않았습니다.");
    ui.label(serde_json::to_string_pretty(manifest).unwrap_or_default());
}
#[cfg(test)]
mod manifest_tests {
    use super::*;
    #[test]
    fn returned_manifest_status_and_warnings_are_visible_without_claiming_provenance() {
        let manifest = serde_json::json!({"completed_id":"h","completeness":"limited","test_status":"not_collected","warnings":["미수집"],"source_hash":"fake"});
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                show_manifest(ui, &manifest);
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
        for v in [
            "limited",
            "not_collected",
            "미수집",
            "출처 검증 여부는 제공되지 않았습니다.",
        ] {
            assert!(labels.iter().any(|s| s.contains(v)), "{v}");
        }
        assert!(!labels.iter().any(|s| s.contains("출처 증명")));
    }
}
#[cfg(test)]
mod result_display_tests {
    use super::*;
    #[test]
    fn completed_manifest_keeps_original_target_and_destination() {
        let mut form = Form {
            result: Some(
                serde_json::json!({"completeness":"limited","test_status":"not_collected"}),
            ),
            result_target: Some(Target::Run("r".into())),
            result_destination: "C:/fake.zip".into(),
            run_destination: "edited".into(),
            ..Default::default()
        };
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut form, Some("r"), false);
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
        for v in ["C:/fake.zip", "limited", "not_collected"] {
            assert!(labels.iter().any(|s| s.contains(v)), "{v}");
        }
    }
}
#[cfg(test)]
mod stale_display_tests {
    use super::*;
    #[test]
    fn run_result_is_hidden_when_another_run_is_selected() {
        let mut form = Form {
            result: Some(serde_json::json!({"completeness":"limited"})),
            result_target: Some(Target::Run("old".into())),
            result_destination: "C:/old.zip".into(),
            ..Default::default()
        };
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut form, Some("new"), false);
            });
        });
        assert!(!out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::epaint::Shape::Text(t) if t.galley.job.text.contains("C:/old.zip"))));
    }
}
