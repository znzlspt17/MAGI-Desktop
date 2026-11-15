#[cfg(windows)]
#[test]
fn rejects_relative_runtime_path() {
    let err = inference::hip::diagnose(std::path::Path::new("amdhip64.dll")).unwrap_err();
    assert!(err.contains("must be absolute"));
}

#[cfg(windows)]
#[test]
fn reports_missing_runtime() {
    let err = inference::hip::diagnose(std::path::Path::new(r"Z:\definitely-missing\amdhip64.dll"))
        .unwrap_err();
    assert!(err.contains("HIP runtime load failed"));
}

#[cfg(not(windows))]
#[test]
fn reports_windows_only() {
    assert!(inference::hip::diagnose(std::path::Path::new("/missing"))
        .unwrap_err()
        .contains("Windows only"));
}
