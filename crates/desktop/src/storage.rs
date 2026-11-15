use serde::Deserialize;
use std::{collections::HashSet, time::Instant};

#[derive(Clone, Deserialize)]
pub struct Failure {
    pub run_id: String,
    pub project_id: String,
    pub stage: String,
    pub state: String,
    pub attempts: u64,
    pub journaled: bool,
    pub message: String,
}

#[derive(Clone, Deserialize)]
pub struct Snapshot {
    pub failures: Vec<Failure>,
    pub log_path: String,
    pub log_available: bool,
    pub log_attempted: bool,
    pub log_entries: Vec<serde_json::Value>,
}

impl super::App {
    pub(super) fn storage_panel(&mut self, ctx: &eframe::egui::Context) {
        use eframe::egui;
        let storage = &self.state.storage;
        let has_failures = storage
            .snapshot
            .as_ref()
            .is_some_and(|s| !s.failures.is_empty());
        let log_failed = storage
            .snapshot
            .as_ref()
            .is_some_and(|s| s.log_attempted && !s.log_available);
        if storage.known && !has_failures && !log_failed && storage.notice.is_none() {
            return;
        }
        egui::TopBottomPanel::top("storage-recovery")
            .exact_height((ctx.screen_rect().height() * 0.35).min(220.0))
            .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if !self.state.storage.known {
                    ui.colored_label(egui::Color32::YELLOW, "완료 저장 상태를 확인할 수 없습니다. 상태를 확인할 때까지 창을 닫을 수 없습니다.");
                    if let Some(error) = &self.state.storage.error { ui.label(error); }
                    if ui.add_enabled(!self.state.storage.pending_poll, egui::Button::new("저장 상태 다시 확인")).clicked() { self.poll_storage(); }
                }
                if let Some(notice) = self.state.storage.notice.clone() {
                    ui.horizontal(|ui| {
                        ui.label(notice);
                        if ui.button("확인").clicked() { self.state.storage.notice = None; }
                    });
                }
                if let Some(snapshot) = self.state.storage.snapshot.clone() {
                    for failure in snapshot.failures {
                        let retrying = self.state.storage.retries.contains(&failure.run_id) || failure.state == "retrying";
                        let label = if matches!(failure.stage.as_str(), "unknown" | "startup") {
                            "완료 복구 기록 또는 저장소를 확인할 수 없음"
                        } else if retrying {
                            "결과 산출됨 - 저장 재시도 중"
                        } else if failure.state == "cleanup_pending" {
                            "처리 확정 - 결과 적용 여부 확인 및 복구 기록 정리 대기"
                        } else {
                            "결과 산출됨 - 아직 저장되지 않음"
                        };
                        ui.colored_label(egui::Color32::YELLOW, format!("{label} | 실행: {} | 프로젝트: {} | 단계: {} | 시도: {}", failure.run_id, failure.project_id, failure.stage, failure.attempts));
                        ui.label(&failure.message);
                        if !failure.journaled && failure.state != "cleanup_pending" && !matches!(failure.stage.as_str(), "unknown" | "startup") {
                            ui.colored_label(egui::Color32::LIGHT_RED, "결과가 메모리에만 있습니다. 엔진 종료 시 보존을 보장할 수 없습니다.");
                        }
                        let button = if matches!(failure.stage.as_str(), "unknown" | "startup") { "저장소 복구 재시도" } else { "완료 저장 재시도" };
                        if !failure.run_id.is_empty() && !retrying && ui.button(button).clicked() { self.retry_storage(&failure.run_id); }
                    }
                    if !snapshot.log_path.is_empty() { ui.label(format!("진단 로그 경로: {}", snapshot.log_path)); }
                    if snapshot.log_attempted && !snapshot.log_available {
                        ui.colored_label(egui::Color32::YELLOW, "진단 로그를 파일에 기록할 수 없습니다. 아래는 엔진 메모리의 최근 진단입니다.");
                        for entry in snapshot.log_entries {
                            ui.label(format!("{} | 실행: {} | 단계: {} | 이벤트: {}", entry["timestamp_ms"], entry["run_id"].as_str().unwrap_or(""), entry["stage"].as_str().unwrap_or(""), entry["event"].as_str().unwrap_or("")));
                        }
                    }
                }
            });
        });
    }
}

pub struct Storage {
    pub snapshot: Option<Snapshot>,
    pub known: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub pending_poll: bool,
    pub poll_again: bool,
    pub retries: HashSet<String>,
    pub protected: HashSet<String>,
    pub last_poll: Instant,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            snapshot: None,
            known: false,
            error: None,
            notice: None,
            pending_poll: false,
            poll_again: false,
            retries: HashSet::new(),
            protected: HashSet::new(),
            last_poll: Instant::now(),
        }
    }
}

impl Storage {
    pub fn may_close(&self) -> bool {
        self.known
            && !self.pending_poll
            && self.retries.is_empty()
            && self.protected.is_empty()
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                !snapshot.failures.iter().any(|f| {
                    f.stage == "unknown"
                        || f.stage == "startup"
                        || (!f.journaled && f.state != "cleanup_pending")
                })
            })
    }
}

impl Snapshot {
    pub fn valid(&self) -> bool {
        self.failures.iter().all(|f| {
            matches!(
                f.stage.as_str(),
                "generation" | "agent" | "worktree" | "unknown" | "startup"
            ) && matches!(f.state.as_str(), "pending" | "retrying" | "cleanup_pending")
                && (!f.run_id.is_empty() || matches!(f.stage.as_str(), "unknown" | "startup"))
        })
    }
}
