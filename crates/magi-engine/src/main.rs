#[cfg(windows)]
use magi_engine::Engine;
#[cfg(windows)]
use std::{path::PathBuf, sync::Arc};
#[cfg(windows)]
use tokio::sync::{mpsc, oneshot};

// 2026-11-15: 제한된 대기열과 별도 Status 용량으로 로컬 요청의 자원 사용을 제한한다.
#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = match (args.next(), args.next()) {
        (Some(flag), Some(path)) if flag == "--database" => PathBuf::from(path),
        _ => return Err("usage: magi-engine --database <path> [--pipe <name>]".into()),
    };
    let pipe_name = match (args.next(), args.next(), args.next()) {
        (None, None, None) => magi_transport::default_pipe_name(),
        (Some(flag), Some(name), None) if flag == "--pipe" => {
            name.into_string().map_err(|_| "invalid pipe name")?
        }
        _ => return Err("usage: magi-engine --database <path> [--pipe <name>]".into()),
    };
    let engine = Arc::new(Engine::open(&path).map_err(std::io::Error::other)?);
    let (tx, mut rx) =
        mpsc::channel::<(magi_core::Request, oneshot::Sender<magi_core::Response>)>(32);
    tokio::spawn(async move {
        let regular = Arc::new(tokio::sync::Semaphore::new(16));
        let control = Arc::new(tokio::sync::Semaphore::new(4));
        while let Some((request, reply)) = rx.recv().await {
            let pool = if matches!(&request.command, magi_core::Command::Status) {
                &control
            } else {
                &regular
            };
            let Ok(permit) = Arc::clone(pool).try_acquire_owned() else {
                let _ = reply.send(magi_core::Response {
                    id: request.id,
                    result: Err("engine is busy".into()),
                });
                continue;
            };
            let engine = Arc::clone(&engine);
            tokio::spawn(async move {
                let response = engine.handle_async(request).await;
                let _ = reply.send(response);
                drop(permit);
            });
        }
    });
    magi_transport::serve(&pipe_name, tx).await?;
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("magi-engine host requires Windows");
    std::process::exit(1);
}
