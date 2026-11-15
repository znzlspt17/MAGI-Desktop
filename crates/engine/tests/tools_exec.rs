use engine::tools::{execute_tool, ToolContext};
use serde_json::json;
use std::{fs, time::Duration};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
#[test]
fn exec_fixture() {
    if std::path::Path::new("enable").exists() {
        fs::write("ran", "fixture").unwrap();
        print!("fixture output");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn only_approved_allowed_exec_starts_child() {
    let root = std::env::temp_dir().join(format!("magi-exec-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(root.join("src/enable"), "").unwrap();
    let mut ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec![],
        timeout: Duration::from_secs(10),
        max_output_bytes: 4096,
        side_effect_authorized: false,
    };
    let args = json!({"program":std::env::current_exe().unwrap().to_string_lossy(),"args":["--exact","exec_fixture","--nocapture"],"cwd":"src"});
    assert!(
        execute_tool(ctx.clone(), "exec", args.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    ctx.allowlist.push("exec".into());
    assert!(
        execute_tool(ctx.clone(), "exec", args.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!root.join("src/ran").exists());
    ctx.side_effect_authorized = true;
    let result = execute_tool(ctx, "exec", args, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.content["stdout"]
        .as_str()
        .unwrap()
        .contains("fixture output"));
    assert!(root.join("src/ran").exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn job_child_helper() {
    use std::{os::windows::process::CommandExt, process::Command};
    if !std::path::Path::new("enable").exists() {
        return;
    }
    fs::write("started", "yes").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "job_grandchild_helper", "--nocapture"])
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
    fs::write("pids", format!("{} {}", std::process::id(), child.id())).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    let _ = child.wait();
}
#[cfg(windows)]
#[test]
fn job_grandchild_helper() {
    if std::path::Path::new("enable").exists() {
        std::thread::sleep(Duration::from_secs(60));
    }
}
#[cfg(windows)]
#[tokio::test]
async fn timeout_terminates_process_and_descendant() {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let root = std::env::temp_dir().join(format!("magi-tools-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(root.join("src/enable"), "").unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["exec".into()],
        timeout: Duration::from_secs(4),
        max_output_bytes: 4096,
        side_effect_authorized: true,
    };
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let result = execute_tool(
        ctx,
        "exec",
        json!({"program":exe,"args":["--exact","job_child_helper","--nocapture"],"cwd":"src"}),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err(), "{result:?}");
    let pids = fs::read_to_string(root.join("src/pids")).unwrap_or_else(|e| {
        panic!(
            "{e}; result {result:?}; started {}",
            root.join("src/started").exists()
        )
    });
    for pid in pids.split_whitespace().map(|s| s.parse::<u32>().unwrap()) {
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !h.is_null() {
            assert_eq!(unsafe { WaitForSingleObject(h, 5000) }, WAIT_OBJECT_0);
            unsafe { CloseHandle(h) };
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn rejects_obvious_secret_target_in_command_arguments() {
    let root = std::env::temp_dir().join(format!("magi-tools-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["exec".into()],
        timeout: Duration::from_secs(5),
        max_output_bytes: 4096,
        side_effect_authorized: true,
    };
    assert!(execute_tool(
        ctx,
        "exec",
        json!({"program":"git","args":["show",".env"],"cwd":"src"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn cancellation_terminates_process_and_descendant() {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let root = std::env::temp_dir().join(format!("magi-tools-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(root.join("src/enable"), "").unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["exec".into()],
        timeout: Duration::from_secs(4),
        max_output_bytes: 4096,
        side_effect_authorized: true,
    };
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        execute_tool(
            ctx,
            "exec",
            json!({"program":exe,"args":["--exact","job_child_helper","--nocapture"],"cwd":"src"}),
            worker_cancel,
        )
        .await
    });
    let marker = root.join("src/pids");
    let mut pids = None;
    for _ in 0..300 {
        if let Ok(s) = fs::read_to_string(&marker) {
            if s.split_whitespace().count() == 2 {
                pids = Some(s);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cancel.cancel();
    assert!(worker.await.unwrap().is_err());
    for pid in pids
        .expect("child fixture did not start")
        .split_whitespace()
        .map(|s| s.parse::<u32>().unwrap())
    {
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !h.is_null() {
            assert_eq!(unsafe { WaitForSingleObject(h, 5000) }, WAIT_OBJECT_0);
            unsafe { CloseHandle(h) };
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn job_environment_helper() {
    if std::path::Path::new("enable-env-test").exists() {
        assert!(std::env::var_os("MAGI_TEST_FAKE_SECRET").is_none());
    }
}
#[cfg(windows)]
#[tokio::test]
async fn command_does_not_inherit_fake_secret_environment() {
    let root = std::env::temp_dir().join(format!("magi-tools-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(root.join("src/enable-env-test"), "").unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["exec".into()],
        timeout: Duration::from_secs(4),
        max_output_bytes: 4096,
        side_effect_authorized: true,
    };
    std::env::set_var("MAGI_TEST_FAKE_SECRET", "fixture-only");
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let result=execute_tool(ctx,"exec",json!({"program":exe,"args":["--exact","job_environment_helper","--nocapture"],"cwd":"src"}),CancellationToken::new()).await;
    std::env::remove_var("MAGI_TEST_FAKE_SECRET");
    assert_eq!(result.unwrap().exit_code, Some(0));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_bad_arguments_and_reports_output_limit() {
    let root = std::env::temp_dir().join(format!("magi-exec-limits-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(root.join("src/enable"), "").unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["exec".into()],
        timeout: Duration::from_secs(10),
        max_output_bytes: 8,
        side_effect_authorized: true,
    };
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    for args in [
        json!({"program":exe,"args":[],"cwd":"src","extra":true}),
        json!({"program":exe,"args":[],"cwd":"../"}),
        json!({"program":exe,"args":[".env"],"cwd":"src"}),
    ] {
        assert!(
            execute_tool(ctx.clone(), "exec", args, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!root.join("src/ran").exists());
    }
    let result = execute_tool(
        ctx,
        "exec",
        json!({"program":exe,"args":["--exact","exec_fixture","--nocapture"],"cwd":"src"}),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        "command output exceeds limit; result uncertain"
    );
    assert!(root.join("src/ran").exists());
    fs::remove_dir_all(root).unwrap();
}
