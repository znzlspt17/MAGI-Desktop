// 2026-11-15: 승인 검사는 문자열 경로만 판정하며 실제 파일 시스템을 확인하지 않는다.
pub(crate) fn protected(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    if p.split('/')
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w == [".config", "gcloud"])
    {
        return true;
    }
    p.split('/').any(|s| {
        s == ".env"
            || s.starts_with(".env.")
            || matches!(
                s,
                ".ssh"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".git-credentials"
                    | ".npmrc"
                    | ".pypirc"
                    | ".netrc"
                    | "credentials.json"
                    | "secrets.json"
                    | "token.json"
            )
            || [".key", ".p12", ".pfx", ".jks", ".keystore"]
                .iter()
                .any(|x| s.ends_with(x))
    })
}

pub(crate) fn safe_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with(['/', '\\']) {
        return false;
    }
    path.split(['/', '\\']).all(|part| {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return false;
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) {
            return false;
        }
        for prefix in ["COM", "LPT"] {
            if let Some(n) = stem.strip_prefix(prefix) {
                if matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                ) {
                    return false;
                }
            }
        }
        true
    })
}

// 2026-11-15: checkout 전 트리를 점검하고 Git의 외부 설정과 hook 실행을 제한한다.
pub(crate) fn create(
    repo: &std::path::Path,
    revision: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("revision must be a full Git object ID".into());
    }
    let meta = std::fs::symlink_metadata(repo.join(".git"))
        .map_err(|_| "repository metadata unavailable")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("linked or indirect Git metadata requires manual review".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("Git metadata reparse point requires manual review".into());
        }
    }
    // 2026-11-15: 검사한 저장소와 Git 메타데이터가 checkout 동안 교체되지 않게 고정한다.
    #[cfg(windows)]
    let _repository_pins =
        crate::tools::pin_directories(repo, std::path::Path::new(".git"), false)?;
    if repo.join(".git/info/attributes").exists() {
        return Err("repository info attributes require manual review".into());
    }
    let parent = target.parent().ok_or("invalid worktree destination")?;
    #[cfg(windows)]
    let _pins = {
        let root = parent.parent().ok_or("invalid worktree parent")?;
        let name = parent.file_name().ok_or("invalid worktree parent")?;
        crate::tools::pin_directories(root, std::path::Path::new(name), true)?
    };
    #[cfg(not(windows))]
    std::fs::create_dir_all(parent).map_err(|_| "cannot create worktree parent")?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| "invalid worktree parent")?;
    if meta.file_type().is_symlink() {
        return Err("worktree parent is a link".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("worktree parent is a reparse point".into());
        }
    }
    let tree = parent.join(format!("{}.tree", uuid::Uuid::new_v4()));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tree)
        .map_err(|_| "cannot prepare tree inspection")?;
    let execute = |args: &[&str], stdout: Option<std::fs::File>| -> Result<(), String> {
        let mut command = Command::new("git");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(stdout.map(Stdio::from).unwrap_or(Stdio::null()))
            .spawn()
            .map_err(|_| "cannot start Git")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().map_err(|_| "cannot check Git process")? {
                return if status.success() {
                    Ok(())
                } else {
                    Err("Git operation failed".into())
                };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Git operation timed out; side effects require inspection".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    let inspected = execute(
        &["ls-tree", "-r", "-z", "--full-tree", revision],
        Some(output),
    );
    if let Err(error) = inspected {
        let _ = std::fs::remove_file(&tree);
        return Err(error);
    }
    let size = std::fs::metadata(&tree)
        .map_err(|_| "cannot inspect tree")?
        .len();
    if size > 8 * 1024 * 1024 {
        let _ = std::fs::remove_file(&tree);
        return Err("Git tree exceeds inspection limit".into());
    }
    let bytes = std::fs::read(&tree).map_err(|_| "cannot inspect tree")?;
    let _ = std::fs::remove_file(&tree);
    for item in bytes.split(|b| *b == 0).filter(|x| !x.is_empty()) {
        let split = item
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("invalid Git tree entry")?;
        let mode = std::str::from_utf8(&item[..split]).map_err(|_| "invalid Git tree metadata")?;
        let name = std::str::from_utf8(&item[split + 1..])
            .map_err(|_| "non UTF-8 path requires manual review")?;
        if !mode.starts_with("100644 ") && !mode.starts_with("100755 ") {
            return Err("link or unsupported tree entry requires manual review".into());
        }
        if !safe_relative(name)
            || protected(name)
            || name
                .split('/')
                .any(|v| v.eq_ignore_ascii_case(".gitattributes"))
        {
            return Err("unsafe or protected Git tree path".into());
        }
    }
    let target = target.to_str().ok_or("invalid worktree path")?;
    #[cfg(windows)]
    let target = target.strip_prefix(r"\\?\").unwrap_or(target);
    execute(
        &["worktree", "add", "--detach", "--", target, revision],
        None,
    )
}
