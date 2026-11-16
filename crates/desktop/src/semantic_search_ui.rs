use serde_json::Value;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Semantic,
    TextFallback,
}
#[derive(Clone, Debug)]
pub(crate) struct ResultPage {
    pub mode: Mode,
    pub documents: Vec<Value>,
    pub hits: Vec<Value>,
}
pub(crate) fn parse(data: &Value) -> Result<ResultPage, String> {
    let mode = match data["mode"].as_str() {
        Some("semantic") => Mode::Semantic,
        Some("text_fallback") => Mode::TextFallback,
        _ => return Err("검색 방식 응답이 올바르지 않습니다.".into()),
    };
    let hits = data["results"]
        .as_array()
        .ok_or("검색 결과 응답이 올바르지 않습니다.")?;
    let mut documents = Vec::new();
    for hit in hits {
        let doc = match mode {
            Mode::Semantic => hit.get("document"),
            Mode::TextFallback => Some(hit),
        }
        .ok_or("검색 문서 응답이 올바르지 않습니다.")?;
        if doc["id"].as_str().is_none()
            || doc["source"].as_str().is_none()
            || doc["version"].as_u64().is_none()
            || doc["text"].as_str().is_none()
        {
            return Err("검색 문서 응답이 올바르지 않습니다.".into());
        }
        documents.push(doc.clone());
    }
    Ok(ResultPage {
        mode,
        documents,
        hits: hits.clone(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_and_fallback_modes_preserve_documents_without_relabeling() {
        let doc = serde_json::json!({"id":"d","version":1,"source":"s","text":"첫 줄\n둘째 줄"});
        let semantic =
            parse(&serde_json::json!({"mode":"semantic","results":[{"document":doc,"score":0.7}]}))
                .unwrap();
        assert_eq!(semantic.mode, Mode::Semantic);
        assert_eq!(semantic.documents[0]["id"], "d");
        let fallback = parse(&serde_json::json!({"mode":"text_fallback","results":[doc]})).unwrap();
        assert_eq!(fallback.mode, Mode::TextFallback);
        assert_eq!(fallback.documents[0]["text"], "첫 줄\n둘째 줄");
        assert!(parse(&serde_json::json!({"mode":"unknown","results":[]})).is_err());
    }
}
#[derive(Default)]
pub(crate) struct Form {
    pub query: String,
    pub sequence: u64,
    pub pending: bool,
    pub page: usize,
    pub result: Option<ResultPage>,
    pub error: Option<String>,
}
#[derive(Default)]
pub(crate) struct State {
    pub forms: std::collections::HashMap<String, Form>,
    pub active: Option<String>,
    pub epoch: u64,
}
impl State {
    pub fn form(&mut self, id: &str) -> &mut Form {
        self.forms.entry(id.into()).or_default()
    }
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
    pub fn reselect(&mut self, id: &str) {
        self.active = Some(id.into());
        self.epoch = self.epoch.wrapping_add(1);
    }
    pub fn edit(&mut self, id: &str, query: &str) {
        let f = self.form(id);
        if f.query != query {
            f.query = query.into();
            f.sequence = f.sequence.wrapping_add(1);
            f.result = None;
            f.error = None;
            f.page = 0;
        }
    }
}
#[cfg(test)]
mod state_tests {
    use super::*;
    #[test]
    fn query_edit_and_project_round_trip_invalidate_old_selection() {
        let mut s = State::default();
        s.select("a");
        let epoch = s.epoch;
        s.edit("a", "old");
        s.form("a").result =
            Some(parse(&serde_json::json!({"mode":"text_fallback","results":[]})).unwrap());
        let seq = s.form("a").sequence;
        s.edit("a", "new");
        assert!(s.form("a").result.is_none());
        assert!(s.form("a").sequence > seq);
        s.select("b");
        s.edit("b", "other");
        s.select("a");
        assert!(s.epoch > epoch);
        assert_eq!(s.form("a").query, "new");
        assert_eq!(s.form("b").query, "other");
    }
}
pub(crate) fn render(ui: &mut eframe::egui::Ui, state: &mut State, id: &str) -> bool {
    state.select(id);
    ui.heading("자연어 지식 검색");
    let mut query = state.form(id).query.clone();
    ui.label("검색어");
    if ui.text_edit_singleline(&mut query).changed() {
        state.edit(id, &query);
    }
    let form = state.form(id);
    let action = ui
        .add_enabled(
            !form.pending && !form.query.trim().is_empty(),
            eframe::egui::Button::new("의미 검색 요청"),
        )
        .clicked();
    if form.pending {
        ui.label("검색 요청 중");
    }
    if let Some(error) = &form.error {
        ui.label(error);
    }
    if let Some(result) = &form.result {
        ui.label(match result.mode {
            Mode::Semantic => "의미 검색 결과",
            Mode::TextFallback => "텍스트 대체 결과 (의미 검색 결과가 아닙니다)",
        });
        ui.label("서버가 반환한 결과만 표시합니다. 전체 건수는 제공되지 않습니다.");
        let count = result.documents.len();
        if count == 0 {
            ui.label("검색 결과가 없습니다.");
        }
        let start = form.page.saturating_mul(20).min(count);
        let end = start.saturating_add(20).min(count);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(form.page > 0, eframe::egui::Button::new("이전 결과"))
                .clicked()
            {
                form.page = form.page.saturating_sub(1);
            }
            if ui
                .add_enabled(end < count, eframe::egui::Button::new("다음 결과"))
                .clicked()
            {
                form.page = form.page.saturating_add(1);
            }
        });
        for i in start..end {
            let doc = &result.documents[i];
            ui.group(|ui| {
                ui.label(format!(
                    "{} / 버전 {} / 출처 {}",
                    doc["id"].as_str().unwrap_or(""),
                    doc["version"],
                    doc["source"].as_str().unwrap_or("")
                ));
                if result.mode == Mode::Semantic {
                    ui.label(format!("서버 점수: {}", result.hits[i]["score"]));
                }
                ui.collapsing("검색 시 반환된 문서", |ui| {
                    let text = doc["text"].as_str().unwrap_or("");
                    let lines: Vec<&str> = text.split('\n').collect();
                    eframe::egui::ScrollArea::vertical()
                        .id_salt(("semantic_doc", i))
                        .max_height(200.0)
                        .show_rows(ui, 20.0, lines.len(), |ui, range| {
                            for n in range {
                                ui.label(lines[n]);
                            }
                        });
                });
            });
        }
    }
    action
}
#[cfg(test)]
mod ui_tests {
    use super::*;
    #[test]
    fn fallback_result_is_visibly_text_not_semantic() {
        let mut s = State::default();
        s.select("a");
        s.form("a").result=Some(parse(&serde_json::json!({"mode":"text_fallback","results":[{"id":"d","version":1,"source":"src","text":"문서"}]})).unwrap());
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
        assert!(labels.iter().any(|s| s.contains("텍스트 대체 결과")));
        assert!(labels.iter().any(|s| s.contains("src")));
        assert!(!labels.iter().any(|s| s == "의미 검색 결과"));
    }
}
