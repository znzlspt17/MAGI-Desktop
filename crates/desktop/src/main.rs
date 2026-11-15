#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use domain::Project;
use domain::{Command, ResponseData};
use eframe::egui;
use std::sync::mpsc::{Receiver, SyncSender};
mod knowledge_text_ui;
mod model_settings;
mod run_history;
mod spec_review;
mod storage;
mod worker;

#[derive(Default)]
struct State {
    selected: Option<String>,
    project: Option<Project>,
    generation: u64,
    name: String,
    path: String,
    status: String,
    projects: Vec<Project>,
    creating: bool,
    starting: bool,
    status_pending: bool,
    storage: storage::Storage,
    close_check: bool,
    close_allowed: bool,
    close_confirmation: bool,
    interview_inputs: std::collections::HashMap<String, String>,
    interview_pending: std::collections::HashSet<String>,
    spec_runs: std::collections::HashMap<String, String>,
    spec_starting: std::collections::HashSet<String>,
    spec_notices: std::collections::HashMap<String, String>,
    model_forms: std::collections::HashMap<String, model_settings::Form>,
    model_baselines: std::collections::HashMap<String, model_settings::Form>,
    model_pending: std::collections::HashSet<String>,
    credential_forms: std::collections::HashMap<String, model_settings::CredentialForm>,
    credential_pending: std::collections::HashSet<String>,
    spec_poll_at: std::collections::HashMap<String, std::time::Instant>,
    spec_forms: std::collections::HashMap<String, spec_review::Form>,
    spec_review_pending: std::collections::HashSet<String>,
    spec_submitted: std::collections::HashMap<String, domain::SpecDraft>,
    history: run_history::History,
    knowledge: knowledge_text_ui::State,
}

impl State {
    fn spec_notice(&mut self, project_id: &str, message: String) {
        if self.selected.as_deref() == Some(project_id) {
            self.status = message.clone();
        }
        self.spec_notices.insert(project_id.to_owned(), message);
    }
    fn select(&mut self, id: &str) -> u64 {
        self.knowledge.select(id);
        self.history.project(id);
        self.selected = Some(id.to_owned());
        self.project = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn receive_project(&mut self, generation: u64, project: Project) {
        if self.generation == generation && self.selected.as_deref() == Some(&project.id) {
            self.project = Some(project);
        }
    }
    fn created(&mut self, name: &str, path: &str, result: Result<Project, String>) {
        match result {
            Ok(project) => {
                if self.name == name && self.path == path {
                    self.name.clear();
                    self.path.clear();
                }
                if !self.projects.iter().any(|p| p.id == project.id) {
                    self.projects.push(project);
                }
                self.status = "프로젝트 생성 완료".into();
            }
            Err(error) => self.status = format!("오류: {error}"),
        }
    }
}

struct App {
    state: State,
    tx: SyncSender<worker::Work>,
    rx: Receiver<worker::Reply>,
    cancel_startup: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl App {
    fn poll_storage(&mut self) {
        if self.state.storage.pending_poll {
            self.state.storage.poll_again = true;
            return;
        }
        if self.send(worker::Work::new(
            worker::Kind::Storage,
            Command::ListStorageFailures,
        )) {
            self.state.storage.pending_poll = true;
            self.state.storage.last_poll = std::time::Instant::now();
        } else {
            self.state.storage.known = false;
            self.state.storage.error =
                Some("저장 상태 요청을 보내지 못했습니다. 다시 확인해 주세요.".into());
            self.state.close_check = false;
        }
    }

    fn retry_storage(&mut self, run_id: &str) {
        if self.state.storage.retries.contains(run_id) {
            return;
        }
        if self.send(worker::Work::new(
            worker::Kind::RetrySave {
                run_id: run_id.to_owned(),
            },
            Command::RetryCompletionSave {
                run_id: run_id.to_owned(),
            },
        )) {
            self.state.storage.notice = None;
            self.state.storage.retries.insert(run_id.to_owned());
            self.state.storage.protected.insert(run_id.to_owned());
        }
    }

    fn send(&mut self, work: worker::Work) -> bool {
        let status = matches!(&work.kind, worker::Kind::Status);
        if status && self.state.status_pending {
            return false;
        }
        if status {
            self.cancel_startup
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if self.tx.try_send(work).is_err() {
            self.state.status = "요청 대기열이 가득 찼거나 엔진 연결이 종료됐습니다".into();
            return false;
        }
        if status {
            self.state.status_pending = true;
        }
        true
    }

    fn apply(&mut self, reply: worker::Reply) {
        use worker::Kind;
        let is_status = matches!(&reply.kind, Kind::Status);
        match reply.kind {
            Kind::Storage => {
                self.state.storage.pending_poll = false;
                let snapshot = match reply.result {
                    Ok(ResponseData::Data(value)) => serde_json::from_value::<storage::Snapshot>(value).map_err(|_| "저장 진단 응답을 읽을 수 없습니다".to_owned()).and_then(|snapshot| if snapshot.valid() { Ok(snapshot) } else { Err("저장 진단 상태가 올바르지 않습니다. 엔진을 확인하고 다시 시도하세요.".into()) }),
                    Ok(_) => Err("저장 진단 응답을 읽을 수 없습니다".into()),
                    Err(error) if error.contains("requested feature is not available") => Err("현재 엔진은 저장 복구 진단을 지원하지 않습니다. 엔진을 업데이트한 뒤 다시 확인하세요.".into()),
                    Err(error) => Err(format!("저장 진단에 연결하지 못했습니다: {error}. 엔진 연결을 확인하고 다시 시도하세요.")),
                };
                match snapshot {
                    Ok(mut snapshot) => {
                        if let Some(previous) = &self.state.storage.snapshot {
                            for failure in &previous.failures {
                                if self.state.storage.protected.contains(&failure.run_id)
                                    && (self.state.storage.poll_again
                                        || self.state.storage.retries.contains(&failure.run_id))
                                    && !snapshot
                                        .failures
                                        .iter()
                                        .any(|item| item.run_id == failure.run_id)
                                {
                                    snapshot.failures.push(failure.clone());
                                }
                            }
                        }
                        self.state.storage.snapshot = Some(snapshot);
                        self.state.storage.known = true;
                        self.state.storage.error = None;
                        if !self.state.storage.poll_again {
                            self.state
                                .storage
                                .protected
                                .retain(|run| self.state.storage.retries.contains(run));
                        }
                    }
                    Err(error) => {
                        self.state.storage.known = false;
                        self.state.storage.error = Some(error);
                    }
                }
                if std::mem::take(&mut self.state.storage.poll_again) {
                    self.poll_storage();
                } else if self.state.close_check {
                    self.state.close_check = false;
                    if self.state.storage.may_close() {
                        if self.state.storage.has_pending_result() {
                            self.state.close_confirmation = true;
                        } else {
                            self.state.close_allowed = true;
                        }
                    } else {
                        self.state.status = "완료 저장 상태를 확인한 뒤 다시 닫아 주세요".into();
                    }
                }
            }
            Kind::RetrySave { run_id } => {
                self.state.storage.retries.remove(&run_id);
                let storage_only = self
                    .state
                    .storage
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.failures.iter().find(|f| f.run_id == run_id))
                    .is_some_and(|f| matches!(f.stage.as_str(), "unknown" | "startup"));
                match reply.result {
                    Ok(ResponseData::Data(value)) if value["state"] == "saved" => self.state.storage.notice = Some(if storage_only { "저장소 복구를 완료했습니다.".into() } else { format!("실행 {run_id}: 완료 결과 저장을 복구했습니다.") }),
                    Ok(ResponseData::Data(value)) if value["state"] == "superseded" => self.state.storage.notice = Some(format!("실행 {run_id}: 새 상태가 먼저 확정되어 이전 결과를 적용하지 않았습니다. 현재 상태를 확인하세요.")),
                    Ok(ResponseData::Data(value)) if value["state"] == "pending" || value["state"] == "cleanup_pending" => {},
                    Ok(_) => self.state.storage.notice = Some(format!("실행 {run_id}: 저장 결과를 확인할 수 없습니다.")),
                    Err(error) => self.state.storage.notice = Some(format!("실행 {run_id}: 저장 재시도 실패: {error}")),
                }
                self.poll_storage();
            }
            Kind::Starting => {
                self.state.starting = true;
                self.state.status = "엔진 시작 대기 중".into();
            }
            Kind::Status => match reply.result {
                Ok(ResponseData::Status { message }) => {
                    self.state.status = if message
                        == "engine ready; project storage only; workflow and models unavailable"
                    {
                        "엔진 연결됨 — 프로젝트 저장 기능 사용 가능".into()
                    } else {
                        message
                    }
                }
                Ok(_) => self.state.status = "오류: 엔진 상태 응답이 올바르지 않습니다".into(),
                Err(e) => self.state.status = format!("오류: {e}"),
            },
            Kind::List => match reply.result {
                Ok(ResponseData::Projects(projects)) => self.state.projects = projects,
                Ok(_) => self.state.status = "오류: 프로젝트 목록 응답이 올바르지 않습니다".into(),
                Err(e) if !self.state.status.starts_with("오류:") => {
                    self.state.status = format!("오류: {e}")
                }
                Err(_) => {}
            },
            Kind::Get { generation } => match reply.result {
                Ok(ResponseData::Project(project)) => {
                    self.state.receive_project(generation, project)
                }
                Err(e) if generation == self.state.generation => {
                    self.state.status = format!("오류: {e}")
                }
                Ok(_) if generation == self.state.generation => {
                    self.state.status = "오류: 프로젝트 응답이 올바르지 않습니다".into()
                }
                _ => {}
            },
            Kind::Interview {
                project_id,
                message,
            } => {
                self.state.interview_pending.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Project(project)) if project.id == project_id => {
                        if self.state.interview_inputs.get(&project_id) == Some(&message) {
                            self.state.interview_inputs.remove(&project_id);
                        }
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.generation = self.state.generation.wrapping_add(1);
                            self.state.project = Some(project);
                            self.state.status =
                                "인터뷰 답변을 저장했습니다. 기존 명세 승인이 해제되었습니다."
                                    .into();
                        }
                    }
                    Ok(_) => self.state.status = "오류: 인터뷰 응답이 올바르지 않습니다".into(),
                    Err(e) => self.state.status = format!("오류: {e}"),
                }
            }
            Kind::GenerateSpec { project_id } => {
                self.state.spec_starting.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Run { run_id }) => {
                        self.state.spec_notices.remove(&project_id);
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.status = "명세 초안 생성 중".into();
                        }
                        self.state
                            .spec_runs
                            .insert(project_id.clone(), run_id.clone());
                        if !self.send(worker::Work::new(
                            worker::Kind::SpecRun {
                                project_id: project_id.clone(),
                                run_id: run_id.clone(),
                            },
                            Command::GetRun { run_id },
                        )) {
                            self.state.spec_poll_at.insert(
                                project_id,
                                std::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                        }
                    }
                    Ok(ResponseData::Project(project)) if project.id == project_id => {
                        self.state.spec_notices.remove(&project_id);
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.generation = self.state.generation.wrapping_add(1);
                            self.state.project = Some(project);
                            self.state.status =
                                "명세 초안을 생성했습니다. 검토가 필요합니다.".into();
                        }
                    }
                    Err(error) => self
                        .state
                        .spec_notice(&project_id, format!("명세 생성 실패: {error}")),
                    _ => self
                        .state
                        .spec_notice(&project_id, "명세 생성 응답이 올바르지 않습니다".into()),
                }
            }
            Kind::SpecRun { project_id, run_id } => {
                if self.state.spec_runs.get(&project_id) == Some(&run_id) {
                    match reply.result {
                        Ok(ResponseData::Data(run))
                            if run["id"] == run_id
                                && run["project_id"] == project_id
                                && run["status"] == "generated" =>
                        {
                            self.state.spec_notices.remove(&project_id);
                            self.state.spec_poll_at.remove(&project_id);
                            if self.state.selected.as_deref() == Some(&project_id) {
                                self.state.generation = self.state.generation.wrapping_add(1);
                                if self.send(worker::Work::new(
                                    worker::Kind::Get {
                                        generation: self.state.generation,
                                    },
                                    Command::GetProject {
                                        project_id: project_id.clone(),
                                    },
                                )) {
                                    self.state.spec_runs.remove(&project_id);
                                    self.state.status = "명세 생성 완료, 초안 조회 중".into();
                                } else {
                                    self.state.spec_poll_at.insert(
                                        project_id,
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(1),
                                    );
                                }
                            } else {
                                self.state.spec_runs.remove(&project_id);
                            }
                        }
                        Ok(ResponseData::Data(run))
                            if run["id"] == run_id
                                && run["project_id"] == project_id
                                && ["failed", "stale", "cancelled"]
                                    .contains(&run["status"].as_str().unwrap_or("")) =>
                        {
                            self.state.spec_runs.remove(&project_id);
                            self.state.spec_notice(
                                &project_id,
                                format!(
                                    "명세 생성 실패 또는 미적용: {}",
                                    run["status"].as_str().unwrap_or("")
                                ),
                            );
                        }
                        Ok(ResponseData::Data(run))
                            if run["id"] == run_id
                                && run["project_id"] == project_id
                                && run["status"] == "generating" =>
                        {
                            self.state.spec_notices.remove(&project_id);
                            if self.state.selected.as_deref() == Some(&project_id) {
                                self.state.status = "명세 초안 생성 중".into();
                            }
                            self.state.spec_poll_at.insert(
                                project_id,
                                std::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                        }
                        Err(error) => {
                            self.state.spec_notice(
                                &project_id,
                                format!("명세 상태 조회 실패, 다시 확인 중: {error}"),
                            );
                            self.state.spec_poll_at.insert(
                                project_id,
                                std::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                        }
                        _ => {
                            self.state.spec_runs.remove(&project_id);
                            if self.state.selected.as_deref() == Some(&project_id) {
                                self.state.status = "명세 상태 응답이 올바르지 않습니다".into();
                            }
                        }
                    }
                }
            }
            Kind::ModelSettings {
                project_id,
                submitted,
            } => {
                self.state.model_pending.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Project(project)) if project.id == project_id => {
                        if self.state.project.as_ref().is_some_and(|current| {
                            current.id == project_id
                                && current.context_version > project.context_version
                        }) {
                            return;
                        }
                        if self.state.model_forms.get(&project_id) == Some(&submitted) {
                            self.state.model_forms.insert(
                                project_id.clone(),
                                model_settings::Form::from_project(&project),
                            );
                        }
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.generation = self.state.generation.wrapping_add(1);
                            self.state.project = Some(project);
                            self.state.status = "모델 설정을 저장했습니다.".into();
                        }
                    }
                    Err(error) if self.state.selected.as_deref() == Some(&project_id) => {
                        self.state.status = format!("모델 설정 저장 실패: {error}")
                    }
                    _ if self.state.selected.as_deref() == Some(&project_id) => {
                        self.state.status = "모델 설정 응답이 올바르지 않습니다".into()
                    }
                    _ => {}
                }
            }
            Kind::Credential { project_id } => {
                self.state.credential_pending.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Empty) => {
                        self.state.credential_forms.remove(&project_id);
                        if self.state.selected.as_deref() == Some(&project_id) {
                            self.state.status = "런타임 키를 설정했습니다.".into();
                        }
                    }
                    _ if self.state.selected.as_deref() == Some(&project_id) => {
                        self.state.status = "런타임 키 설정에 실패했습니다.".into()
                    }
                    _ => {}
                }
            }
            Kind::SaveDraft {
                project_id,
                base_hash,
                context,
                generation,
            } => {
                self.state.spec_review_pending.remove(&project_id);
                let submitted = self.state.spec_submitted.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Project(project))
                        if project.id == project_id
                            && self.state.selected.as_deref() == Some(&project_id)
                            && self.state.generation == generation
                            && self.state.project.as_ref().is_some_and(|p| {
                                p.draft_hash == base_hash && p.context_version == context
                            }) =>
                    {
                        if submitted.as_ref().is_some_and(|d| {
                            serde_json::to_value(d).ok()
                                == serde_json::to_value(&project.draft).ok()
                        }) {
                            if let Some(form) = self.state.spec_forms.get_mut(&project_id) {
                                form.acknowledge_saved(&project.draft);
                            }
                        }
                        self.state.generation = self.state.generation.wrapping_add(1);
                        self.state.project = Some(project);
                        self.state.status =
                            "명세 초안을 저장했습니다. 승인이 해제되었습니다.".into();
                    }
                    Err(error) if self.state.selected.as_deref() == Some(&project_id) => {
                        self.state.status = format!("명세 저장 실패: {error}")
                    }
                    Ok(_) if self.state.selected.as_deref() == Some(&project_id) => self.state.status = "명세 상태가 변경되어 저장 결과를 적용하지 않았습니다. 프로젝트를 다시 확인하세요.".into(),
                    _ => {}
                }
            }
            Kind::ApproveDraft {
                project_id,
                context,
                generation,
            } => {
                self.state.spec_review_pending.remove(&project_id);
                match reply.result {
                    Ok(ResponseData::Approved(approved))
                        if self.state.selected.as_deref() == Some(&project_id)
                            && self.state.generation == generation =>
                    {
                        if let Some(project) = self.state.project.as_mut().filter(|p| {
                            p.id == project_id
                                && p.draft_hash == approved.hash
                                && p.context_version == context
                                && p.approval_version < approved.version
                        }) {
                            project.approval_version = approved.version;
                            project.approval_history.push(approved.clone());
                            project.approved = Some(approved);
                            self.state.generation = self.state.generation.wrapping_add(1);
                            self.state.status = "저장된 명세를 승인했습니다.".into();
                        }
                    }
                    Err(error) if self.state.selected.as_deref() == Some(&project_id) => {
                        self.state.status = format!("명세 승인 실패: {error}")
                    }
                    _ => {}
                }
            }
            Kind::HistoryPage {
                project_id,
                request,
                offset,
            } => {
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.history.project.as_deref() == Some(&project_id)
                    && self.state.history.request == request
                {
                    match reply.result {
                        Ok(ResponseData::Data(data))
                            if data["offset"].as_u64() == Some(offset as u64)
                                && data["limit"].as_u64()
                                    == Some(run_history::PAGE_SIZE as u64)
                                && data["total"].as_u64().is_some()
                                && data["runs"].as_array().is_some_and(|runs| {
                                    runs.len() <= run_history::PAGE_SIZE
                                        && runs.iter().all(|run| {
                                            run["project_id"] == project_id
                                                && run["id"].as_str().is_some()
                                                && run["status"].as_str().is_some()
                                        })
                                }) =>
                        {
                            self.state.history.runs = data["runs"].as_array().unwrap().clone();
                            self.state.history.offset = offset;
                            self.state.history.total = data["total"].as_u64().unwrap() as usize;
                            self.state.history.loaded = true;
                            self.state.history.error = None;
                        }
                        Err(error) => {
                            self.state.history.error = Some(format!("실행 기록 조회 실패: {error}"))
                        }
                        _ => {
                            self.state.history.error =
                                Some("실행 기록 응답이 올바르지 않습니다.".into())
                        }
                    }
                }
            }
            Kind::HistoryDetail {
                project_id,
                run_id,
                request,
            } => {
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.history.project.as_deref() == Some(&project_id)
                    && self.state.history.selected.as_deref() == Some(&run_id)
                    && self.state.history.selection_request == request
                {
                    match reply.result {
                        Ok(ResponseData::Data(data))
                            if data["id"] == run_id
                                && data["project_id"] == project_id
                                && data["status"].as_str().is_some() =>
                        {
                            self.state.history.detail = Some(data);
                        }
                        Err(error) => {
                            self.state.history.error = Some(format!("실행 상태 조회 실패: {error}"))
                        }
                        _ => {
                            self.state.history.error =
                                Some("실행 상태 응답이 올바르지 않습니다.".into())
                        }
                    }
                }
            }
            Kind::HistoryEvents {
                project_id,
                run_id,
                request,
            } => {
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.history.project.as_deref() == Some(&project_id)
                    && self.state.history.selected.as_deref() == Some(&run_id)
                    && self.state.history.selection_request == request
                {
                    match reply.result {
                        Ok(ResponseData::Events(events))
                            if events.iter().all(|event| event.run_id == run_id) =>
                        {
                            self.state.history.events = events;
                        }
                        Err(error) => {
                            self.state.history.error =
                                Some(format!("실행 이벤트 조회 실패: {error}"))
                        }
                        _ => {
                            self.state.history.error =
                                Some("실행 이벤트 응답이 올바르지 않습니다.".into())
                        }
                    }
                }
            }
            Kind::KnowledgePut { project_id, epoch } => {
                self.state.knowledge.put_pending.remove(&project_id);
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.knowledge.active.as_deref() == Some(&project_id)
                    && self.state.knowledge.epoch == epoch
                {
                    self.state.status = match reply.result {
                        Ok(ResponseData::Empty) => "원문을 저장했습니다.".into(),
                        Err(error) => format!("원문 저장 실패: {error}"),
                        _ => "원문 저장 응답이 올바르지 않습니다.".into(),
                    };
                }
            }
            Kind::KnowledgeSearch {
                project_id,
                epoch,
                request,
                query,
                offset,
            } => {
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.knowledge.active.as_deref() == Some(&project_id)
                    && self.state.knowledge.epoch == epoch
                    && self.state.knowledge.search_request == request
                {
                    let form = self.state.knowledge.forms.entry(project_id).or_default();
                    match reply.result {
                        Ok(ResponseData::Data(data))
                            if data["mode"] == "text"
                                && data["results"].as_array().is_some_and(|docs| {
                                    docs.len() <= 20
                                        && docs.iter().all(|d| {
                                            d["id"].as_str().is_some()
                                                && d["source"].as_str().is_some()
                                                && d["text"].as_str().is_some()
                                                && d["version"].as_u64().is_some()
                                        })
                                }) =>
                        {
                            form.results = data["results"].as_array().unwrap().clone();
                            form.result_query = query;
                            form.offset = offset;
                            form.loaded = true;
                            form.error = None;
                        }
                        Err(error) => form.error = Some(format!("텍스트 검색 실패: {error}")),
                        _ => form.error = Some("텍스트 검색 응답이 올바르지 않습니다.".into()),
                    }
                }
            }
            Kind::KnowledgeSource {
                project_id,
                epoch,
                request,
                document_id,
            } => {
                if self.state.selected.as_deref() == Some(&project_id)
                    && self.state.knowledge.active.as_deref() == Some(&project_id)
                    && self.state.knowledge.epoch == epoch
                    && self.state.knowledge.source_request == request
                {
                    let form = self.state.knowledge.forms.entry(project_id).or_default();
                    match reply.result {
                        Ok(ResponseData::Data(doc))
                            if doc["id"] == document_id
                                && doc["source"].as_str().is_some()
                                && doc["text"].as_str().is_some()
                                && doc["version"].as_u64().is_some() =>
                        {
                            form.source_document = Some(doc);
                            form.error = None;
                        }
                        Err(error) => form.error = Some(format!("원문 조회 실패: {error}")),
                        _ => form.error = Some("원문 응답이 올바르지 않습니다.".into()),
                    }
                }
            }
            Kind::Create { name, path } => {
                self.state.creating = false;
                let result = match reply.result {
                    Ok(ResponseData::Project(project)) => Ok(project),
                    Ok(_) => Err("프로젝트 생성 응답이 올바르지 않습니다".into()),
                    Err(e) => Err(e),
                };
                self.state.created(&name, &path, result);
            }
        }
        if is_status {
            self.state.starting = false;
            self.state.status_pending = false;
        }
    }

    fn render(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.rx.try_recv() {
            self.apply(reply);
        }
        let due: Vec<_> = self
            .state
            .spec_poll_at
            .iter()
            .filter(|(_, at)| **at <= std::time::Instant::now())
            .map(|(id, _)| id.clone())
            .collect();
        for project_id in due {
            self.state.spec_poll_at.remove(&project_id);
            if let Some(run_id) = self.state.spec_runs.get(&project_id).cloned() {
                if !self.send(worker::Work::new(
                    worker::Kind::SpecRun {
                        project_id: project_id.clone(),
                        run_id: run_id.clone(),
                    },
                    Command::GetRun { run_id },
                )) {
                    self.state.spec_poll_at.insert(
                        project_id,
                        std::time::Instant::now() + std::time::Duration::from_secs(1),
                    );
                }
            }
        }
        if self.state.close_allowed && !self.state.storage.may_close() {
            self.state.close_allowed = false;
        }
        if self.state.close_confirmation && !self.state.storage.may_close() {
            self.state.close_confirmation = false;
        }
        if self.state.close_allowed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.state.close_allowed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.status = "완료 저장 상태를 확인한 뒤 다시 닫아 주세요".into();
            self.state.close_check = true;
            self.poll_storage();
        }
        if !self.state.close_allowed
            && self.state.storage.last_poll.elapsed() >= std::time::Duration::from_secs(2)
            && !self.state.storage.pending_poll
        {
            self.poll_storage();
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MAGI Desktop");
                ui.separator();
                ui.label(&self.state.status);
                if self.state.starting
                    && ui
                        .button("시작 대기 취소")
                        .on_hover_text("대기만 중단하며 이미 시작된 엔진은 종료하지 않습니다.")
                        .clicked()
                {
                    self.cancel_startup
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.state.status = "엔진 시작 대기 취소 중".into();
                }
            });
        });
        self.storage_panel(ctx);
        if self.state.close_confirmation {
            egui::Window::new("완료 저장 대기")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("완료 결과가 저장 대기 중입니다. 복구 기록은 보존되며 엔진은 계속 실행됩니다.");
                    ui.horizontal(|ui| {
                        if ui.button("창 유지").clicked() {
                            self.state.close_confirmation = false;
                        }
                        if ui.button("창만 닫기").clicked() && self.state.storage.may_close() {
                            self.state.close_confirmation = false;
                            self.state.close_allowed = true;
                        }
                    });
                });
        }
        egui::SidePanel::left("sidebar")
            .default_width(230.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("프로젝트");
                    if self.state.status.starts_with("오류:") {
                        ui.label("엔진이 실행 중인지 확인한 뒤 새로고침하세요.");
                    }
                    if ui.button("새로고침").clicked() {
                        self.send(worker::Work::new(worker::Kind::Status, Command::Status));
                        self.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
                    }
                    for project in self.state.projects.clone() {
                        if ui
                            .selectable_label(
                                self.state.selected.as_deref() == Some(&project.id),
                                &project.name,
                            )
                            .clicked()
                        {
                            let generation = self.state.select(&project.id);
                            self.send(worker::Work::new(
                                worker::Kind::Get { generation },
                                Command::GetProject {
                                    project_id: project.id,
                                },
                            ));
                        }
                    }
                    ui.separator();
                    ui.label("새 프로젝트");
                    ui.text_edit_singleline(&mut self.state.name);
                    ui.text_edit_singleline(&mut self.state.path);
                    if ui
                        .add_enabled(
                            !self.state.creating
                                && !self.state.name.trim().is_empty()
                                && !self.state.path.trim().is_empty(),
                            egui::Button::new("생성"),
                        )
                        .clicked()
                    {
                        let name = self.state.name.clone();
                        let path = self.state.path.clone();
                        if self.send(worker::Work::new(
                            worker::Kind::Create {
                                name: name.clone(),
                                path: path.clone(),
                            },
                            Command::CreateProject { name, path },
                        )) {
                            self.state.creating = true;
                            self.state.status = "프로젝트 생성 중".into();
                        }
                    }
                });
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("project-content")
                .show(ui, |ui| {
                    if let Some(project) = self.state.project.clone() {
                        ui.heading(&project.name);
                        ui.label(&project.path);
                        self.state.history.project(&project.id);
                        if let Some(action) = run_history::render(ui, &self.state.history) {
                            match action {
                                run_history::Action::Page(offset) => {
                                    let request = self.state.history.request.wrapping_add(1);
                                    if self.send(worker::Work::new(
                                        worker::Kind::HistoryPage {
                                            project_id: project.id.clone(),
                                            request,
                                            offset,
                                        },
                                        Command::ListRuns {
                                            project_id: project.id.clone(),
                                            offset,
                                            limit: run_history::PAGE_SIZE,
                                        },
                                    )) {
                                        self.state.history.request = request;
                                    }
                                }
                                run_history::Action::Select(run_id) => {
                                    self.state.history.selected = Some(run_id.clone());
                                    self.state.history.detail = None;
                                    self.state.history.events.clear();
                                    self.state.history.error = None;
                                    self.state.history.selection_request =
                                        self.state.history.selection_request.wrapping_add(1);
                                    let request = self.state.history.selection_request;
                                    self.send(worker::Work::new(
                                        worker::Kind::HistoryDetail {
                                            project_id: project.id.clone(),
                                            run_id: run_id.clone(),
                                            request,
                                        },
                                        Command::GetRun {
                                            run_id: run_id.clone(),
                                        },
                                    ));
                                    self.send(worker::Work::new(
                                        worker::Kind::HistoryEvents {
                                            project_id: project.id.clone(),
                                            run_id: run_id.clone(),
                                            request,
                                        },
                                        Command::ListEvents { run_id, after: 0 },
                                    ));
                                }
                            }
                        }
                        if let Some(notice) = self.state.spec_notices.get(&project.id) {
                            ui.label(notice);
                        }
                        if self.state.spec_runs.contains_key(&project.id)
                            || self.state.spec_starting.contains(&project.id)
                        {
                            ui.label("명세 초안 생성 중");
                        }
                        if ui
                            .add_enabled(
                                !self.state.spec_starting.contains(&project.id)
                                    && !self.state.spec_runs.contains_key(&project.id),
                                egui::Button::new("명세 초안 생성"),
                            )
                            .clicked()
                            && self.send(worker::Work::new(
                                worker::Kind::GenerateSpec {
                                    project_id: project.id.clone(),
                                },
                                Command::GenerateSpec {
                                    project_id: project.id.clone(),
                                },
                            ))
                        {
                            self.state.spec_starting.insert(project.id.clone());
                        }
                        ui.heading("인터뷰");
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for message in &project.interview {
                                    ui.label(message);
                                }
                            });
                        let input = self
                            .state
                            .interview_inputs
                            .entry(project.id.clone())
                            .or_default();
                        ui.add(egui::TextEdit::multiline(input).desired_rows(4));
                        let message = input.clone();
                        if ui
                            .add_enabled(
                                !message.trim().is_empty()
                                    && !self.state.interview_pending.contains(&project.id),
                                egui::Button::new("응답 저장"),
                            )
                            .clicked()
                        {
                            let id = project.id.clone();
                            if self.send(worker::Work::new(
                                worker::Kind::Interview {
                                    project_id: id.clone(),
                                    message: message.clone(),
                                },
                                Command::SubmitInterview {
                                    project_id: id.clone(),
                                    message,
                                },
                            )) {
                                self.state.interview_pending.insert(id);
                                self.state.generation = self.state.generation.wrapping_add(1);
                            }
                        }
                        ui.label("답변을 저장하면 기존 명세 승인이 해제됩니다.");
                        if let Some(result) = model_settings::render(
                            ui,
                            &project,
                            &mut self.state.model_forms,
                            &mut self.state.model_baselines,
                            self.state.model_pending.contains(&project.id),
                        ) {
                            match result {
                                Ok((submitted, command)) => {
                                    if self.send(worker::Work::new(
                                        worker::Kind::ModelSettings {
                                            project_id: project.id.clone(),
                                            submitted,
                                        },
                                        command,
                                    )) {
                                        self.state.model_pending.insert(project.id.clone());
                                    }
                                }
                                Err(error) => self.state.status = error,
                            }
                        }
                        if let Some(result) = model_settings::credentials(
                            ui,
                            &project,
                            &mut self.state.credential_forms,
                            self.state.credential_pending.contains(&project.id),
                        ) {
                            match result {
                                Ok(command) => {
                                    if self.send(worker::Work::new(
                                        worker::Kind::Credential {
                                            project_id: project.id.clone(),
                                        },
                                        command,
                                    )) {
                                        self.state.credential_pending.insert(project.id.clone());
                                    }
                                }
                                Err(error) => self.state.status = error,
                            }
                        }

                        ui.heading("명세 초안");
                        let review_busy = self.state.spec_review_pending.contains(&project.id)
                            || self.state.interview_pending.contains(&project.id)
                            || self.state.model_pending.contains(&project.id)
                            || self.state.spec_starting.contains(&project.id)
                            || self.state.spec_runs.contains_key(&project.id);
                        if let Some(action) = spec_review::render(
                            ui,
                            &project,
                            &mut self.state.spec_forms,
                            review_busy,
                        ) {
                            match action {
                                spec_review::Action::Save(Ok(draft)) => {
                                    let draft = *draft;
                                    let submitted = draft.clone();
                                    if self.send(worker::Work::new(
                                        worker::Kind::SaveDraft {
                                            project_id: project.id.clone(),
                                            base_hash: project.draft_hash.clone(),
                                            context: project.context_version,
                                            generation: self.state.generation,
                                        },
                                        Command::SaveDraft {
                                            project_id: project.id.clone(),
                                            draft,
                                        },
                                    )) {
                                        self.state.spec_review_pending.insert(project.id.clone());
                                        self.state
                                            .spec_submitted
                                            .insert(project.id.clone(), submitted);
                                    }
                                }
                                spec_review::Action::Save(Err(error)) => self.state.status = error,
                                spec_review::Action::Approve(accepted_optional) => {
                                    if self.send(worker::Work::new(
                                        worker::Kind::ApproveDraft {
                                            project_id: project.id.clone(),
                                            context: project.context_version,
                                            generation: self.state.generation,
                                        },
                                        Command::ApproveSpec {
                                            project_id: project.id.clone(),
                                            expected_draft_hash: project.draft_hash.clone(),
                                            accepted_optional,
                                        },
                                    )) {
                                        self.state.spec_review_pending.insert(project.id.clone());
                                    }
                                }
                            }
                        }
                        ui.label(format!("제목: {}", project.draft.title));
                        ui.label(format!("기준 revision: {}", project.draft.base_revision));
                        for (label, values) in [
                            ("범위", &project.draft.scope),
                            ("요구사항", &project.draft.requirements),
                            ("승인 기준", &project.draft.acceptance),
                            ("필수 질문", &project.draft.mandatory_questions),
                            ("필수 검증 실패", &project.draft.required_failures),
                            ("선택 항목", &project.draft.optional_issues),
                            ("제약", &project.draft.constraints),
                            ("출처", &project.draft.source_links),
                            ("재사용한 결정", &project.draft.reused_decisions),
                        ] {
                            ui.label(label);
                            for value in values {
                                ui.label(value);
                            }
                        }
                        ui.label(&project.draft.markdown);
                        for task in &project.draft.tasks {
                            ui.label(format!("작업: {}", task.id));
                            for (label, values) in [
                                ("의존 작업", &task.dependencies),
                                ("담당 경로", &task.owned_paths),
                                ("검증", &task.validation),
                            ] {
                                ui.label(label);
                                for value in values {
                                    ui.label(value);
                                }
                            }
                        }
                        for binding in &project.draft.task_bindings {
                            ui.label(format!("작업 연결: {} / {}", binding.task_id, binding.goal));
                            for value in binding
                                .requirement_ids
                                .iter()
                                .chain(&binding.acceptance_ids)
                            {
                                ui.label(value);
                            }
                        }
                        self.state.knowledge.select(&project.id);
                        if let Some(action) =
                            knowledge_text_ui::render(ui, &mut self.state.knowledge, &project.id)
                        {
                            match action {
                                knowledge_text_ui::Action::Put(result) => match result {
                                    Ok(document) => {
                                        if self.send(worker::Work::new(
                                            worker::Kind::KnowledgePut {
                                                project_id: project.id.clone(),
                                                epoch: self.state.knowledge.epoch,
                                            },
                                            Command::PutKnowledge {
                                                project_id: project.id.clone(),
                                                document,
                                            },
                                        )) {
                                            self.state
                                                .knowledge
                                                .put_pending
                                                .insert(project.id.clone());
                                        }
                                    }
                                    Err(error) => self.state.status = error,
                                },
                                knowledge_text_ui::Action::Search(query, offset) => {
                                    let request =
                                        self.state.knowledge.search_request.wrapping_add(1);
                                    if self.send(worker::Work::new(
                                        worker::Kind::KnowledgeSearch {
                                            project_id: project.id.clone(),
                                            epoch: self.state.knowledge.epoch,
                                            request,
                                            query: query.clone(),
                                            offset,
                                        },
                                        Command::SearchKnowledgePage {
                                            project_id: project.id.clone(),
                                            query,
                                            offset,
                                            limit: 20,
                                        },
                                    )) {
                                        self.state.knowledge.search_request = request;
                                    }
                                }
                                knowledge_text_ui::Action::Source(document_id) => {
                                    let request =
                                        self.state.knowledge.source_request.wrapping_add(1);
                                    if self.send(worker::Work::new(
                                        worker::Kind::KnowledgeSource {
                                            project_id: project.id.clone(),
                                            epoch: self.state.knowledge.epoch,
                                            request,
                                            document_id: document_id.clone(),
                                        },
                                        Command::GetKnowledgeSource {
                                            project_id: project.id.clone(),
                                            document_id,
                                        },
                                    )) {
                                        self.state.knowledge.source_request = request;
                                        if let Some(form) =
                                            self.state.knowledge.forms.get_mut(&project.id)
                                        {
                                            form.source_document = None;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        ui.label("프로젝트를 선택하세요.");
                    }
                    ui.separator();
                });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_startup
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "MAGI Desktop",
        eframe::NativeOptions::default(),
        Box::new(|cc| {
            let font_ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                if let Some(path) = std::env::var_os("WINDIR").map(|dir| {
                    std::path::PathBuf::from(dir)
                        .join("Fonts")
                        .join("malgun.ttf")
                }) {
                    if let Ok(bytes) = std::fs::read(path) {
                        let mut fonts = egui::FontDefinitions::default();
                        fonts
                            .font_data
                            .insert("korean".into(), egui::FontData::from_owned(bytes).into());
                        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                        {
                            if let Some(fonts) = fonts.families.get_mut(&family) {
                                fonts.push("korean".into());
                            }
                        }
                        font_ctx.set_fonts(fonts);
                        font_ctx.request_repaint();
                    }
                }
            });
            let (tx, rx, cancel_startup) = worker::start(cc.egui_ctx.clone());
            let mut app = App {
                state: State {
                    status: "엔진 연결 중".into(),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup,
            };
            app.send(worker::Work::new(worker::Kind::Status, Command::Status));
            app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
            app.poll_storage();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str) -> Project {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"path":"C:/work","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":null})).unwrap()
    }

    #[test]
    fn delayed_project_response_does_not_replace_new_selection() {
        let mut state = State::default();
        let old = state.select("a");
        let new = state.select("b");
        state.receive_project(new, project("b"));
        state.receive_project(old, project("a"));
        assert_eq!(state.project.unwrap().id, "b");
    }

    #[test]
    fn create_reply_preserves_newly_edited_input() {
        let mut state = State {
            name: "다음 이름".into(),
            path: "다음 경로".into(),
            ..Default::default()
        };
        state.created("이전 이름", "이전 경로", Ok(project("new")));
        assert_eq!(state.name, "다음 이름");
        assert_eq!(state.path, "다음 경로");
    }

    #[test]
    fn creation_failure_preserves_inputs_and_reports_reason() {
        let mut state = State {
            name: "이름".into(),
            path: "경로".into(),
            ..Default::default()
        };
        state.created("이름", "경로", Err("저장 실패".into()));
        assert_eq!(state.name, "이름");
        assert_eq!(state.path, "경로");
        assert!(state.status.contains("저장 실패"));
    }

    #[test]
    fn queue_full_does_not_clear_input_or_start_creation() {
        let (tx, _work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                name: "이름".into(),
                path: "경로".into(),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        assert!(app.send(worker::Work::new(worker::Kind::Status, Command::Status)));
        assert!(!app.send(worker::Work::new(worker::Kind::List, Command::ListProjects)));
        assert_eq!(app.state.name, "이름");
        assert!(!app.state.creating);
        assert!(app.state.status.contains("가득"));
    }

    #[test]
    fn startup_error_is_not_hidden_by_the_following_list_failure() {
        let (tx, _work) = std::sync::mpsc::sync_channel(2);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Status,
            result: Err("데이터베이스를 열 수 없음".into()),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::List,
            result: Err("연결 실패".into()),
        });
        assert!(app.state.status.contains("데이터베이스를 열 수 없음"));
    }

    #[test]
    fn unknown_storage_status_cancels_window_close() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
    }

    #[test]
    fn unsupported_storage_and_memory_only_results_block_close() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: Err("requested feature is not available".into()),
        });
        assert!(!app.state.storage.known);
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(app
            .state
            .storage
            .error
            .as_deref()
            .unwrap()
            .contains("지원하지 않습니다"));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("지원하지 않습니다"))));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":1,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
    }

    #[test]
    fn journaled_pending_requires_confirmation_before_close() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"worktree","state":"pending","attempts":1,"journaled":true,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close)),
            "저장 대기를 확인하기 전에 창이 닫혔습니다"
        );
        fn click(ctx: &egui::Context, app: &mut App, label: &str) {
            let input = || egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input(), |ctx| app.render(ctx));
            let output = ctx.run(input(), |ctx| app.render(ctx));
            let pos = output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::epaint::Shape::Text(text) if text.galley.job.text == label => {
                        Some(text.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .expect("선택 버튼이 보여야 합니다");
            let mut down = input();
            down.events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = ctx.run(down, |ctx| app.render(ctx));
            let mut up = input();
            up.events = vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }];
            let _ = ctx.run(up, |ctx| app.render(ctx));
        }
        click(&ctx, &mut app, "창 유지");
        assert!(!app.state.close_allowed && !app.state.close_confirmation);
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"worktree","state":"retrying","attempts":1,"journaled":true,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        click(&ctx, &mut app, "창만 닫기");
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn close_confirmation_cannot_bypass_new_unresolved_storage_state() {
        for (stage, journaled, local_retry) in [
            ("agent", false, false),
            ("unknown", true, false),
            ("agent", true, true),
        ] {
            let (tx, _requests) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.state.close_confirmation = true;
            if local_retry {
                app.state.storage.retries.insert("r".into());
            }
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
                "failures":[{"run_id":"r","project_id":"p","stage":stage,"state":"pending","attempts":1,"journaled":journaled,"message":"대기"}],
                "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
            }))) });
            let ctx = egui::Context::default();
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(!app.state.close_confirmation && !app.state.close_allowed);
            assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close)));
        }
    }

    #[test]
    fn closing_checks_fresh_storage_state_before_allowing_exit() {
        let (tx, requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose)));
        assert!(requests
            .try_iter()
            .any(|work| matches!(work.command, Command::ListStorageFailures)));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn close_waits_for_new_poll_when_older_poll_was_in_flight() {
        let (tx, requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let empty = || {
            Ok(ResponseData::Data(
                serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}),
            ))
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: empty(),
        });
        app.poll_storage();
        assert!(matches!(
            requests.try_recv().unwrap().command,
            Command::ListStorageFailures
        ));
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| app.render(ctx));
        app.apply(worker::Reply {
            kind: worker::Kind::Storage,
            result: empty(),
        });
        assert!(matches!(
            requests.try_recv().unwrap().command,
            Command::ListStorageFailures
        ));
        assert!(!app.state.close_allowed);
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":2,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close)));
    }

    #[test]
    fn storage_warning_and_retry_are_visible_across_project_switches() {
        let (tx, requests) = std::sync::mpsc::sync_channel(16);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"run-7","project_id":"project-a","stage":"agent","state":"pending","attempts":3,"journaled":false,"message":"저장 대기"}],
            "log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]
        }))) });
        app.state.select("project-b");
        let ctx = egui::Context::default();
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        frame(vec![]);
        let output = frame(vec![]);
        let visible = |text: &str| {
            output.shapes.iter().find_map(|clipped| {
                if let egui::epaint::Shape::Text(shape) = &clipped.shape {
                    let center = shape.visual_bounding_rect().center();
                    if shape.galley.job.text.contains(text) && clipped.clip_rect.contains(center) {
                        Some(center)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        };
        assert!(visible("아직 저장되지 않음").is_some());
        assert!(visible("run-7").is_some());
        assert!(visible("C:/temp/events.log").is_some());
        assert!(visible("진단 로그를 파일에 기록할 수 없습니다").is_some());
        let retry = visible("완료 저장 재시도").unwrap();
        frame(vec![
            egui::Event::PointerMoved(retry),
            egui::Event::PointerButton {
                pos: retry,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerButton {
            pos: retry,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(vec![]);
        frame(vec![
            egui::Event::PointerMoved(retry),
            egui::Event::PointerButton {
                pos: retry,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerButton {
            pos: retry,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let commands: Vec<_> = requests.try_iter().map(|work| work.command).collect();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(&commands[0], Command::RetryCompletionSave { run_id } if run_id == "run-7")
        );
        drop(frame);
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]}))) });
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장 재시도 중"))), "미확정 재시도 경고는 이전 진단으로 사라지면 안 됨");
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "run-7".into(),
            },
            result: Ok(ResponseData::Data(serde_json::json!({"state":"saved"}))),
        });
        assert!(requests
            .try_iter()
            .any(|work| matches!(work.command, Command::ListStorageFailures)));
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,"log_entries":[]}))) });
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("완료 결과 저장을 복구했습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("아직 저장되지 않음"))));
    }

    #[test]
    fn superseded_retry_does_not_claim_saved() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "run-old".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"state":"superseded"}),
            )),
        });
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("이전 결과를 적용하지 않았습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장을 복구했습니다"))));
    }

    #[test]
    fn startup_recovery_does_not_claim_a_computed_run_was_saved() {
        let (tx, _requests) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"diagnostic","project_id":"","stage":"startup","state":"pending","attempts":1,"journaled":false,"message":"저장소 초기화 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
        app.apply(worker::Reply {
            kind: worker::Kind::RetrySave {
                run_id: "diagnostic".into(),
            },
            result: Ok(ResponseData::Data(serde_json::json!({"state":"saved"}))),
        });
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("저장소 복구를 완료했습니다"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("완료 결과 저장을 복구했습니다"))));
    }

    #[test]
    fn storage_stage_and_logging_states_are_rendered_distinctly() {
        for (stage, state, label) in [
            ("agent", "retrying", "저장 재시도 중"),
            ("worktree", "cleanup_pending", "복구 기록 정리 대기"),
            (
                "unknown",
                "pending",
                "완료 복구 기록 또는 저장소를 확인할 수 없음",
            ),
            (
                "startup",
                "pending",
                "완료 복구 기록 또는 저장소를 확인할 수 없음",
            ),
        ] {
            let (tx, _work) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
                "failures":[{"run_id":"r","project_id":"p","stage":stage,"state":state,"attempts":1,"journaled":true,"message":"진단"}],
                "log_path":"C:/temp/events.log","log_available":false,"log_attempted":true,
                "log_entries":[{"timestamp_ms":1,"run_id":"r","stage":stage,"event":"storage_pending"}]
            }))) });
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains(label))), "{stage} {state}");
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("storage_pending"))));
            if matches!(stage, "unknown" | "startup") {
                assert!(!app.state.storage.may_close());
            }
        }
    }

    #[test]
    fn older_diagnostic_after_pending_retry_cannot_remove_warning() {
        for failed in [false, true] {
            let (tx, requests) = std::sync::mpsc::sync_channel(16);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(8);
            let mut app = App {
                state: State::default(),
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({
            "failures":[{"run_id":"r","project_id":"p","stage":"agent","state":"pending","attempts":1,"journaled":false,"message":"저장 대기"}],
            "log_path":"","log_available":true,"log_attempted":true,"log_entries":[]
        }))) });
            app.poll_storage();
            app.retry_storage("r");
            requests.try_iter().for_each(drop);
            let result = if failed {
                Err("저장 실패".into())
            } else {
                Ok(ResponseData::Data(serde_json::json!({"state":"pending"})))
            };
            app.apply(worker::Reply {
                kind: worker::Kind::RetrySave { run_id: "r".into() },
                result,
            });
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":true,"log_entries":[]}))) });
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text.contains("아직 저장되지 않음"))));
            assert!(!app.state.storage.may_close());
            assert!(requests
                .try_iter()
                .any(|work| matches!(work.command, Command::ListStorageFailures)));
        }
    }

    #[test]
    fn startup_progress_and_cancellation_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(2);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::Starting,
            result: Ok(ResponseData::Status {
                message: "엔진 시작 대기 중".into(),
            }),
        });
        assert!(app.state.starting);
        app.apply(worker::Reply {
            kind: worker::Kind::Status,
            result: Err("엔진 시작 대기가 취소됐습니다".into()),
        });
        assert!(!app.state.starting);
        assert!(app.state.status.contains("취소"));
    }

    #[test]
    fn selected_project_interview_is_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.interview.push("기존 인터뷰 답변".into());
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == "기존 인터뷰 답변")));
    }

    #[test]
    fn interview_input_and_submit_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == "응답 저장")));
    }

    #[test]
    fn interview_response_preserves_new_input_and_other_selection() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state
            .interview_inputs
            .insert("a".into(), "새 답변".into());
        app.state.interview_pending.insert("a".into());
        let generation = app.state.select("b");
        app.state.receive_project(generation, project("b"));
        let mut updated = project("a");
        updated.interview.push("이전 답변".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "이전 답변".into(),
            },
            result: Ok(ResponseData::Project(updated)),
        });
        assert_eq!(app.state.project.as_ref().unwrap().id, "b");
        assert_eq!(app.state.interview_inputs["a"], "새 답변");
        assert!(!app.state.interview_pending.contains("a"));
    }

    #[test]
    fn interview_failure_retains_input_and_success_invalidates_old_fetch() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let generation = app.state.select("a");
        app.state.interview_inputs.insert("a".into(), "답변".into());
        app.state.interview_pending.insert("a".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "답변".into(),
            },
            result: Err("저장 실패".into()),
        });
        assert_eq!(app.state.interview_inputs["a"], "답변");
        assert!(app.state.status.contains("저장 실패"));
        let mut updated = project("a");
        updated.interview.push("답변".into());
        app.apply(worker::Reply {
            kind: worker::Kind::Interview {
                project_id: "a".into(),
                message: "답변".into(),
            },
            result: Ok(ResponseData::Project(updated)),
        });
        app.state.receive_project(generation, project("a"));
        assert_eq!(app.state.project.as_ref().unwrap().interview, ["답변"]);
        assert!(!app.state.interview_inputs.contains_key("a"));
        assert!(app.state.status.contains("승인"));
    }

    #[test]
    fn interview_button_sends_selected_project_and_preserves_input_when_queue_full() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.interview_inputs.insert("a".into(), "답변".into());
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            )
        };
        let output = frame(&mut app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.job.text == "응답 저장" => {
                    Some(text.visual_bounding_rect().center())
                }
                _ => None,
            })
            .unwrap();
        let click = |app: &mut App| {
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
        assert!(
            matches!(work.try_recv().unwrap().command, Command::SubmitInterview { project_id, message } if project_id == "a" && message == "답변")
        );
        app.state.interview_pending.clear();
        app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
        click(&mut app);
        assert_eq!(app.state.interview_inputs["a"], "답변");
        assert!(!app.state.interview_pending.contains("a"));
        assert!(app.state.status.contains("가득"));
    }

    #[test]
    fn draft_displays_required_questions_and_failures() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.mandatory_questions.push("필수 질문 예시".into());
        p.draft.required_failures.push("필수 검증 실패 예시".into());
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        for expected in ["필수 질문 예시", "필수 검증 실패 예시"] {
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == expected)), "{expected}");
        }
    }

    #[test]
    fn spec_generation_button_sends_project_command() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "명세 초안 생성" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("명세 생성 버튼");
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
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GenerateSpec { project_id } if project_id == "a")
        );
        frame(
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
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
        assert!(work.try_recv().is_err());
    }

    #[test]
    fn synchronous_generation_updates_only_matching_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let old = app.state.generation;
        let mut updated = project("a");
        updated.draft.title = "생성된 초안".into();
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Project(updated)),
        });
        app.state.receive_project(old, project("a"));
        assert_eq!(
            app.state.project.as_ref().unwrap().draft.title,
            "생성된 초안"
        );
        app.state.select("b");
        app.state.project = Some(project("b"));
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Project(project("a"))),
        });
        assert_eq!(app.state.project.as_ref().unwrap().id, "b");
    }

    #[test]
    fn async_generation_requests_run_progress() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run {
                run_id: "run-1".into(),
            }),
        });
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GetRun { run_id } if run_id == "run-1")
        );
    }

    #[test]
    fn generated_run_refreshes_matching_project_and_failure_is_visible() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run {
                run_id: "run-1".into(),
            }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "run-1".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"run-1","project_id":"a","status":"generated"}),
            )),
        });
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GetProject { project_id } if project_id == "a")
        );
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Err("provider unavailable".into()),
        });
        assert!(app.state.status.contains("provider unavailable"));
    }

    #[test]
    fn failed_run_is_reported_without_changing_other_project() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"failed"}),
            )),
        });
        assert!(app.state.status.contains("실패"));
    }

    #[test]
    fn generating_run_is_polled_again() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"generating"}),
            )),
        });
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GetRun { run_id } if run_id == "r")
        );
    }

    #[test]
    fn old_project_generation_response_does_not_change_selected_project_status() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("b".into()),
                project: Some(project("b")),
                status: "현재 프로젝트".into(),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Err("old error".into()),
        });
        assert_eq!(app.state.status, "현재 프로젝트");
        assert_eq!(app.state.project.as_ref().unwrap().id, "b");
        app.state.select("a");
        app.state.project = Some(project("a"));
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("old error"))));
    }

    #[test]
    fn progress_queue_full_is_retried() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GetRun { run_id } if run_id == "r")
        );
    }

    #[test]
    fn completed_run_does_not_claim_refreshed_draft_when_queue_full() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.send(worker::Work::new(worker::Kind::List, Command::ListProjects));
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"generated"}),
            )),
        });
        assert!(app.state.status.contains("가득"));
    }

    #[test]
    fn failure_remains_visible_after_switching_back_to_project() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.state.select("b");
        app.state.project = Some(project("b"));
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"failed"}),
            )),
        });
        app.state.select("a");
        app.state.project = Some(project("a"));
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("명세 생성 실패"))));
    }

    #[test]
    fn draft_displays_constraints_and_sources() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.constraints.push("제약 예시".into());
        p.draft.source_links.push("출처 예시".into());
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        for expected in ["제약 예시", "출처 예시"] {
            assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == expected)), "{expected}");
        }
    }

    #[test]
    fn successful_generation_clears_previous_failure_notice() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Err("이전 오류".into()),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Project(project("a"))),
        });
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("이전 오류"))));
    }

    #[test]
    fn active_generation_is_visible_after_project_switch() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.state.select("b");
        app.state.project = Some(project("b"));
        app.state.status = "다른 상태".into();
        app.state.select("a");
        app.state.project = Some(project("a"));
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "명세 초안 생성 중")));
    }

    #[test]
    fn draft_shows_task_validation() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.tasks.push(domain::TaskSpec {
            id: "task-1".into(),
            dependencies: vec![],
            owned_paths: vec![],
            validation: vec!["검증 방법".into()],
        });
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(
            |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "검증 방법")
        ));
    }

    #[test]
    fn internal_queue_full_retries_progress_request() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Err("요청 처리 대기열이 가득 찼거나 종료됐습니다".into()),
        });
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(
            matches!(work.try_recv().unwrap().command, Command::GetRun { run_id } if run_id == "r")
        );
    }

    #[test]
    fn recovered_progress_clears_temporary_error() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Err("일시 오류".into()),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"generating"}),
            )),
        });
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("일시 오류"))));
    }

    #[test]
    fn completed_progress_clears_temporary_error() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.apply(worker::Reply {
            kind: worker::Kind::GenerateSpec {
                project_id: "a".into(),
            },
            result: Ok(ResponseData::Run { run_id: "r".into() }),
        });
        work.try_recv().unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Err("일시 오류".into()),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::SpecRun {
                project_id: "a".into(),
                run_id: "r".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"r","project_id":"a","status":"generated"}),
            )),
        });
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text.contains("일시 오류"))));
    }

    #[test]
    fn model_settings_are_visible_for_selected_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.model_settings = Some(domain::ModelSettings {
            provider: "mock".into(),
            model: "모델 예시".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(4096),
            },
        });
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "모델 설정 저장")));
        for expected in ["모델 예시", "4096"] {
            assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == expected)), "{expected}");
        }
    }

    #[test]
    fn model_save_button_sends_explicit_capabilities() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.model_settings = Some(domain::ModelSettings {
            provider: "mock".into(),
            model: "model".into(),
            capabilities: domain::ModelCapabilities {
                streaming: true,
                tools: false,
                context_tokens: Some(4096),
            },
        });
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 800.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "모델 설정 저장" => {
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
        assert!(
            matches!(work.try_recv().unwrap().command, Command::ConfigureModel { project_id, provider, model, capabilities } if project_id == "a" && provider == "mock" && model == "model" && capabilities.streaming && !capabilities.tools && capabilities.context_tokens == Some(4096))
        );
        app.state.model_pending.clear();
        assert!(app.send(worker::Work::new(worker::Kind::List, Command::ListProjects)));
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
        assert!(app.state.status.contains("가득"));
        assert!(!app.state.model_pending.contains("a"));
        let out = frame(&mut app, vec![]);
        assert!(out.shapes.iter().any(
            |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "model")
        ));
    }

    #[test]
    fn saved_model_response_updates_matching_project_only() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let submitted = app.state.model_forms["a"].clone();
        app.state.model_pending.insert("a".into());
        let old = app.state.generation;
        let mut updated = project("a");
        updated.context_version = 4;
        app.apply(worker::Reply {
            kind: worker::Kind::ModelSettings {
                project_id: "a".into(),
                submitted,
            },
            result: Ok(ResponseData::Project(updated)),
        });
        app.state.receive_project(old, project("a"));
        assert_eq!(app.state.project.as_ref().unwrap().context_version, 4);
        assert!(!app.state.model_pending.contains("a"));
        assert!(app.state.status.contains("저장"));
    }

    #[test]
    fn late_model_reply_preserves_newer_selected_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut current = project("a");
        current.context_version = 5;
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(current),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        let submitted = app.state.model_forms["a"].clone();
        let mut old = project("a");
        old.context_version = 3;
        app.apply(worker::Reply {
            kind: worker::Kind::ModelSettings {
                project_id: "a".into(),
                submitted,
            },
            result: Ok(ResponseData::Project(old)),
        });
        assert_eq!(app.state.project.as_ref().unwrap().context_version, 5);
    }

    #[test]
    fn runtime_credential_entry_is_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "런타임 키 설정")));
    }

    #[test]
    fn spec_editor_exposes_save_control() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(
            |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "초안 저장")
        ));
        assert!(out.shapes.iter().any(
            |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.job.text == "작업 JSON")
        ));
    }

    #[test]
    fn spec_save_sends_current_draft() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.title = "저장할 제목".into();
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
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
        assert!(
            matches!(work.try_recv().unwrap().command, Command::SaveDraft {project_id,draft} if project_id == "a" && draft.title == "저장할 제목")
        );
        app.state.spec_review_pending.clear();
        assert!(app.send(worker::Work::new(worker::Kind::List, Command::ListProjects)));
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
        assert!(app.state.status.contains("가득"));
        assert!(!app.state.spec_review_pending.contains("a"));
        let out = frame(&mut app, vec![]);
        assert!(out.shapes.iter().any(
            |s| matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text=="저장할 제목")
        ));
    }

    #[test]
    fn spec_approval_sends_viewed_draft_identifier() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.title = "제목".into();
        p.draft.base_revision = "rev".into();
        p.draft.scope = vec!["src".into()];
        p.draft.requirements = vec!["요구".into()];
        p.draft.acceptance = vec!["기준".into()];
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "명세 승인" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("승인 버튼");
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
        assert!(
            matches!(work.try_recv().unwrap().command, Command::ApproveSpec {project_id,expected_draft_hash,..} if project_id == "a" && expected_draft_hash == "h")
        );
    }

    #[test]
    fn approval_is_blocked_while_interview_is_saving() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut p = project("a");
        p.draft.title = "제목".into();
        p.draft.base_revision = "rev".into();
        p.draft.scope = vec!["src".into()];
        p.draft.requirements = vec!["요구".into()];
        p.draft.acceptance = vec!["기준".into()];
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(p),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.interview_pending.insert("a".into());
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "명세 승인" => {
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
        assert!(work.try_recv().is_err());
    }

    #[test]
    fn saved_draft_response_updates_selected_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.spec_review_pending.insert("a".into());
        let old = app.state.generation;
        let mut saved = project("a");
        saved.draft_hash = "new".into();
        saved.draft.title = "저장된 명세".into();
        app.apply(worker::Reply {
            kind: worker::Kind::SaveDraft {
                project_id: "a".into(),
                base_hash: "h".into(),
                context: 0,
                generation: old,
            },
            result: Ok(ResponseData::Project(saved)),
        });
        app.state.receive_project(old, project("a"));
        assert_eq!(app.state.project.as_ref().unwrap().draft_hash, "new");
        assert!(!app.state.spec_review_pending.contains("a"));
    }

    #[test]
    fn late_save_reply_does_not_replace_newer_draft() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut newer = project("a");
        newer.draft_hash = "newer".into();
        newer.draft.title = "새 버전".into();
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(newer),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let mut older = project("a");
        older.draft_hash = "saved-old".into();
        app.apply(worker::Reply {
            kind: worker::Kind::SaveDraft {
                project_id: "a".into(),
                base_hash: "h".into(),
                context: 0,
                generation: 0,
            },
            result: Ok(ResponseData::Project(older)),
        });
        assert_eq!(app.state.project.as_ref().unwrap().draft_hash, "newer");
        assert!(app.state.status.contains("다시 확인"));
    }

    #[test]
    fn approved_response_marks_reviewed_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.spec_review_pending.insert("a".into());
        let approved: domain::ApprovedSpec=serde_json::from_value(serde_json::json!({"version":1,"hash":"h","base_revision":"rev","scope":[],"requirements":[],"acceptance":[]})).unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::ApproveDraft {
                project_id: "a".into(),
                context: 0,
                generation: 0,
            },
            result: Ok(ResponseData::Approved(approved)),
        });
        assert!(app.state.project.as_ref().unwrap().approved.is_some());
        assert!(!app.state.spec_review_pending.contains("a"));
    }

    #[test]
    fn late_approval_does_not_mark_a_newer_draft_approved() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut newer = project("a");
        newer.draft_hash = "new".into();
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(newer),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let approved:domain::ApprovedSpec=serde_json::from_value(serde_json::json!({"version":1,"hash":"h","base_revision":"rev","scope":[],"requirements":[],"acceptance":[]})).unwrap();
        app.apply(worker::Reply {
            kind: worker::Kind::ApproveDraft {
                project_id: "a".into(),
                context: 0,
                generation: 0,
            },
            result: Ok(ResponseData::Approved(approved)),
        });
        assert_eq!(app.state.project.as_ref().unwrap().draft_hash, "new");
        assert!(app.state.project.as_ref().unwrap().approved.is_none());
    }

    #[test]
    fn run_history_controls_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text=="실행 기록 조회")));
    }

    #[test]
    fn history_button_requests_selected_project_page() {
        let (tx, work) = std::sync::mpsc::sync_channel(1);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 700.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "실행 기록 조회" => {
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
        let first = work.try_recv().unwrap();
        assert!(
            matches!(first.command,Command::ListRuns{project_id,offset:0,limit:20} if project_id=="a")
        );
        assert!(app.send(worker::Work::new(worker::Kind::List, Command::ListProjects)));
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
        assert!(app.state.status.contains("가득"));
        app.apply(worker::Reply{kind:first.kind,result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"kept","project_id":"a","status":"running"}],"total":1,"offset":0,"limit":20}))) });
        assert_eq!(app.state.history.runs[0]["id"], "kept");
    }

    #[test]
    fn history_page_displays_runs_for_current_project() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply{kind:worker::Kind::HistoryPage{project_id:"a".into(),request,offset:0},result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"run-1","project_id":"a","status":"completed","persistence":null}],"total":1,"offset":0,"limit":20}))) });
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("run-1"))));
    }

    #[test]
    fn history_next_page_requests_next_offset() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryPage {
                project_id: "a".into(),
                request,
                offset: 0,
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"runs":[],"total":41,"offset":0,"limit":20}),
            )),
        });
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 700.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text == "다음 페이지" => {
                    Some(t.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("다음 페이지");
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
        assert!(
            matches!(work.try_recv().unwrap().command,Command::ListRuns{project_id,offset:20,limit:20} if project_id=="a")
        );
    }

    #[test]
    fn selecting_run_requests_detail_and_events() {
        let (tx, work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply{kind:worker::Kind::HistoryPage{project_id:"a".into(),request,offset:0},result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"run-1","project_id":"a","status":"running"}],"total":1,"offset":0,"limit":20}))) });
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 700.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text.contains("run-1") => {
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
        assert!(
            matches!(work.try_recv().unwrap().command,Command::GetRun{run_id} if run_id=="run-1")
        );
        assert!(
            matches!(work.try_recv().unwrap().command,Command::ListEvents{run_id,after:0} if run_id=="run-1")
        );
    }

    #[test]
    fn selected_run_shows_events_and_unsaved_completion() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("r".into());
        let request = app.state.history.selection_request;
        app.apply(worker::Reply{kind:worker::Kind::HistoryDetail{project_id:"a".into(),run_id:"r".into(),request},result:Ok(ResponseData::Data(serde_json::json!({"id":"r","project_id":"a","status":"completed","persistence":{"state":"pending"}})))});
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryEvents {
                project_id: "a".into(),
                run_id: "r".into(),
                request,
            },
            result: Ok(ResponseData::Events(vec![domain::RunEvent {
                sequence: 1,
                run_id: "r".into(),
                attempt: 1,
                kind: "progress".into(),
                message: "이벤트 내용".into(),
            }])),
        });
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("결과 저장 대기"))));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("이벤트 내용"))));
    }

    #[test]
    fn project_switch_invalidates_old_history_response_even_when_switched_back() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.select("a");
        app.state.history.project("a");
        let request = app.state.history.request;
        app.state.select("b");
        app.state.select("a");
        app.state.project = Some(project("a"));
        app.apply(worker::Reply{kind:worker::Kind::HistoryPage{project_id:"a".into(),request,offset:0},result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"old","project_id":"a","status":"completed"}],"total":1,"offset":0,"limit":20}))) });
        assert!(app.state.history.runs.is_empty());
    }

    #[test]
    fn selecting_another_run_clears_prior_error_and_detail() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply{kind:worker::Kind::HistoryPage{project_id:"a".into(),request,offset:0},result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"new","project_id":"a","status":"running"}],"total":1,"offset":0,"limit":20}))) });
        app.state.history.error = Some("기존 오류".into());
        app.state.history.detail = Some(serde_json::json!({"id":"old","status":"completed"}));
        let ctx = egui::Context::default();
        let frame = |app: &mut App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 700.),
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
                egui::epaint::Shape::Text(t) if t.galley.job.text.contains("new") => {
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
        assert!(app.state.history.detail.is_none());
        assert!(app.state.history.error.is_none());
    }

    #[test]
    fn late_run_detail_and_events_do_not_replace_current_selection() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        app.state.history.selected = Some("new".into());
        app.state.history.selection_request = 2;
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryDetail {
                project_id: "a".into(),
                run_id: "old".into(),
                request: 1,
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"old","project_id":"a","status":"completed"}),
            )),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryEvents {
                project_id: "a".into(),
                run_id: "old".into(),
                request: 1,
            },
            result: Ok(ResponseData::Events(vec![domain::RunEvent {
                sequence: 1,
                run_id: "old".into(),
                attempt: 1,
                kind: "test".into(),
                message: "old".into(),
            }])),
        });
        assert_eq!(app.state.history.selected.as_deref(), Some("new"));
        assert!(app.state.history.detail.is_none());
        assert!(app.state.history.events.is_empty());
    }

    #[test]
    fn history_empty_page_and_failure_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryPage {
                project_id: "a".into(),
                request,
                offset: 0,
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"runs":[],"total":0,"offset":0,"limit":20}),
            )),
        });
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text=="실행 기록이 없습니다.")));
        app.apply(worker::Reply {
            kind: worker::Kind::HistoryPage {
                project_id: "a".into(),
                request,
                offset: 0,
            },
            result: Err("임시 오류".into()),
        });
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("임시 오류"))));
    }

    #[test]
    fn malformed_history_page_is_not_shown_as_valid() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.history.project("a");
        let request = app.state.history.request;
        app.apply(worker::Reply{kind:worker::Kind::HistoryPage{project_id:"a".into(),request,offset:0},result:Ok(ResponseData::Data(serde_json::json!({"runs":[{"id":"r","project_id":"a","status":"running"}],"total":1,"offset":0,"limit":99}))) });
        assert!(app.state.history.runs.is_empty());
        assert!(app
            .state
            .history
            .error
            .as_deref()
            .unwrap_or("")
            .contains("올바르지"));
    }

    #[test]
    fn knowledge_text_controls_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        for expected in [
            "원문 저장",
            "텍스트 검색",
            "문서 ID",
            "버전",
            "출처",
            "원문",
            "검색어",
        ] {
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text==expected)),"{expected}");
        }
    }

    #[test]
    fn knowledge_search_displays_document_hits() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.knowledge.select("a");
        let epoch = app.state.knowledge.epoch;
        let request = app.state.knowledge.search_request;
        app.apply(worker::Reply{kind:worker::Kind::KnowledgeSearch{project_id:"a".into(),epoch,request,query:"검색".into(),offset:0},result:Ok(ResponseData::Data(serde_json::json!({"mode":"text","results":[{"id":"doc1","version":1,"source":"가상 출처","text":"첫 줄\n둘째 줄"}]})))});
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("doc1"))));
    }

    #[test]
    fn knowledge_source_displays_exact_multiline_text_and_provenance() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.knowledge.select("a");
        let epoch = app.state.knowledge.epoch;
        let request = app.state.knowledge.source_request;
        app.apply(worker::Reply{kind:worker::Kind::KnowledgeSource{project_id:"a".into(),epoch,request,document_id:"doc".into()},result:Ok(ResponseData::Data(serde_json::json!({"id":"doc","version":2,"source":"출처 예시","text":"첫 줄\n  다음 줄"})))});
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        for expected in ["출처 예시", "첫 줄\n  다음 줄"] {
            assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text==expected)),"{expected}");
        }
    }

    #[test]
    fn knowledge_empty_results_and_source_error_are_visible() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.knowledge.select("a");
        let epoch = app.state.knowledge.epoch;
        app.apply(worker::Reply {
            kind: worker::Kind::KnowledgeSearch {
                project_id: "a".into(),
                epoch,
                request: 0,
                query: "없는 단어".into(),
                offset: 0,
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"mode":"text","results":[]}),
            )),
        });
        let ctx = egui::Context::default();
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text=="검색 결과가 없습니다.")));
        app.apply(worker::Reply {
            kind: worker::Kind::KnowledgeSource {
                project_id: "a".into(),
                epoch,
                request: 0,
                document_id: "doc".into(),
            },
            result: Err("document not found".into()),
        });
        let out = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        assert!(out.shapes.iter().any(|s|matches!(&s.shape,egui::epaint::Shape::Text(t) if t.galley.job.text.contains("document not found"))));
    }
    #[test]
    fn older_source_reply_does_not_replace_newer_selection() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_rt, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State {
                selected: Some("a".into()),
                project: Some(project("a")),
                ..Default::default()
            },
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        app.state.knowledge.select("a");
        let epoch = app.state.knowledge.epoch;
        app.state.knowledge.source_request = 2;
        app.apply(worker::Reply {
            kind: worker::Kind::KnowledgeSource {
                project_id: "a".into(),
                epoch,
                request: 2,
                document_id: "new".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"new","version":1,"source":"s","text":"new"}),
            )),
        });
        app.apply(worker::Reply {
            kind: worker::Kind::KnowledgeSource {
                project_id: "a".into(),
                epoch,
                request: 1,
                document_id: "old".into(),
            },
            result: Ok(ResponseData::Data(
                serde_json::json!({"id":"old","version":1,"source":"s","text":"old"}),
            )),
        });
        assert_eq!(
            app.state.knowledge.forms["a"]
                .source_document
                .as_ref()
                .unwrap()["id"],
            "new"
        );
    }

    #[test]
    fn shell_renders_headlessly_at_multiple_scales() {
        let (tx, _work) = std::sync::mpsc::sync_channel(8);
        let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut app = App {
            state: State::default(),
            tx,
            rx,
            cancel_startup: Default::default(),
        };
        for scale in [1.0, 2.0, 3.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        }
    }

    #[test]
    fn long_project_list_allows_selecting_last_and_creating_project() {
        for size in [egui::vec2(800.0, 600.0), egui::vec2(320.0, 240.0)] {
            let (tx, work) = std::sync::mpsc::sync_channel(8);
            let (_reply_tx, rx) = std::sync::mpsc::sync_channel(1);
            let mut app = App {
                state: State {
                    projects: (0..40).map(|i| project(&format!("project-{i}"))).collect(),
                    name: "new".into(),
                    path: "C:/work".into(),
                    ..Default::default()
                },
                tx,
                rx,
                cancel_startup: Default::default(),
            };
            app.apply(worker::Reply { kind: worker::Kind::Storage, result: Ok(ResponseData::Data(serde_json::json!({"failures":[],"log_path":"","log_available":true,"log_attempted":false,"log_entries":[]}))) });
            let ctx = egui::Context::default();
            let mut time = 0.0;
            let mut frame = |events: Vec<egui::Event>, app: &mut App| {
                time += 0.1;
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ctx| app.render(ctx),
                )
            };
            frame(
                vec![egui::Event::PointerMoved(egui::pos2(80.0, 100.0))],
                &mut app,
            );
            let mut output = frame(
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -3000.0),
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            for _ in 0..30 {
                output = frame(vec![], &mut app);
            }
            let visible = |output: &egui::FullOutput, label: &str| -> Option<egui::Pos2> {
                output.shapes.iter().find_map(|clipped| {
                    if let egui::epaint::Shape::Text(text) = &clipped.shape {
                        let center = text.visual_bounding_rect().center();
                        if text.galley.job.text == label
                            && clipped.clip_rect.contains(center)
                            && center.x >= 0.0
                            && center.x < size.x
                            && center.y >= 0.0
                            && center.y < size.y
                        {
                            return Some(center);
                        }
                    }
                    None
                })
            };
            let last =
                visible(&output, "project-39").expect("마지막 프로젝트가 보이고 선택 가능해야 함");
            frame(
                vec![
                    egui::Event::PointerMoved(last),
                    egui::Event::PointerButton {
                        pos: last,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            frame(
                vec![egui::Event::PointerButton {
                    pos: last,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            assert_eq!(app.state.selected.as_deref(), Some("project-39"));
            frame(
                vec![
                    egui::Event::PointerMoved(egui::pos2(80.0, 100.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -3000.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            for _ in 0..30 {
                output = frame(vec![], &mut app);
            }
            let create =
                visible(&output, "생성").expect("프로젝트 생성 버튼이 보이고 클릭 가능해야 함");
            frame(
                vec![
                    egui::Event::PointerMoved(create),
                    egui::Event::PointerButton {
                        pos: create,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut app,
            );
            frame(
                vec![egui::Event::PointerButton {
                    pos: create,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut app,
            );
            assert_eq!(app.state.selected.as_deref(), Some("project-39"));
            assert!(matches!(
                work.try_recv().unwrap().command,
                Command::GetProject { .. }
            ));
            assert!(matches!(
                work.try_recv().unwrap().command,
                Command::CreateProject { .. }
            ));
        }
    }
}
