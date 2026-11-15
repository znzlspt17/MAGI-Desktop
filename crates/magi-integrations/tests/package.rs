use magi_integrations::{export_package, PackageInput};
use std::path::PathBuf;

fn input() -> PackageInput {
    PackageInput {
        completed_id: "approved".into(),
        readme: "readme".into(),
        spec: "spec".into(),
        tasks: "tasks".into(),
        constraints: "constraints".into(),
        verification: "verification".into(),
        completeness: "complete".into(),
        test_status: "passed".into(),
    }
}

fn create_test_root() -> PathBuf {
    let base = std::env::temp_dir();
    for attempt in 0..100 {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!(
            "magi-package-test-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        if std::fs::create_dir(&root).is_ok() {
            return root;
        }
    }
    panic!("cannot create isolated test directory")
}

#[test]
fn export_preserves_manifest_and_rejects_overwriting_existing_package() {
    let root = create_test_root();
    let dest = root.join("bundle");
    let manifest = export_package(&input(), &dest).unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dest.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(stored, manifest);
    assert_eq!(manifest["format_version"], 1);
    assert_eq!(manifest["completed_id"], "approved");
    assert_eq!(manifest["completeness"], "complete");
    assert_eq!(manifest["test_status"], "passed");
    let names: Vec<_> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "README.ko.md",
            "SPEC.ko.md",
            "TASKS.md",
            "CONSTRAINTS.md",
            "VERIFICATION.md"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("SPEC.ko.md")).unwrap(),
        "spec"
    );
    assert!(export_package(&input(), &dest).is_err());
    assert_eq!(
        std::fs::read_to_string(dest.join("SPEC.ko.md")).unwrap(),
        "spec"
    );
    for name in names {
        std::fs::remove_file(dest.join(name)).unwrap();
    }
    std::fs::remove_file(dest.join("manifest.json")).unwrap();
    std::fs::remove_dir(&dest).unwrap();
    std::fs::remove_dir(&root).unwrap();
}

#[test]
fn export_requires_completed_identifier_before_creating_output() {
    let root = create_test_root();
    let dest = root.join("bundle");
    let mut unapproved = input();
    unapproved.completed_id = " ".into();
    assert!(export_package(&unapproved, &dest).is_err());
    assert!(!dest.exists());
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn export_refuses_existing_empty_directory_and_cleans_temporary_output() {
    let root = create_test_root();
    let dest = root.join("empty");
    std::fs::create_dir(&dest).unwrap();
    assert!(export_package(&input(), &dest).is_err());
    assert!(dest.is_dir());
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    std::fs::remove_dir(dest).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn export_refuses_existing_file_and_preserves_contents() {
    let root = create_test_root();
    let dest = root.join("existing");
    std::fs::write(&dest, "sentinel").unwrap();
    assert!(export_package(&input(), &dest).is_err());
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "sentinel");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    std::fs::remove_file(dest).unwrap();
    std::fs::remove_dir(root).unwrap();
}
