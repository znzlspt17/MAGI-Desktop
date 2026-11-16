use serde_json::Value;
#[derive(Clone, Debug)]
pub(crate) struct Diff {
    pub changes: Vec<Value>,
    pub revision: Option<String>,
    pub current_revision: Option<String>,
    pub spec_hash: Option<String>,
}
pub(crate) fn parse(data: &Value) -> Result<Diff, String> {
    let changes = data
        .get("changes")
        .and_then(Value::as_array)
        .ok_or("잘못된 변경 응답")?;
    if changes.iter().any(|c| {
        c.get("path").and_then(Value::as_str).is_none()
            || ["before", "after"]
                .iter()
                .any(|k| c.get(*k).is_none_or(|v| !v.is_null() && !v.is_string()))
    }) {
        return Err("잘못된 변경 응답".into());
    }
    Ok(Diff {
        changes: changes.clone(),
        revision: data["revision"].as_str().map(str::to_owned),
        current_revision: data["current_revision"].as_str().map(str::to_owned),
        spec_hash: data["spec_hash"].as_str().map(str::to_owned),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_preserves_multiline_and_absent_files_without_inventing_revision() {
        let data = serde_json::json!({"changes":[{"path":"src/a","before":null,"after":"첫 줄\n  둘째 줄"},{"path":"src/b","before":"old","after":null}]});
        let diff = parse(&data).unwrap();
        assert_eq!(diff.revision, None);
        assert_eq!(diff.current_revision, None);
        assert_eq!(diff.changes[0]["after"], "첫 줄\n  둘째 줄");
        assert!(diff.changes[1]["after"].is_null());
        assert!(parse(&serde_json::json!({"changes":"wrong"})).is_err());
    }
}
#[derive(Default)]
pub(crate) struct State {
    pub diff: Option<Diff>,
    pub error: Option<String>,
    pub project: Option<String>,
    pub run: Option<String>,
    pub sequence: u64,
    pub selection: u64,
    pub pending: bool,
}
fn text_rows(ui: &mut eframe::egui::Ui, id: impl std::hash::Hash, text: &str) {
    let lines: Vec<&str> = text.split('\n').collect();
    eframe::egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(240.0)
        .show_rows(ui, 20.0, lines.len(), |ui, range| {
            for index in range {
                ui.label(lines[index]);
            }
        });
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &State, detail: Option<&Value>) -> bool {
    ui.heading("변경 및 검증 근거");
    let request = ui
        .add_enabled(!state.pending, eframe::egui::Button::new("변경 조회"))
        .clicked();
    if let Some(error) = &state.error {
        ui.label(error);
    }
    if let Some(diff) = &state.diff {
        ui.label(format!(
            "기준 revision: {}",
            diff.revision.as_deref().unwrap_or("제공되지 않음")
        ));
        ui.label(format!(
            "현재 revision: {}",
            diff.current_revision.as_deref().unwrap_or("제공되지 않음")
        ));
        ui.label(format!(
            "명세 식별자: {}",
            diff.spec_hash.as_deref().unwrap_or("제공되지 않음")
        ));
        if diff.changes.is_empty() {
            ui.label("변경된 파일이 없습니다.");
        }
        for (index, change) in diff.changes.iter().enumerate() {
            ui.collapsing(change["path"].as_str().unwrap_or("경로 미제공"), |ui| {
                for key in ["before", "after"] {
                    ui.label(if key == "before" {
                        "이전"
                    } else {
                        "변경 후"
                    });
                    if let Some(text) = change[key].as_str() {
                        text_rows(ui, (index, key), text);
                    } else {
                        ui.label("파일 없음");
                    }
                }
            });
        }
    }
    if let Some(items) = detail.and_then(|v| v["evidence"].as_array()) {
        ui.label("검증 근거");
        if items.is_empty() {
            ui.label("검증 근거가 없습니다.");
        }
        for item in items {
            ui.group(|ui| {
                ui.label(format!(
                    "명령: {}",
                    item["command"].as_str().unwrap_or("제공되지 않음")
                ));
                ui.label(format!(
                    "결과: {}",
                    match item["passed"].as_bool() {
                        Some(true) => "통과",
                        Some(false) => "실패",
                        None => "제공되지 않음",
                    }
                ));
                ui.label(format!("요구사항: {}", item["requirement_ids"]));
                ui.label(format!("승인 기준: {}", item["acceptance_ids"]));
                ui.label(format!(
                    "revision: {}",
                    item["revision"].as_str().unwrap_or("제공되지 않음")
                ));
                ui.label(format!(
                    "검증 ID: {}",
                    item["verification_id"].as_str().unwrap_or("제공되지 않음")
                ));
                ui.label("종료 코드: 제공되지 않음");
                ui.label("시도 번호: 제공되지 않음");
                if let Some(output) = item["output"].as_str() {
                    ui.label(output);
                }
            });
        }
    }
    request
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn evidence_displays_only_reported_outcome_and_requirement_links() {
        let detail = serde_json::json!({"evidence":[{"command":"read_contains:src/a:ok","passed":true,"output":"matched","requirement_ids":["req-1"],"acceptance_ids":["ac-1"]}]});
        let out = eframe::egui::Context::default().run(eframe::egui::RawInput::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &State::default(), Some(&detail));
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
            "변경 조회",
            "read_contains:src/a:ok",
            "통과",
            "req-1",
            "ac-1",
            "종료 코드: 제공되지 않음",
        ] {
            assert!(labels.iter().any(|v| v.contains(text)), "{text}");
        }
    }
}
impl State {
    pub fn bind_selection(&mut self, project: &str, run: &str, selection: u64) {
        if self.selection != selection {
            self.project = None;
            self.selection = selection;
        }
        self.bind(project, run);
    }
    pub fn bind(&mut self, project: &str, run: &str) {
        if self.project.as_deref() != Some(project) || self.run.as_deref() != Some(run) {
            self.project = Some(project.into());
            self.run = Some(run.into());
            self.sequence = self.sequence.wrapping_add(1);
            self.diff = None;
            self.error = None;
            self.pending = false;
        }
    }
}
#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn selecting_same_run_again_invalidates_inflight_result() {
        let mut state = State::default();
        state.bind_selection("a", "r", 1);
        state.pending = true;
        state.bind_selection("a", "r", 2);
        assert!(!state.pending);
    }
    #[test]
    fn changing_run_and_returning_discards_old_result_and_changes_generation() {
        let mut state = State::default();
        state.bind("a", "r1");
        let old = state.sequence;
        state.diff = Some(parse(&serde_json::json!({"changes":[]})).unwrap());
        state.bind("a", "r2");
        state.bind("a", "r1");
        assert!(state.diff.is_none());
        assert!(state.sequence > old);
    }
}
