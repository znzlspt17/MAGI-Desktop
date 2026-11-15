use eframe::egui;

#[derive(Clone)]
pub(crate) struct Form {
    draft: domain::SpecDraft,
    title: String,
    revision: String,
    scope: String,
    requirements: String,
    acceptance: String,
    questions: String,
    failures: String,
    optional: String,
    markdown: String,
    constraints: String,
    sources: String,
    reused: String,
    tasks: String,
    bindings: String,
    accepted: std::collections::HashSet<String>,
}
impl Form {
    fn new(project: &domain::Project) -> Self {
        let d = &project.draft;
        Self {
            draft: d.clone(),
            title: d.title.clone(),
            revision: d.base_revision.clone(),
            scope: d.scope.join("\n"),
            requirements: d.requirements.join("\n"),
            acceptance: d.acceptance.join("\n"),
            questions: d.mandatory_questions.join("\n"),
            failures: d.required_failures.join("\n"),
            optional: d.optional_issues.join("\n"),
            markdown: d.markdown.clone(),
            constraints: d.constraints.join("\n"),
            sources: d.source_links.join("\n"),
            reused: d.reused_decisions.join("\n"),
            tasks: serde_json::to_string_pretty(&d.tasks).unwrap_or_default(),
            bindings: serde_json::to_string_pretty(&d.task_bindings).unwrap_or_default(),
            accepted: Default::default(),
        }
    }
    fn draft(&self) -> Result<domain::SpecDraft, String> {
        fn lines(s: &str) -> Vec<String> {
            s.lines()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect()
        }
        let mut d = self.draft.clone();
        d.title = self.title.clone();
        d.base_revision = self.revision.clone();
        d.scope = lines(&self.scope);
        d.requirements = lines(&self.requirements);
        d.acceptance = lines(&self.acceptance);
        d.mandatory_questions = lines(&self.questions);
        d.required_failures = lines(&self.failures);
        d.optional_issues = lines(&self.optional);
        d.markdown = self.markdown.clone();
        d.constraints = lines(&self.constraints);
        d.source_links = lines(&self.sources);
        d.reused_decisions = lines(&self.reused);
        d.tasks = serde_json::from_str(&self.tasks).map_err(|e| format!("작업 JSON: {e}"))?;
        d.task_bindings =
            serde_json::from_str(&self.bindings).map_err(|e| format!("작업 바인딩 JSON: {e}"))?;
        Ok(d)
    }
    pub(crate) fn acknowledge_saved(&mut self, draft: &domain::SpecDraft) {
        self.draft = draft.clone();
        self.accepted.clear();
    }
}
pub(crate) enum Action {
    Save(Result<domain::SpecDraft, String>),
    Approve(Vec<String>),
}
pub(crate) fn render(
    ui: &mut egui::Ui,
    project: &domain::Project,
    forms: &mut std::collections::HashMap<String, Form>,
    pending: bool,
) -> Option<Action> {
    let form = forms
        .entry(project.id.clone())
        .or_insert_with(|| Form::new(project));
    let edited = form.draft().ok().and_then(|d| serde_json::to_value(d).ok());
    let previous = serde_json::to_value(&form.draft).ok();
    let current = serde_json::to_value(&project.draft).ok();
    if edited == previous && previous != current {
        *form = Form::new(project);
    } else if edited.is_some() && edited == current {
        form.draft = project.draft.clone();
    }
    let conflict = serde_json::to_value(&form.draft).ok() != current;
    if conflict {
        ui.label("저장된 초안이 변경되어 현재 편집을 저장할 수 없습니다. 내용을 따로 보존한 뒤 저장된 초안을 불러오세요.");
        if ui.button("저장된 초안 불러오기 (편집 내용 교체)").clicked() {
            *form = Form::new(project);
        }
    }
    let save = ui
        .add_enabled(!pending && !conflict, egui::Button::new("초안 저장"))
        .clicked();
    let clean = form.draft().ok().and_then(|d| serde_json::to_value(d).ok())
        == serde_json::to_value(&project.draft).ok();
    let assessed = project.assessment.as_ref().is_none_or(|a| {
        a.draft_hash == project.draft_hash && a.context_version == project.context_version
    });
    let valid = project.draft.mandatory_questions.is_empty()
        && project.draft.required_failures.is_empty()
        && project
            .draft
            .optional_issues
            .iter()
            .all(|item| form.accepted.contains(item))
        && assessed;
    let approve = ui
        .add_enabled(!pending && clean && valid, egui::Button::new("명세 승인"))
        .clicked();
    if project
        .approved
        .as_ref()
        .is_some_and(|a| a.hash == project.draft_hash)
    {
        ui.label("저장된 명세 승인됨");
    } else {
        ui.label("저장된 명세 미승인");
    }
    if !clean {
        ui.label("저장되지 않은 편집이 있습니다.");
    }
    for issue in &project.draft.optional_issues {
        let mut accepted = form.accepted.contains(issue);
        if ui
            .checkbox(&mut accepted, format!("선택 항목 수락: {issue}"))
            .changed()
        {
            if accepted {
                form.accepted.insert(issue.clone());
            } else {
                form.accepted.remove(issue);
            }
        }
    }
    ui.label("제목");
    ui.text_edit_singleline(&mut form.title);
    for (label, value) in [
        ("기준 revision", &mut form.revision),
        ("범위", &mut form.scope),
        ("요구사항", &mut form.requirements),
        ("승인 기준", &mut form.acceptance),
        ("필수 질문", &mut form.questions),
        ("필수 검사 실패", &mut form.failures),
        ("선택 항목", &mut form.optional),
        ("명세 원문", &mut form.markdown),
        ("제약", &mut form.constraints),
        ("출처", &mut form.sources),
        ("재사용 결정", &mut form.reused),
        ("작업 JSON", &mut form.tasks),
        ("바인딩 JSON", &mut form.bindings),
    ] {
        ui.label(label);
        ui.text_edit_multiline(value);
    }
    if save {
        Some(Action::Save(form.draft()))
    } else if approve {
        Some(Action::Approve(project.draft.optional_issues.clone()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project() -> domain::Project {
        serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"old","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap()
    }
    #[test]
    fn newly_loaded_draft_updates_unedited_form() {
        let ctx = egui::Context::default();
        let mut forms = std::collections::HashMap::new();
        let mut p = project();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &p, &mut forms, false);
            });
        });
        p.draft.title = "new".into();
        p.draft_hash = "new-hash".into();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &p, &mut forms, false);
            });
        });
        assert_eq!(forms["a"].title, "new");
    }
    #[test]
    fn stale_edit_cannot_replace_newer_draft() {
        let ctx = egui::Context::default();
        let mut forms = std::collections::HashMap::new();
        let mut p = project();
        let frame =
            |p: &domain::Project, forms: &mut std::collections::HashMap<String, Form>, events| {
                let mut action = None;
                let out = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(800., 1600.),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            action = render(ui, p, forms, false);
                        });
                    },
                );
                (out, action)
            };
        frame(&p, &mut forms, vec![]);
        forms.get_mut("a").unwrap().title = "내 편집".into();
        p.draft.title = "다른 새 버전".into();
        p.draft_hash = "new".into();
        let (out, _) = frame(&p, &mut forms, vec![]);
        let pos = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text == "초안 저장" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        frame(
            &p,
            &mut forms,
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
        let (_, action) = frame(
            &p,
            &mut forms,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(action.is_none());
        assert_eq!(forms["a"].title, "내 편집");
    }
    #[test]
    fn edits_made_during_save_remain_saveable() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let p = project();
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
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 2500.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let out = frame(&mut app, vec![]);
        let pos = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.job.text == "초안 저장" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        app.state.spec_forms.get_mut("a").unwrap().title = "제출".into();
        let click = |app: &mut crate::App| {
            frame(
                app,
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
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        };
        click(&mut app);
        let sent = work.try_recv().unwrap();
        app.state.spec_forms.get_mut("a").unwrap().title = "새 편집".into();
        let mut saved = project();
        saved.draft.title = "제출".into();
        saved.draft_hash = "saved".into();
        app.apply(crate::worker::Reply {
            kind: sent.kind,
            result: Ok(domain::ResponseData::Project(saved)),
        });
        click(&mut app);
        assert!(
            matches!(work.try_recv().unwrap().command,domain::Command::SaveDraft{draft,..} if draft.title=="새 편집")
        );
    }
    #[test]
    fn approval_for_another_draft_is_not_shown_as_current() {
        let mut p = project();
        p.approved=Some(serde_json::from_value(serde_json::json!({"version":1,"hash":"old-hash","base_revision":"r","scope":[],"requirements":[],"acceptance":[]})).unwrap());
        let ctx = egui::Context::default();
        let mut forms = std::collections::HashMap::new();
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &p, &mut forms, false);
            });
        });
        assert!(!out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text=="저장된 명세 승인됨")));
    }
    #[test]
    fn approval_is_blocked_by_edits_pending_and_required_items() {
        for case in ["edited", "pending", "question", "failure", "optional"] {
            let ctx = egui::Context::default();
            let mut forms = std::collections::HashMap::new();
            let mut p = project();
            if case == "question" {
                p.draft.mandatory_questions.push("질문".into());
            }
            if case == "failure" {
                p.draft.required_failures.push("검사".into());
            }
            if case == "optional" {
                p.draft.optional_issues.push("선택".into());
            }
            let frame = |forms: &mut std::collections::HashMap<String, Form>, events| {
                let mut action = None;
                let out = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(800., 1600.),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            action = render(ui, &p, forms, case == "pending");
                        });
                    },
                );
                (out, action)
            };
            frame(&mut forms, vec![]);
            if case == "edited" {
                forms.get_mut("a").unwrap().title = "미저장".into();
            }
            let (out, _) = frame(&mut forms, vec![]);
            let pos = out
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::epaint::Shape::Text(t) if t.galley.job.text == "명세 승인" => {
                        Some(t.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .unwrap();
            frame(
                &mut forms,
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
            let (_, action) = frame(
                &mut forms,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert!(action.is_none(), "{case}");
        }
    }
    #[test]
    fn invalid_json_and_save_failure_keep_edit() {
        let p = project();
        let (tx, _work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
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
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let form = app.state.spec_forms.get_mut("a").unwrap();
        form.title = "미저장 편집".into();
        form.tasks = "[broken".into();
        assert!(form.draft().is_err());
        app.state.spec_review_pending.insert("a".into());
        app.apply(crate::worker::Reply {
            kind: crate::worker::Kind::SaveDraft {
                project_id: "a".into(),
                base_hash: "h".into(),
                context: 0,
                generation: 0,
            },
            result: Err("저장 실패".into()),
        });
        assert_eq!(app.state.spec_forms["a"].title, "미저장 편집");
        assert_eq!(app.state.spec_forms["a"].tasks, "[broken");
        assert!(app.state.status.contains("저장 실패"));
    }
}
