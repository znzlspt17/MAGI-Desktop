use engine::tools::{execute_tool, ToolContext};
use serde_json::json;
use std::{fs, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn reads_and_searches_owned_text_with_line_numbers() {
    let root = std::env::temp_dir().join(format!("magi-read-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/example.txt"),
        "hello\ntarget\ntarget again\n",
    )
    .unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["read".into(), "search".into()],
        timeout: Duration::from_secs(3),
        max_output_bytes: 4096,
        side_effect_authorized: false,
    };
    let read = execute_tool(
        ctx.clone(),
        "read",
        json!({"path":"src/example.txt"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(read.content["text"], "hello\ntarget\ntarget again\n");
    let search = execute_tool(
        ctx,
        "search",
        json!({"path":"src/example.txt", "query":"target"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(search.content["matches"][0]["line"], 2);
    assert_eq!(search.content["matches"][1]["line"], 3);
    fs::remove_file(root.join("src/example.txt")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn rejects_unsafe_or_unlisted_requests_and_invalid_arguments() {
    let root = std::env::temp_dir().join(format!("magi-read-policy-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.txt"), "safe text").unwrap();
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec![
            "read".into(),
            "search".into(),
            "patch".into(),
            "exec".into(),
            "git".into(),
            "mcp".into(),
        ],
        timeout: Duration::from_secs(3),
        max_output_bytes: 128,
        side_effect_authorized: true,
    };
    for path in [
        "../a",
        "src/.env",
        "src/.git/config",
        "src/name:stream",
        "src/CON.txt",
        "src/../a",
        "other/a.txt",
    ] {
        assert!(
            execute_tool(
                ctx.clone(),
                "read",
                json!({"path":path}),
                CancellationToken::new()
            )
            .await
            .is_err(),
            "{path}"
        );
    }
    for args in [
        json!({"path":"src/a.txt","extra":true}),
        json!({}),
        json!({"path":12}),
    ] {
        assert!(
            execute_tool(ctx.clone(), "read", args, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(execute_tool(
        ctx.clone(),
        "search",
        json!({"path":"src/a.txt","query":""}),
        CancellationToken::new()
    )
    .await
    .is_err());
    for name in ["patch", "exec", "git", "mcp"] {
        assert!(
            execute_tool(ctx.clone(), name, json!({}), CancellationToken::new())
                .await
                .is_err()
        );
    }
    let mut no_read = ctx.clone();
    no_read.allowlist.clear();
    assert!(execute_tool(
        no_read,
        "read",
        json!({"path":"src/a.txt"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    let mut not_owned = ctx.clone();
    not_owned.owned_paths = vec!["elsewhere".into()];
    assert!(execute_tool(
        not_owned,
        "read",
        json!({"path":"src/a.txt"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        execute_tool(ctx, "read", json!({"path":"src/a.txt"}), cancelled)
            .await
            .is_err()
    );
    fs::remove_file(root.join("src/a.txt")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn enforces_utf8_file_and_search_output_limits() {
    let root = std::env::temp_dir().join(format!("magi-read-limit-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.txt"), "a\na\na\na\n").unwrap();
    fs::write(root.join("src/b.txt"), [0xff]).unwrap();
    let mut ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["read".into(), "search".into()],
        timeout: Duration::from_secs(3),
        max_output_bytes: 4,
        side_effect_authorized: false,
    };
    assert!(execute_tool(
        ctx.clone(),
        "read",
        json!({"path":"src/a.txt"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    ctx.max_output_bytes = 10;
    assert!(execute_tool(
        ctx.clone(),
        "read",
        json!({"path":"src/b.txt"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    assert!(execute_tool(
        ctx,
        "search",
        json!({"path":"src/a.txt","query":"a"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    fs::remove_file(root.join("src/a.txt")).unwrap();
    fs::remove_file(root.join("src/b.txt")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    fs::remove_dir(root).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_junction_and_hardlink_targets() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let root = std::env::temp_dir().join(format!("magi-read-links-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("target")).unwrap();
    fs::write(root.join("target/a.txt"), "fixture").unwrap();
    fs::hard_link(root.join("target/a.txt"), root.join("src/hard.txt")).unwrap();
    let status = Command::new("cmd.exe")
        .creation_flags(0x08000000)
        .args(["/c", "mklink", "/J"])
        .arg(root.join("src").join("junction"))
        .arg(root.join("target"))
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "junction creation failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let ctx = ToolContext {
        worktree: root.clone(),
        allowed_paths: vec!["src".into()],
        owned_paths: vec!["src".into()],
        allowlist: vec!["read".into()],
        timeout: Duration::from_secs(3),
        max_output_bytes: 128,
        side_effect_authorized: false,
    };
    for path in ["src/hard.txt", "src/junction/a.txt"] {
        assert!(
            execute_tool(
                ctx.clone(),
                "read",
                json!({"path":path}),
                CancellationToken::new()
            )
            .await
            .is_err(),
            "{path}"
        );
    }
    let mut linked_root = ctx;
    linked_root.worktree = root.join("src/junction");
    linked_root.allowed_paths = vec!["a.txt".into()];
    linked_root.owned_paths = vec!["a.txt".into()];
    assert!(execute_tool(
        linked_root,
        "read",
        json!({"path":"a.txt"}),
        CancellationToken::new()
    )
    .await
    .is_err());
    fs::remove_dir(root.join("src/junction")).unwrap();
    fs::remove_file(root.join("src/hard.txt")).unwrap();
    fs::remove_file(root.join("target/a.txt")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    fs::remove_dir(root.join("target")).unwrap();
    fs::remove_dir(root).unwrap();
}
