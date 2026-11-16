use eframe::egui;
pub(crate) const PAGE_SIZE: usize = 30;
#[derive(Clone, serde::Deserialize)]
pub(crate) struct Edge {
    pub source: String,
    pub document_id: String,
    pub document_version: u64,
}
pub(crate) fn edges(data: &serde_json::Value) -> Result<Vec<Edge>, ()> {
    let nodes = data["nodes"].as_array().ok_or(())?;
    let values = data["edges"].as_array().ok_or(())?;
    if values.len() > PAGE_SIZE {
        return Err(());
    }
    let edges: Vec<Edge> = serde_json::from_value(data["edges"].clone()).map_err(|_| ())?;
    if !edges.iter().all(|edge| {
        edge.document_version > 0
            && nodes
                .iter()
                .any(|n| n["kind"] == "source" && n["id"] == edge.source)
            && nodes.iter().any(|n| {
                n["kind"] == "document"
                    && n["id"] == edge.document_id
                    && n["version"].as_u64() == Some(edge.document_version)
            })
    }) {
        return Err(());
    }
    Ok(edges)
}
#[derive(Default)]
pub(crate) struct Page {
    pub edges: Vec<Edge>,
    pub offset: usize,
    pub loaded: bool,
    pub error: Option<String>,
}
#[derive(Default)]
pub(crate) struct State {
    pub active: Option<String>,
    pub epoch: u64,
    pub page_request: u64,
    pub pages: std::collections::HashMap<String, Page>,
    selected: std::collections::HashMap<String, Node>,
    canvas: std::collections::HashMap<String, Canvas>,
}
struct Canvas {
    zoom: f32,
    pan: egui::Vec2,
}
impl Default for Canvas {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
        }
    }
}
impl State {
    pub fn select(&mut self, id: &str) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.into());
            self.epoch = self.epoch.wrapping_add(1);
        }
    }
}
pub(crate) fn render(ui: &mut egui::Ui, state: &mut State, project_id: &str) -> Option<usize> {
    ui.heading("지식 출처 관계");
    let page = state.pages.entry(project_id.into()).or_default();
    let mut action = ui.button("출처 관계 불러오기").clicked().then_some(0);
    if let Some(error) = &page.error {
        ui.label(error);
    }
    if page.loaded {
        if state
            .selected
            .get(project_id)
            .is_some_and(|node| !page.nodes().contains(node))
        {
            state.selected.remove(project_id);
        }
        ui.label(format!(
            "현재 페이지: {}부터 (전체 건수 미제공)",
            page.offset
        ));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    page.previous().is_some(),
                    egui::Button::new("이전 관계 페이지"),
                )
                .clicked()
            {
                action = page.previous();
            }
            if ui
                .add_enabled(
                    page.next_candidate().is_some(),
                    egui::Button::new("다음 관계 페이지 확인"),
                )
                .clicked()
            {
                action = page.next_candidate();
            }
        });
        if page.edges.is_empty() {
            ui.label("이 페이지에 출처 관계가 없습니다.");
        }
        let canvas = state.canvas.entry(project_id.into()).or_default();
        ui.add(egui::Slider::new(&mut canvas.zoom, 0.5..=2.0).text("확대"));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 240.0), egui::Sense::drag());
        canvas.pan += response.drag_delta();
        let painter = ui.painter_at(rect);
        painter.rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
            egui::StrokeKind::Inside,
        );
        for (i, edge) in page.edges.iter().enumerate() {
            let a = rect.left_top()
                + canvas.pan
                + egui::vec2(20.0, 20.0 + i as f32 * 28.0 * canvas.zoom);
            let b = a + egui::vec2(220.0 * canvas.zoom, 0.0);
            painter.line_segment(
                [a + egui::vec2(0.0, 8.0), b + egui::vec2(0.0, 8.0)],
                egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
            );
            for (point, node, label, color) in [
                (
                    a,
                    Node::Source(edge.source.clone()),
                    format!("출처 {}", edge.source),
                    egui::Color32::LIGHT_BLUE,
                ),
                (
                    b,
                    Node::Document(edge.document_id.clone(), edge.document_version),
                    format!("문서 {} (v{})", edge.document_id, edge.document_version),
                    egui::Color32::WHITE,
                ),
            ] {
                let text = painter.text(
                    point,
                    egui::Align2::LEFT_TOP,
                    label,
                    egui::FontId::proportional(13.0 * canvas.zoom),
                    color,
                );
                let hit = rect.intersect(text.expand(4.0));
                if hit.is_positive()
                    && ui
                        .interact(
                            hit,
                            ui.id()
                                .with(("graph-canvas", i, matches!(node, Node::Document(..)))),
                            egui::Sense::click(),
                        )
                        .clicked()
                {
                    state.selected.insert(project_id.into(), node);
                }
            }
        }
        ui.label("드래그로 이동하고 확대를 조절하세요. 노드를 선택할 수 있습니다.");
        egui::ScrollArea::vertical()
            .id_salt("graph_nodes")
            .max_height(240.0)
            .show(ui, |ui| {
                for node in page.nodes() {
                    let label = match &node {
                        Node::Source(id) => format!("출처: {id}"),
                        Node::Document(id, version) => format!("문서: {id} (v{version})"),
                    };
                    if ui
                        .selectable_label(state.selected.get(project_id) == Some(&node), label)
                        .clicked()
                    {
                        state.selected.insert(project_id.into(), node);
                    }
                }
            });
        if let Some(node) = state.selected.get(project_id) {
            ui.label(format!(
                "선택: {}",
                match node {
                    Node::Source(id) => format!("출처 {id}"),
                    Node::Document(id, v) => format!("문서 {id} (v{v})"),
                }
            ));
        }
        for edge in &page.edges {
            ui.label(format!(
                "{} → {} (v{})",
                edge.source, edge.document_id, edge.document_version
            ));
        }
    }
    action
}
impl Page {
    fn previous(&self) -> Option<usize> {
        (self.loaded && self.offset > 0).then(|| self.offset.saturating_sub(PAGE_SIZE))
    }
    fn next_candidate(&self) -> Option<usize> {
        if self.loaded && self.edges.len() == PAGE_SIZE {
            self.offset.checked_add(PAGE_SIZE)
        } else {
            None
        }
    }
}
#[cfg(test)]
mod additional_tests {
    use super::*;
    #[test]
    fn graph_page_navigation_keeps_offsets_and_does_not_claim_more_results() {
        let edge = Edge {
            source: "출처".into(),
            document_id: "문서".into(),
            document_version: 2,
        };
        let mut page = Page {
            edges: vec![edge; PAGE_SIZE],
            offset: PAGE_SIZE,
            loaded: true,
            error: None,
        };
        assert_eq!(page.previous(), Some(0));
        assert_eq!(page.next_candidate(), Some(PAGE_SIZE * 2));
        page.edges.pop();
        assert_eq!(page.next_candidate(), None);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Source(String),
    Document(String, u64),
}
impl Page {
    fn nodes(&self) -> Vec<Node> {
        let mut nodes = Vec::new();
        for edge in &self.edges {
            for node in [
                Node::Source(edge.source.clone()),
                Node::Document(edge.document_id.clone(), edge.document_version),
            ] {
                if !nodes.contains(&node) {
                    nodes.push(node);
                }
            }
        }
        nodes
    }
}
#[cfg(test)]
mod node_tests {
    use super::*;
    #[test]
    fn graph_page_renders_interactive_canvas_with_original_navigation_hint() {
        let ctx = egui::Context::default();
        let mut state = State::default();
        state.pages.insert(
            "p".into(),
            Page {
                edges: vec![Edge {
                    source: "src".into(),
                    document_id: "doc".into(),
                    document_version: 2,
                }],
                loaded: true,
                ..Default::default()
            },
        );
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "p");
            });
        });
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains("드래그로 이동"))), "current page must expose canvas navigation");
    }
    #[test]
    fn canvas_document_click_selects_exact_kind_id_and_version() {
        let ctx = egui::Context::default();
        let mut state = State::default();
        state.pages.insert(
            "p".into(),
            Page {
                edges: vec![Edge {
                    source: "doc".into(),
                    document_id: "doc".into(),
                    document_version: 2,
                }],
                loaded: true,
                ..Default::default()
            },
        );
        let frame = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let output = ctx.run(frame(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "p");
            });
        });
        let point = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.job.text == "문서 doc (v2)" => {
                    Some(t.pos + egui::vec2(5.0, 5.0))
                }
                _ => None,
            })
            .expect("canvas must draw document");
        for pressed in [true, false] {
            let events = vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = ctx.run(frame(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    render(ui, &mut state, "p");
                });
            });
        }
        assert_eq!(
            state.selected.get("p"),
            Some(&Node::Document("doc".into(), 2))
        );
    }
    #[test]
    fn canvas_clips_panned_nodes_and_keeps_view_state_per_project() {
        let ctx = egui::Context::default();
        let mut state = State::default();
        state.pages.insert(
            "p".into(),
            Page {
                edges: vec![Edge {
                    source: "src".into(),
                    document_id: "doc".into(),
                    document_version: 1,
                }],
                loaded: true,
                ..Default::default()
            },
        );
        state.canvas.insert(
            "p".into(),
            Canvas {
                zoom: 1.5,
                pan: egui::vec2(0.0, 300.0),
            },
        );
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "p");
            });
        });
        let drawn = output.shapes.iter().find(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.job.text == "문서 doc (v1)")).expect("node draw command");
        if let egui::Shape::Text(t) = &drawn.shape {
            assert!(t.pos.y > drawn.clip_rect.bottom());
        }
        assert_eq!(state.canvas.entry("other".into()).or_default().zoom, 1.0);
        assert_eq!(state.canvas["p"].zoom, 1.5);
    }
    #[test]
    fn graph_nodes_preserve_kind_id_and_document_version() {
        let page = Page {
            edges: vec![
                Edge {
                    source: "same".into(),
                    document_id: "same".into(),
                    document_version: 1,
                },
                Edge {
                    source: "same".into(),
                    document_id: "same".into(),
                    document_version: 2,
                },
            ],
            offset: 0,
            loaded: true,
            error: None,
        };
        assert_eq!(
            page.nodes(),
            vec![
                Node::Source("same".into()),
                Node::Document("same".into(), 1),
                Node::Document("same".into(), 2)
            ]
        );
    }
}
#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn graph_render_shows_nodes_and_navigation_as_current_page_only() {
        let mut state = State::default();
        state.pages.insert(
            "p".into(),
            Page {
                edges: vec![
                    Edge {
                        source: "src".into(),
                        document_id: "doc".into(),
                        document_version: 3
                    };
                    PAGE_SIZE
                ],
                offset: PAGE_SIZE,
                loaded: true,
                error: None,
            },
        );
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "p");
            });
        });
        let labels: Vec<String> = out
            .shapes
            .iter()
            .filter_map(|s| {
                if let egui::epaint::Shape::Text(t) = &s.shape {
                    Some(t.galley.job.text.clone())
                } else {
                    None
                }
            })
            .collect();
        for wanted in [
            "이전 관계 페이지",
            "다음 관계 페이지 확인",
            "출처: src",
            "문서: doc (v3)",
        ] {
            assert!(labels.iter().any(|s| s == wanted), "missing {wanted}");
        }
    }
}
#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn selection_is_cleared_when_it_is_not_on_current_page() {
        let mut state = State::default();
        state
            .selected
            .insert("p".into(), Node::Document("old".into(), 1));
        state.pages.insert(
            "p".into(),
            Page {
                edges: vec![Edge {
                    source: "s".into(),
                    document_id: "new".into(),
                    document_version: 1,
                }],
                offset: 30,
                loaded: true,
                error: None,
            },
        );
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &mut state, "p");
            });
        });
        assert!(!state.selected.contains_key("p"));
    }
}
