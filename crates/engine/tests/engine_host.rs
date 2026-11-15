use domain::{Command, Request, ResponseData};
use engine::Engine;

#[test]
fn status_reports_only_current_engine_capabilities() {
    let dir = std::env::temp_dir().join(format!("magi-host-status-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let db = dir.join("state.redb");
    let engine = Engine::open(&db).unwrap();
    let response = engine.handle(Request {
        id: "status-1".into(),
        command: Command::Status,
    });
    assert_eq!(response.id, "status-1");
    match response.result.unwrap() {
        ResponseData::Status { message } => assert_eq!(
            message,
            "engine ready; worktree preparation, spec generation, agent execution and storage recovery available; agent resume and local inference unavailable"
        ),
        _ => panic!("expected engine status"),
    }
    drop(engine);
    std::fs::remove_file(db).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn process_serves_status_and_rejects_unavailable_commands() {
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command as Process, Stdio};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let binary = env!("CARGO_BIN_EXE_engine");
    let dir = std::env::temp_dir().join(format!("magi-host-ipc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let database = dir.join("state.redb");
    let pipe = format!(r"\\.\pipe\magi-host-test-{}", uuid::Uuid::new_v4());
    let child = Process::new(binary)
        .creation_flags(0x08000000)
        .arg("--database")
        .arg(&database)
        .arg("--pipe")
        .arg(&pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let request = Request {
        id: "ipc-status".into(),
        command: Command::Status,
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let response = loop {
        if let Ok(response) = transport::request(&pipe, &request).await {
            break response;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "engine exited before accepting requests"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "engine did not become ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    };
    assert_eq!(response.id, request.id);
    assert!(matches!(response.result, Ok(ResponseData::Status { .. })));
    let mut concurrent = tokio::task::JoinSet::new();
    for number in 0..16 {
        let pipe = pipe.clone();
        concurrent.spawn(async move {
            let id = format!("status-{number}");
            let response = transport::request(
                &pipe,
                &Request {
                    id: id.clone(),
                    command: Command::Status,
                },
            )
            .await
            .unwrap();
            assert_eq!(response.id, id);
            match response.result {
                Ok(ResponseData::Status { .. }) => {}
                Err(error) => assert_eq!(error, "engine is busy"),
                _ => panic!("unexpected response"),
            }
        });
    }
    while let Some(result) = concurrent.join_next().await {
        result.unwrap();
    }
    let request = Request {
        id: "unsupported".into(),
        command: Command::ListHistory {
            project_id: "fixture".into(),
        },
    };
    let response = transport::request(&pipe, &request).await.unwrap();
    assert_eq!(response.id, request.id);
    assert_eq!(
        response.result.unwrap_err(),
        "requested feature is not available"
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(transport::request(
        &pipe,
        &Request {
            id: "after-stop".into(),
            command: Command::Status
        }
    )
    .await
    .is_err());
    drop(child);
    std::fs::remove_file(database).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[cfg(windows)]
#[test]
fn process_rejects_missing_or_extra_arguments() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command as Process, Stdio};
    let binary = env!("CARGO_BIN_EXE_engine");
    for args in [vec![], vec!["--database", "unused.redb", "--extra"]] {
        let status = Process::new(binary)
            .creation_flags(0x08000000)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
    }
}
