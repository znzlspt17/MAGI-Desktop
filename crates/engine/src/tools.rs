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
        false,
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

        "exec" => run_exec(context, args, cancel).await,
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
pub(crate) struct DirectoryPins {
    _handles: Vec<std::os::windows::io::OwnedHandle>,
    root: PathBuf,
}

// 2026-11-15: 속성 전용 핸들은 공유 제한이 적용되지 않으므로 목록 접근을 요청해 교체를 막는다.
// 각 핸들의 실제 경로와 reparse 여부도 검증한다.
#[cfg(windows)]
pub(crate) fn pin_directories(
    root: &Path,
    relative_parent: &Path,
    create_allowed: bool,
) -> Result<DirectoryPins, String> {
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
        // 2026-11-15: 고정된 부모 아래에서만 새 디렉터리를 만들고 다시 핸들로 검증한다.
        if create_allowed
            && matches!(std::fs::symlink_metadata(&parent), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            match std::fs::create_dir(&parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("cannot create directory".into()),
            }
        }
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
        let pins = pin_directories(&root, Path::new("src"), false).unwrap();

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
        let pins = pin_directories(&root, Path::new("src"), false).unwrap();

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
        let pins = pin_directories(&root, Path::new("src"), false).unwrap();

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

#[cfg(all(test, windows))]
mod journal_pin_tests {
    use super::pin_directories;
    use std::path::Path;
    #[test]
    fn pin_can_create_missing_parents_only_when_allowed() {
        let root = std::env::temp_dir().join(format!("journal-pins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(pin_directories(&root, Path::new("one/two"), false).is_err());
        let pins = pin_directories(&root, Path::new("one/two"), true).unwrap();
        assert!(root.join("one/two").is_dir());
        drop(pins);
        std::fs::remove_dir(root.join("one/two")).unwrap();
        std::fs::remove_dir(root.join("one")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArguments {
    program: String,
    args: Vec<String>,
    cwd: String,
}

// 2026-11-16: 명령 실행은 승인 스냅샷이 있을 때만 허용하고 프로세스를 시작 전에 Job에 묶는다.
async fn run_exec(
    context: ToolContext,
    args: Value,
    cancel: CancellationToken,
) -> Result<ToolResult, String> {
    if !context.side_effect_authorized {
        return Err("command approval required".into());
    }
    let args: ExecArguments =
        serde_json::from_value(args).map_err(|_| "invalid command arguments")?;
    if args.program.is_empty()
        || args.program.contains('\0')
        || args.args.iter().any(|a| a.contains('\0'))
        || args.args.len() > 128
        || context.timeout.is_zero()
        || context.timeout > Duration::from_secs(600)
        || context.max_output_bytes > 4 * 1024 * 1024
        || context.max_output_bytes == 0
    {
        return Err("invalid command limits".into());
    }
    // 2026-11-16: 명백하게 보호 저장소를 지정하는 인자는 승인된 일반 명령에서도 실행하지 않는다.
    if std::iter::once(&args.program)
        .chain(args.args.iter())
        .any(|arg| {
            arg.split(['=', ':'])
                .any(|part| crate::worktree::protected(&part.replace('\\', "/")))
        })
    {
        return Err("protected command target".into());
    }
    if !crate::worktree::safe_relative(&args.cwd)
        || crate::worktree::protected(&args.cwd.replace('\\', "/"))
        || args
            .cwd
            .split(['/', '\\'])
            .any(|s| s.eq_ignore_ascii_case(".git"))
    {
        return Err("unsafe working directory".into());
    }
    let normalized = args.cwd.replace('\\', "/").to_lowercase();
    for scopes in [&context.allowed_paths, &context.owned_paths] {
        if !scopes.iter().any(|s| {
            let s = s.replace('\\', "/").trim_end_matches('/').to_lowercase();
            !s.is_empty() && (normalized == s || normalized.starts_with(&(s + "/")))
        }) {
            return Err("working directory outside scope".into());
        }
    }
    let root = checked_root(&context.worktree)?;
    let mut cwd = root.clone();
    for component in Path::new(&args.cwd).components() {
        cwd.push(component);
        let meta = std::fs::symlink_metadata(&cwd).map_err(|_| "working directory unavailable")?;
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
    let cwd = cwd
        .canonicalize()
        .map_err(|_| "working directory unavailable")?;
    if !cwd.starts_with(&root) || !cwd.is_dir() {
        return Err("invalid working directory".into());
    }
    #[cfg(windows)]
    {
        let pins = pin_directories(&context.worktree, Path::new(&args.cwd), false)?;
        if pins.root.join(&args.cwd) != cwd || pins.root != root {
            return Err("working directory changed".into());
        }
        tokio::task::spawn_blocking(move || {
            exec_windows(
                args,
                cwd,
                context.timeout,
                context.max_output_bytes,
                cancel,
                pins,
            )
        })
        .await
        .map_err(|_| "command worker failed".to_owned())?
    }
    #[cfg(not(windows))]
    {
        let _ = (args, cwd, context, cancel);
        Err("safe command execution unsupported on this platform".into())
    }
}

#[cfg(windows)]
fn exec_windows(
    args: ExecArguments,
    cwd: PathBuf,
    timeout: Duration,
    limit: usize,
    cancel: CancellationToken,
    _pins: DirectoryPins,
) -> Result<ToolResult, String> {
    use std::{
        ffi::c_void,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr,
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0},
        Security::SECURITY_ATTRIBUTES,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Pipes::CreatePipe,
            Threading::*,
        },
    };
    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        let mut slashes = 0;
        for ch in s.chars() {
            if ch == '\\' {
                slashes += 1;
            } else if ch == '"' {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            } else {
                out.push_str(&"\\".repeat(slashes));
                slashes = 0;
                out.push(ch);
            }
        }
        out.push_str(&"\\".repeat(slashes * 2));
        out.push('"');
        out
    }
    unsafe fn own(h: HANDLE) -> Result<OwnedHandle, String> {
        if h.is_null() {
            Err("handle creation failed".into())
        } else {
            Ok(OwnedHandle::from_raw_handle(h as _))
        }
    }
    fn raw(h: &OwnedHandle) -> HANDLE {
        h.as_raw_handle() as HANDLE
    }
    unsafe fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
        let mut r = ptr::null_mut();
        let mut w = ptr::null_mut();
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        if CreatePipe(&mut r, &mut w, &sa, 0) == 0 {
            return Err("pipe creation failed".into());
        }
        Ok((own(r)?, own(w)?))
    }
    let (stdin_r, stdin_w) = unsafe { pipe()? };
    let (stdout_r, stdout_w) = unsafe { pipe()? };
    let (stderr_r, stderr_w) = unsafe { pipe()? };
    for h in [&stdin_w, &stdout_r, &stderr_r] {
        if unsafe { SetHandleInformation(raw(h), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err("pipe setup failed".into());
        }
    }
    let job = unsafe { own(CreateJobObjectW(ptr::null(), ptr::null()))? };
    let mut job_info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    job_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            &job_info as *const _ as *const c_void,
            std::mem::size_of_val(&job_info) as u32,
        )
    } == 0
    {
        return Err("job configuration failed".into());
    }
    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
    let mut storage =
        vec![0usize; (size + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>()];
    let attrs = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) } == 0 {
        return Err("process attributes failed".into());
    }
    struct Attr(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for Attr {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) }
        }
    }
    let _attr = Attr(attrs);
    let handles = [raw(&stdin_r), raw(&stdout_w), raw(&stderr_w)];
    if unsafe {
        UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            std::mem::size_of_val(&handles),
            ptr::null_mut(),
            ptr::null(),
        )
    } == 0
    {
        return Err("process handle list failed".into());
    }
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = raw(&stdin_r);
    startup.StartupInfo.hStdOutput = raw(&stdout_w);
    startup.StartupInfo.hStdError = raw(&stderr_w);
    startup.lpAttributeList = attrs;
    let mut command = std::iter::once(&args.program)
        .chain(args.args.iter())
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // 2026-11-16: 승인한 명령에 부모 프로세스의 임의 환경 변수와 인증 정보가 전달되지 않게 한다.
    let mut environment = Vec::<u16>::new();
    for name in ["PATH", "SystemRoot", "TEMP", "TMP", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            environment.extend(std::ffi::OsStr::new(name).encode_wide());
            environment.push('=' as u16);
            environment.extend(value.encode_wide());
            environment.push(0);
        }
    }
    environment.push(0);
    let cwd: Vec<u16> = cwd.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            ptr::null(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr() as *const c_void,
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut pi,
        )
    } == 0
    {
        return Err("command start failed".into());
    }
    let process = unsafe { own(pi.hProcess)? };
    let thread = unsafe { own(pi.hThread)? };
    if unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) } == 0 {
        unsafe { TerminateProcess(raw(&process), 1) };
        return Err("cannot contain command".into());
    }
    if cancel.is_cancelled() {
        unsafe { TerminateProcess(raw(&process), 1) };
        return Err("cancelled before command start".into());
    }
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        unsafe { TerminateProcess(raw(&process), 1) };
        return Err("cannot resume command".into());
    }
    drop(thread);
    drop(stdin_r);
    drop(stdin_w);
    drop(stdout_w);
    drop(stderr_w);
    let read = move |h: OwnedHandle| {
        std::thread::spawn(move || {
            let mut f = std::fs::File::from(h);
            let mut out = Vec::new();
            f.by_ref()
                .take(limit as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|_| "command output failed")?;
            Ok::<_, &str>(out)
        })
    };
    let stdout = read(stdout_r);
    let stderr = read(stderr_r);
    let deadline = Instant::now() + timeout;
    let error = loop {
        if cancel.is_cancelled() {
            break Some("command cancelled; result uncertain");
        }
        if Instant::now() >= deadline {
            break Some("command timed out; result uncertain");
        }
        let state = unsafe { WaitForSingleObject(raw(&process), 20) };
        if state == WAIT_OBJECT_0 {
            break None;
        }
        if state != 258 {
            break Some("command wait failed; result uncertain");
        }
    };
    if error.is_some() {
        drop(job);
        let _ = unsafe { WaitForSingleObject(raw(&process), 5000) };
    } else {
        drop(job);
    }
    let mut code = 0u32;
    let code_ok = unsafe { GetExitCodeProcess(raw(&process), &mut code) } != 0;
    let out = stdout.join().map_err(|_| "output worker failed")??;
    let err = stderr.join().map_err(|_| "output worker failed")??;
    if let Some(e) = error {
        return Err(e.into());
    }
    if !code_ok {
        return Err("command exit unavailable".into());
    }
    if out.len() > limit || err.len() > limit || out.len() + err.len() > limit {
        return Err("command output exceeds limit; result uncertain".into());
    }
    let stdout = String::from_utf8(out).map_err(|_| "command output not UTF-8")?;
    let stderr = String::from_utf8(err).map_err(|_| "command output not UTF-8")?;
    Ok(ToolResult {
        content: json!({"stdout":stdout,"stderr":stderr}),
        exit_code: Some(code as i32),
    })
}
