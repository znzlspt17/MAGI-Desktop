use magi_inference::inspect;
use std::io::Cursor;

fn header(version: u32, tensors: u64, metadata: u64) -> Vec<u8> {
    let mut v = b"GGUF".to_vec();
    v.extend(version.to_le_bytes());
    v.extend(tensors.to_le_bytes());
    v.extend(metadata.to_le_bytes());
    v
}
fn string(v: &mut Vec<u8>, s: &str) {
    v.extend((s.len() as u64).to_le_bytes());
    v.extend(s.as_bytes());
}

#[test]
fn inspects_valid_header_and_architecture() {
    let mut v = header(3, 0, 1);
    string(&mut v, "general.architecture");
    v.extend(8u32.to_le_bytes());
    string(&mut v, "gemma4");
    let info = inspect(&mut Cursor::new(v)).unwrap();
    assert_eq!(info.architecture.as_deref(), Some("gemma4"));
    assert!(!info.inference_supported);
}
#[test]
fn rejects_bad_header_and_unbounded_counts() {
    assert!(inspect(&mut Cursor::new(b"NOPE".to_vec())).is_err());
    assert!(inspect(&mut Cursor::new(header(1, 0, 0))).is_err());
    assert!(inspect(&mut Cursor::new(header(3, u64::MAX, 0))).is_err());
}
#[test]
fn rejects_out_of_bounds_tensor_offsets() {
    let mut v = header(3, 1, 0);
    string(&mut v, "x");
    v.extend(1u32.to_le_bytes());
    v.extend(4u64.to_le_bytes());
    v.extend(0u32.to_le_bytes());
    v.extend(4096u64.to_le_bytes());
    assert!(inspect(&mut Cursor::new(v)).is_err());
}
#[test]
fn validates_known_payload_length_and_alignment() {
    let mut v = header(2, 1, 0);
    string(&mut v, "x");
    v.extend(1u32.to_le_bytes());
    v.extend(4u64.to_le_bytes());
    v.extend(0u32.to_le_bytes());
    v.extend(0u64.to_le_bytes());
    while !v.len().is_multiple_of(32) {
        v.push(0);
    }
    v.extend([0u8; 15]);
    assert!(inspect(&mut Cursor::new(v.clone())).is_err());
    v.push(0);
    assert_eq!(
        inspect(&mut Cursor::new(v)).unwrap().tensors[0].ggml_type,
        0
    );
}
#[test]
fn rejects_unknown_array_element_type_even_when_empty() {
    let mut v = header(3, 0, 1);
    string(&mut v, "array");
    v.extend(9u32.to_le_bytes());
    v.extend(99u32.to_le_bytes());
    v.extend(0u64.to_le_bytes());
    assert!(inspect(&mut Cursor::new(v)).is_err());
}
#[test]
fn rejects_dimension_overflow_and_truncated_metadata() {
    let mut v = header(3, 1, 0);
    string(&mut v, "x");
    v.extend(2u32.to_le_bytes());
    v.extend(u64::MAX.to_le_bytes());
    v.extend(2u64.to_le_bytes());
    v.extend(0u32.to_le_bytes());
    v.extend(0u64.to_le_bytes());
    assert!(inspect(&mut Cursor::new(v)).is_err());
    let mut v = header(3, 0, 1);
    string(&mut v, "x");
    v.extend(8u32.to_le_bytes());
    v.extend(100u64.to_le_bytes());
    assert!(inspect(&mut Cursor::new(v)).is_err());
}
#[test]
fn opaque_tensor_type_does_not_claim_model_support() {
    let mut v = header(3, 1, 0);
    string(&mut v, "x");
    v.extend(1u32.to_le_bytes());
    v.extend(8u64.to_le_bytes());
    v.extend(999u32.to_le_bytes());
    v.extend(0u64.to_le_bytes());
    while !v.len().is_multiple_of(32) {
        v.push(0);
    }
    v.push(0);
    let model = inspect(&mut Cursor::new(v)).unwrap();
    assert_eq!(model.tensors[0].ggml_type, 999);
    assert!(!model.inference_supported);
}
