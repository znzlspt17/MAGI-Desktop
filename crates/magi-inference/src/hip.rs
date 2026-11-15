use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct HipDevice {
    pub ordinal: i32,
    pub name: String,
}
#[derive(Debug, Serialize)]
pub struct HipDiagnostic {
    pub runtime_version_raw: i32,
    pub devices: Vec<HipDevice>,
}

#[cfg(windows)]
pub fn diagnose(path: &Path) -> Result<HipDiagnostic, String> {
    use std::{
        ffi::{c_char, c_void, CStr},
        os::windows::ffi::OsStrExt,
        ptr,
    };
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    struct Library(*mut c_void);
    impl Drop for Library {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    if !path.is_absolute() {
        return Err("HIP runtime path must be absolute".into());
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err("invalid HIP runtime path".into());
    }
    wide.push(0);
    // 선택한 경로로 로드하며 의존성에는 DLL 디렉터리와 Windows 기본 검색 디렉터리를 사용한다.
    let handle = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 0x00000100 | 0x00001000) };
    if handle.is_null() {
        return Err(format!(
            "HIP runtime load failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let library = Library(handle);
    unsafe fn symbol(module: *mut c_void, name: &'static [u8]) -> Result<*mut c_void, String> {
        let p = GetProcAddress(module, name.as_ptr());
        if p.is_null() {
            Err(format!(
                "HIP symbol missing: {}",
                String::from_utf8_lossy(&name[..name.len() - 1])
            ))
        } else {
            Ok(p)
        }
    }
    // HIP's public C ABI uses int result and int device handles.
    let version: unsafe extern "C" fn(*mut i32) -> i32 =
        unsafe { std::mem::transmute(symbol(library.0, b"hipRuntimeGetVersion\0")?) };
    let count: unsafe extern "C" fn(*mut i32) -> i32 =
        unsafe { std::mem::transmute(symbol(library.0, b"hipGetDeviceCount\0")?) };
    let get_device: unsafe extern "C" fn(*mut i32, i32) -> i32 =
        unsafe { std::mem::transmute(symbol(library.0, b"hipDeviceGet\0")?) };
    let name: unsafe extern "C" fn(*mut c_char, i32, i32) -> i32 =
        unsafe { std::mem::transmute(symbol(library.0, b"hipDeviceGetName\0")?) };
    fn check(call: &str, code: i32) -> Result<(), String> {
        if code == 0 {
            Ok(())
        } else {
            Err(format!("{call} failed with HIP error {code}"))
        }
    }
    let mut raw = 0;
    check("hipRuntimeGetVersion", unsafe { version(&mut raw) })?;
    let mut n = 0;
    check("hipGetDeviceCount", unsafe { count(&mut n) })?;
    if !(0..=256).contains(&n) {
        return Err("invalid HIP device count".into());
    }
    let mut devices = Vec::new();
    for ordinal in 0..n {
        let mut device = 0;
        check("hipDeviceGet", unsafe { get_device(&mut device, ordinal) })?;
        let mut bytes = [0 as c_char; 256];
        check("hipDeviceGetName", unsafe {
            name(bytes.as_mut_ptr(), bytes.len() as i32, device)
        })?;
        bytes[255] = 0;
        let label = unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        devices.push(HipDevice {
            ordinal,
            name: label,
        });
    }
    Ok(HipDiagnostic {
        runtime_version_raw: raw,
        devices,
    })
}
#[cfg(not(windows))]
pub fn diagnose(_: &Path) -> Result<HipDiagnostic, String> {
    Err("HIP diagnostic currently supports Windows only".into())
}
