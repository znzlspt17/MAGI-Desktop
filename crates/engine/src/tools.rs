use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ToolContext {
    pub worktree: PathBuf,
    pub allowed_paths: Vec<String>,
    pub owned_paths: Vec<String>,
    pub allowlist: Vec<String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub side_effect_authorized: bool,
}
#[derive(Debug)]
pub struct ToolResult {
    pub content: Value,
    pub exit_code: Option<i32>,
}

// 2026-11-15: 작업 루트 또는 상위 경로가 junction으로 바뀌면 승인한 경로 밖을 열지 않는다.
fn checked_root(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err("worktree path must be absolute".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err("invalid worktree path".into());
        }
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let meta = std::fs::symlink_metadata(&current).map_err(|_| "worktree unavailable")?;
        if meta.file_type().is_symlink() {
            return Err("worktree links are not allowed".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("worktree reparse points are not allowed".into());
            }
        }
    }
    let canonical = path.canonicalize().map_err(|_| "worktree unavailable")?;
    #[cfg(windows)]
    {
        let normalize = |p: &Path| {
            let text = p.to_string_lossy().replace('/', "\\");
            let text = if let Some(s) = text.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{s}")
            } else {
                text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
            };
            text.to_lowercase()
        };
        if normalize(path) != normalize(&canonical) {
            return Err("worktree path changed".into());
        }
    }
    #[cfg(not(windows))]
    if canonical != path {
        return Err("worktree path changed".into());
    }
    Ok(canonical)
}
// 2026-11-15: 보호 경로와 링크를 거부하고 실제 열린 파일의 위치 및 hardlink 수를 확인한다.
fn read_checked(ctx: &ToolContext, name: &str) -> Result<String, String> {
    if !crate::worktree::safe_relative(name)
        || crate::worktree::protected(&name.replace('\\', "/"))
        || name
            .split(['/', '\\'])
            .any(|s| s.eq_ignore_ascii_case(".git"))
    {
        return Err("unsafe path".into());
    }
    let normalized = name.replace('\\', "/").to_lowercase();
    let in_scope = |paths: &Vec<String>| {
        paths.iter().any(|p| {
            let p = p.replace('\\', "/").trim_end_matches('/').to_lowercase();
            !p.is_empty() && (normalized == p || normalized.starts_with(&(p + "/")))
        })
    };
    if !in_scope(&ctx.allowed_paths) || !in_scope(&ctx.owned_paths) {
        return Err("path outside permitted scope".into());
    }
    #[cfg(windows)]
    let pins = pin_directories(
        &ctx.worktree,
        Path::new(name).parent().ok_or("invalid parent")?,
    )?;
    #[cfg(windows)]
    let root = pins.root.clone();
    #[cfg(not(windows))]
    let root = checked_root(&ctx.worktree)?;
    let mut current = root.clone();
    for part in Path::new(name).components() {
        current.push(part);
        let meta = std::fs::symlink_metadata(&current).map_err(|_| "path unavailable")?;
        if meta.file_type().is_symlink() {
            return Err("links are not allowed".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("reparse points are not allowed".into());
            }
        }
    }
    let canonical = current.canonicalize().map_err(|_| "path unavailable")?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return Err("path outside worktree".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(&canonical).map_err(|_| "file open failed")?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        };
        let h = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(h, &mut info) } == 0
            || info.nNumberOfLinks != 1
            || info.dwFileAttributes & 0x400 != 0
        {
            return Err("unsafe file handle".into());
        }
        let mut b = vec![0u16; 32768];
        let n = unsafe { GetFinalPathNameByHandleW(h, b.as_mut_ptr(), b.len() as u32, 0) } as usize;
        if n == 0 || n >= b.len() {
            return Err("file path changed".into());
        }
        let actual = String::from_utf16(&b[..n]).map_err(|_| "invalid path")?;
        if Path::new(&actual) != canonical.as_path() {
            return Err("file path changed".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata().map_err(|_| "file unavailable")?.nlink() != 1 {
            return Err("hardlinks are not allowed".into());
        }
    }
    if ctx.max_output_bytes == 0 {
        return Err("output limit required".into());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(ctx.max_output_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file read failed")?;
    if bytes.len() > ctx.max_output_bytes {
        return Err("file exceeds output limit".into());
    }
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8".into())
}

pub async fn execute_tool(
    context: ToolContext,
    name: &str,
    args: Value,
    cancel: CancellationToken,
) -> Result<ToolResult, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    if !context.allowlist.iter().any(|tool| tool == name) {
        return Err("tool not allowed".into());
    }
    match name {
        "read" | "search" => {
            let name = name.to_owned();
            let worker = tokio::task::spawn_blocking(move || read_or_search(context, &name, args));
            tokio::select! { _=cancel.cancelled()=>Err("cancelled".into()), result=worker=>result.map_err(|_|"read worker failed".to_owned())? }
        }

        _ => Err("unsupported tool".into()),
    }
}

// 2026-11-15: 파일 접근과 텍스트 검색은 비동기 요청 처리 스레드를 막지 않도록 별도 작업에서 수행한다.
fn read_or_search(context: ToolContext, name: &str, args: Value) -> Result<ToolResult, String> {
    match name {
        "read" => {
            let args: ReadArguments =
                serde_json::from_value(args).map_err(|_| "invalid read arguments")?;
            let text = read_checked(&context, &args.path)?;
            Ok(ToolResult {
                content: json!({"path":args.path,"text":text}),
                exit_code: None,
            })
        }
        "search" => {
            let args: SearchArguments =
                serde_json::from_value(args).map_err(|_| "invalid search arguments")?;
            if args.query.is_empty() {
                return Err("query required".into());
            }
            let text = read_checked(&context, &args.path)?;
            let matches: Vec<Value> = text
                .lines()
                .enumerate()
                .filter(|(_, line)| line.contains(&args.query))
                .map(|(i, line)| json!({"path":args.path,"line":i+1,"text":line}))
                .collect();
            let content = json!({"matches":matches});
            if content.to_string().len() > context.max_output_bytes {
                return Err("search output exceeds limit".into());
            }
            Ok(ToolResult {
                content,
                exit_code: None,
            })
        }
        _ => Err("unsupported tool".into()),
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    path: String,
    query: String,
}
#[cfg(windows)]
struct DirectoryPins {
    _handles: Vec<std::os::windows::io::OwnedHandle>,
    root: PathBuf,
}

// 2026-11-15: 속성 전용 핸들은 공유 제한이 적용되지 않으므로 목록 접근을 요청해 교체를 막는다.
// 각 핸들의 실제 경로와 reparse 여부도 검증한다.
#[cfg(windows)]
fn pin_directories(root: &Path, relative_parent: &Path) -> Result<DirectoryPins, String> {
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::Component,
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
    };
    fn same(a: &Path, b: &Path) -> bool {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    fn open_checked(path: &Path) -> Result<OwnedHandle, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err("cannot pin directory".into());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as _, &mut info) } == 0
            || info.dwFileAttributes & 0x400 != 0
            || info.dwFileAttributes & 0x10 == 0
        {
            return Err("unsafe directory handle".into());
        }
        let mut buffer = vec![0u16; 32768];
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle() as _,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        } as usize;
        if n == 0 || n >= buffer.len() {
            return Err("cannot verify directory handle".into());
        }
        let actual =
            PathBuf::from(String::from_utf16(&buffer[..n]).map_err(|_| "invalid directory path")?);
        if !same(&actual, path) {
            return Err("directory path changed".into());
        }
        Ok(handle)
    }
    if relative_parent
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("invalid parent path".into());
    }
    let root = checked_root(root)?;
    let mut handles = vec![open_checked(&root)?];
    let mut parent = root.clone();
    for part in relative_parent.components() {
        parent.push(part);
        handles.push(open_checked(&parent)?);
    }
    Ok(DirectoryPins {
        _handles: handles,
        root,
    })
}

#[cfg(all(test, windows))]
mod directory_pin_tests {
    use super::*;
    #[test]
    fn pinned_directory_allows_child_rename() {
        let root =
            std::env::temp_dir().join(format!("magi-read-pin-child-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("src").join("a.txt"), "fixture").unwrap();
        let pins = pin_directories(&root, Path::new("src")).unwrap();
        std::fs::rename(
            root.join("src").join("a.txt"),
            root.join("src").join("b.txt"),
        )
        .unwrap();
        drop(pins);
        std::fs::remove_file(root.join("src").join("b.txt")).unwrap();
        std::fs::remove_dir(root.join("src")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn pinned_empty_directory_cannot_be_deleted() {
        let root =
            std::env::temp_dir().join(format!("magi-read-pin-delete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        let pins = pin_directories(&root, Path::new("src")).unwrap();
        assert!(std::fs::remove_dir(root.join("src")).is_err());
        drop(pins);
        std::fs::remove_dir(root.join("src")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn pinned_directory_cannot_be_replaced_while_handle_is_open() {
        let root = std::env::temp_dir().join(format!("magi-read-pin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("src").join("a.txt"), "fixture").unwrap();
        let pins = pin_directories(&root, Path::new("src")).unwrap();
        let attacker = root.clone();
        assert!(std::thread::spawn(move || {
            std::fs::rename(attacker.join("src"), attacker.join("moved"))
        })
        .join()
        .unwrap()
        .is_err());
        assert!(std::fs::rename(&root, root.with_extension("moved")).is_err());
        drop(pins);
        std::fs::rename(root.join("src"), root.join("moved")).unwrap();
        std::fs::remove_file(root.join("moved").join("a.txt")).unwrap();
        std::fs::remove_dir(root.join("moved")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
