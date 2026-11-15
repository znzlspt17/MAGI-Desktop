use eframe::egui;
pub(crate) const PAGE_SIZE:usize=30;
#[derive(Clone,serde::Deserialize)] pub(crate) struct Edge {pub source:String,pub document_id:String,pub document_version:u64}
pub(crate) fn edges(data:&serde_json::Value)->Result<Vec<Edge>,()> {
    let nodes=data["nodes"].as_array().ok_or(())?;let values=data["edges"].as_array().ok_or(())?;if values.len()>PAGE_SIZE{return Err(());}let edges:Vec<Edge>=serde_json::from_value(data["edges"].clone()).map_err(|_|())?;
    if !edges.iter().all(|edge| edge.document_version>0 && nodes.iter().any(|n|n["kind"]=="source" && n["id"]==edge.source) && nodes.iter().any(|n|n["kind"]=="document" && n["id"]==edge.document_id && n["version"].as_u64()==Some(edge.document_version))){return Err(());}Ok(edges)
}
#[derive(Default)] pub(crate) struct Page {pub edges:Vec<Edge>,pub offset:usize,pub loaded:bool,pub error:Option<String>}
#[derive(Default)] pub(crate) struct State { pub active:Option<String>,pub epoch:u64,pub page_request:u64,pub pages:std::collections::HashMap<String,Page> }
impl State {pub fn select(&mut self,id:&str){if self.active.as_deref()!=Some(id){self.active=Some(id.into());self.epoch=self.epoch.wrapping_add(1);}}}
pub(crate) fn render(ui:&mut egui::Ui,state:&mut State,project_id:&str)->Option<usize> { ui.heading("지식 출처 관계");let page=state.pages.entry(project_id.into()).or_default();let action=ui.button("출처 관계 불러오기").clicked().then_some(0);if let Some(error)=&page.error{ui.label(error);}if page.loaded {ui.label(format!("현재 페이지: {}부터 (전체 건수 미제공)",page.offset));if page.edges.is_empty(){ui.label("이 페이지에 출처 관계가 없습니다.");}for edge in &page.edges {ui.label(format!("출처: {}",edge.source));ui.label(format!("문서: {} (v{})",edge.document_id,edge.document_version));}}action }

