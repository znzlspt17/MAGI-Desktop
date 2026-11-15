use std::{
    fs,
    process::{Command, Output},
};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_inference"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn capability_reports_unsupported_without_probing_gpu() {
    let output = run(&["capability"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["inference_supported"], false);
    assert_eq!(json["gpu_runtime"], "not probed");
    assert!(json["reason"].as_str().unwrap().contains("not implemented"));
}

#[test]
fn inspect_outputs_fake_gguf_metadata_without_claiming_support() {
    let path = std::env::temp_dir().join(format!("magi-cli-{}.gguf", std::process::id()));
    let mut header = b"GGUF".to_vec();
    header.extend(3u32.to_le_bytes());
    header.extend(0u64.to_le_bytes());
    header.extend(0u64.to_le_bytes());
    fs::write(&path, header).unwrap();
    let output = run(&["inspect", path.to_str().unwrap()]);
    fs::remove_file(path).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["version"], 3);
    assert_eq!(json["inference_supported"], false);
}

#[test]
fn invalid_arguments_and_diagnostic_path_fail_on_stderr() {
    for args in [
        &[][..],
        &["capability", "extra"][..],
        &["inspect"][..],
        &["diagnostic"][..],
        &["diagnostic", "relative.dll"][..],
        &["unknown"][..],
    ] {
        let output = run(args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}
