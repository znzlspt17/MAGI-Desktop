use eframe::egui;
use magi_core::{Command, Request, ResponseData};
use std::sync::mpsc::{self, Receiver, SyncSender};

pub enum Kind {
    Status,
    List,
    Get { generation: u64 },
    Create { name: String, path: String },
}

pub struct Work {
    pub kind: Kind,
    pub command: Command,
}
impl Work {
    pub fn new(kind: Kind, command: Command) -> Self {
        Self { kind, command }
    }
}
pub struct Reply {
    pub kind: Kind,
    pub result: Result<ResponseData, String>,
}

pub fn start(ctx: egui::Context) -> (SyncSender<Work>, Receiver<Reply>) {
    start_with_pipe(ctx, magi_transport::default_pipe_name())
}
fn start_with_pipe(ctx: egui::Context, pipe: String) -> (SyncSender<Work>, Receiver<Reply>) {
    let (tx, work) = mpsc::sync_channel::<Work>(32);
    let (reply_tx, rx) = mpsc::sync_channel::<Reply>(64);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = reply_tx.send(Reply {
                    kind: Kind::Status,
                    result: Err(format!("IPC 실행기 시작 실패: {e}")),
                });
                ctx.request_repaint();
                return;
            }
        };
        let mut id = 0u64;
        while let Ok(work) = work.recv() {
            id = id.wrapping_add(1);
            let request = Request {
                id: id.to_string(),
                command: work.command,
            };
            let result = match runtime.block_on(magi_transport::request(&pipe, &request)) {
                Ok(reply) => reply.result,
                Err(error) => Err(format!("엔진 연결 실패: {error:#}")),
            };
            if reply_tx
                .send(Reply {
                    kind: work.kind,
                    result,
                })
                .is_err()
            {
                break;
            }
            ctx.request_repaint();
        }
    });
    (tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn reconnects_and_creates_then_reads_project_through_real_pipe() {
        let name = format!(r"\\.\pipe\magi-desktop-test-{}", std::process::id());
        let (tx, rx) = start_with_pipe(egui::Context::default(), name.clone());
        tx.send(Work::new(Kind::Status, Command::Status)).unwrap();
        assert!(rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .result
            .is_err());
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("magi-desktop-test-{}-{suffix}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let engine = Arc::new(magi_engine::Engine::open(&dir.join("test.redb")).unwrap());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (requests, mut pending) = tokio::sync::mpsc::channel::<(Request, tokio::sync::oneshot::Sender<magi_core::Response>)>(32);
                tokio::spawn(async move {
                    while let Some((request, reply)) = pending.recv().await {
                        let _ = reply.send(engine.clone().handle_async(request).await);
                    }
                });
                tokio::select! { result = magi_transport::serve(&name, requests) => { result.unwrap(); }, _ = stop_rx => {} }
            });
        });
        let mut connected = false;
        for _ in 0..50 {
            tx.send(Work::new(Kind::Status, Command::Status)).unwrap();
            if rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .result
                .is_ok()
            {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(connected);
        tx.send(Work::new(
            Kind::Create {
                name: "테스트".into(),
                path: "C:/work".into(),
            },
            Command::CreateProject {
                name: "테스트".into(),
                path: "C:/work".into(),
            },
        ))
        .unwrap();
        let project = match rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .result
            .unwrap()
        {
            ResponseData::Project(p) => p,
            _ => panic!("unexpected response"),
        };
        tx.send(Work::new(Kind::List, Command::ListProjects))
            .unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap().result.unwrap(), ResponseData::Projects(p) if p.iter().any(|p| p.id == project.id))
        );
        tx.send(Work::new(
            Kind::Get { generation: 1 },
            Command::GetProject {
                project_id: project.id.clone(),
            },
        ))
        .unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap().result.unwrap(), ResponseData::Project(p) if p.id == project.id)
        );
        drop(tx);
        let _ = stop_tx.send(());
        thread.join().unwrap();
        std::fs::remove_file(dir.join("test.redb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
