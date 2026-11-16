use domain::Project;
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pending {
    pub id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub call_id: serde_json::Value,
    pub raw: serde_json::Value,
}
pub(crate) fn pending(
    project: &Project,
    run: &serde_json::Value,
    selected: &str,
) -> Option<Pending> {
    if run["id"] != selected
        || run["project_id"] != project.id
        || run["status"] != "awaiting_approval"
    {
        return None;
    }
    let item = run.get("pending")?.as_object()?;
    let id = item.get("id")?.as_str()?.to_owned();
    let tool = item.get("tool")?.as_str()?.to_owned();
    if id.is_empty() || !["patch", "mcp"].contains(&tool.as_str()) {
        return None;
    }
    let arguments = item.get("arguments")?.clone();
    if !arguments.is_object() {
        return None;
    }
    Some(Pending {
        id,
        tool,
        arguments,
        call_id: item
            .get("call_id")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        raw: run["pending"].clone(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_preserves_exact_request_and_only_selected_project_run() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let mut run = serde_json::json!({"id":"r","project_id":"a","status":"awaiting_approval","pending":{"id":"approval","tool":"patch","arguments":{"path":"src/a","content":"  첫 줄\n둘째 줄  "},"call_id":"c"}});
        let v = pending(&p, &run, "r").unwrap();
        assert_eq!(v.raw, run["pending"]);
        assert_eq!(v.id, "approval");
        assert_eq!(v.arguments["content"], "  첫 줄\n둘째 줄  ");
        assert!(pending(&p, &run, "other").is_none());
        run["project_id"] = "b".into();
        assert!(pending(&p, &run, "r").is_none());
        run["project_id"] = "a".into();
        run["status"] = "executing".into();
        assert!(pending(&p, &run, "r").is_none());
    }
}
pub(crate) fn approval_matches(project: &Project, run: &serde_json::Value) -> bool {
    project.approved.as_ref().is_some_and(|spec| {
        project.draft_hash == spec.hash
            && run["spec_hash"] == spec.hash
            && run["spec_version"].as_u64() == Some(spec.version)
            && run["scope"] == serde_json::json!(spec.scope)
    })
}
#[cfg(test)]
mod approval_tests {
    use super::*;
    #[test]
    fn approval_requires_current_saved_spec_id_version_and_scope() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":["src"],"requirements":[],"acceptance":[]}})).unwrap();
        let mut run = serde_json::json!({"spec_hash":"h","spec_version":1,"scope":["src"]});
        assert!(approval_matches(&p, &run));
        run["scope"] = serde_json::json!(["other"]);
        assert!(!approval_matches(&p, &run));
        run["scope"] = serde_json::json!(["src"]);
        run["spec_version"] = 2.into();
        assert!(!approval_matches(&p, &run));
    }
}
pub(crate) fn render(
    ui: &mut eframe::egui::Ui,
    project: &Project,
    run: &serde_json::Value,
    selected: &str,
    busy: bool,
) -> Option<(Pending, bool)> {
    let item = pending(project, run, selected)?;
    ui.group(|ui| {
        ui.heading("도구 실행 승인 요청");
        ui.label(format!("요청 ID: {}", item.id));
        ui.label(format!("도구: {}", item.tool));
        ui.label(format!("호출 ID: {}", item.call_id));
        ui.label("작업 ID는 제공되지 않았습니다.");
        ui.label(format!("실행: {}", selected));
        ui.label(format!("실행 명세: {}", run["spec_hash"]));
        ui.label(format!("승인 범위: {}", run["scope"]));
        ui.label(format!(
            "실행 작업 공간: {}",
            run["worktree"].as_str().unwrap_or("제공되지 않음")
        ));
        ui.label("요청 인수 전체");
        ui.label(serde_json::to_string_pretty(&item.arguments).unwrap_or_default());
        if let Some(content) = item.arguments.get("content").and_then(|v| v.as_str()) {
            ui.label("변경할 내용");
            ui.label(content);
        }
        let matches = approval_matches(project, run);
        let active = tool_active_permitted(&item, run);
        if item.tool == "patch"
            && !active
            && !item
                .raw
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty())
        {
            ui.label("요청 연결 정보 확인 필요");
        } else if item.tool == "patch" && !active {
            match run["agent_active"].as_bool() {
                Some(false) => {
                    ui.label("실행을 재개한 후 승인할 수 있습니다.");
                }
                _ => {
                    ui.label("에이전트 상태 확인 필요");
                }
            }
        }
        if !matches {
            ui.label("현재 승인된 명세와 실행 범위가 다릅니다. 승인은 사용할 수 없습니다.");
        }
        let mut result = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy && matches && active,
                    eframe::egui::Button::new("승인"),
                )
                .clicked()
            {
                result = Some((item.clone(), true));
            }
            if ui
                .add_enabled(!busy, eframe::egui::Button::new("거절"))
                .clicked()
            {
                result = Some((item.clone(), false));
            }
        });
        result
    })
    .inner
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn displays_full_pending_request_and_run_context() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap();
        let r = serde_json::json!({"id":"r","project_id":"a","status":"awaiting_approval","spec_hash":"h","scope":["src"],"worktree":"C:/work","pending":{"id":"id1","tool":"patch","arguments":{"path":"src/a","content":"첫 줄\n둘째 줄"},"call_id":"call1"}});
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &p, &r, "r", false);
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
        for fragment in [
            "id1",
            "patch",
            "src/a",
            "첫 줄\n둘째 줄",
            "C:/work",
            "src",
            "call1",
            "작업 ID는 제공되지 않았습니다.",
            "승인",
            "거절",
        ] {
            assert!(labels.iter().any(|s| s.contains(fragment)), "{fragment}");
        }
    }
}
pub(crate) fn tool_active_permitted(item: &Pending, run: &serde_json::Value) -> bool {
    if item.tool != "patch" {
        return true;
    }
    match item.raw.get("call_id") {
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(id)) if !id.is_empty() => {
            run["agent_active"].as_bool() == Some(true)
        }
        _ => false,
    }
}
#[cfg(test)]
mod active_tests {
    use super::*;
    #[test]
    fn patch_requires_explicit_active_agent_but_mcp_does_not() {
        let mut item = Pending {
            id: "id".into(),
            tool: "patch".into(),
            arguments: serde_json::json!({}),
            call_id: serde_json::Value::Null,
            raw: serde_json::json!({"call_id":"call"}),
        };
        assert!(!tool_active_permitted(
            &item,
            &serde_json::json!({"agent_active":false})
        ));
        assert!(!tool_active_permitted(&item, &serde_json::json!({})));
        assert!(tool_active_permitted(
            &item,
            &serde_json::json!({"agent_active":true})
        ));
        item.tool = "mcp".into();
        assert!(tool_active_permitted(
            &item,
            &serde_json::json!({"agent_active":false})
        ));
        assert!(tool_active_permitted(&item, &serde_json::json!({})));
    }
}
#[cfg(test)]
mod inactive_display_tests {
    use super::*;
    #[test]
    fn inactive_or_missing_patch_state_is_explained_without_blocking_mcp() {
        let p:Project=serde_json::from_value(serde_json::json!({"id":"a","name":"a","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":[],"requirements":[],"acceptance":[]}})).unwrap();
        for (active, expected) in [
            (Some(false), "실행을 재개한 후 승인"),
            (None, "에이전트 상태 확인 필요"),
        ] {
            let mut r = serde_json::json!({"id":"r","project_id":"a","status":"awaiting_approval","spec_hash":"h","spec_version":1,"scope":[],"pending":{"id":"id","tool":"patch","arguments":{},"call_id":"call"}});
            if let Some(value) = active {
                r["agent_active"] = value.into();
            }
            let out =
                eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
                    eframe::egui::CentralPanel::default().show(ctx, |ui| {
                        render(ui, &p, &r, "r", false);
                    });
                });
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,eframe::egui::epaint::Shape::Text(t) if t.galley.job.text.contains(expected))),"{expected}");
        }
    }
}
#[cfg(test)]
mod bound_shape_tests {
    use super::*;
    #[test]
    fn bound_unbound_and_unknown_call_identity_are_distinct() {
        let mut p = Pending {
            id: "i".into(),
            tool: "patch".into(),
            arguments: serde_json::json!({}),
            call_id: serde_json::Value::Null,
            raw: serde_json::json!({"call_id":null}),
        };
        assert!(tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":false})
        ));
        p.raw = serde_json::json!({"call_id":"call"});
        assert!(!tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":false})
        ));
        assert!(tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":true})
        ));
        p.raw = serde_json::json!({});
        assert!(!tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":true})
        ));
        p.raw = serde_json::json!({"call_id":42});
        assert!(!tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":true})
        ));
        p.raw = serde_json::json!({"call_id":""});
        assert!(!tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":true})
        ));
        p.tool = "mcp".into();
        assert!(tool_active_permitted(
            &p,
            &serde_json::json!({"agent_active":false})
        ));
    }
}
