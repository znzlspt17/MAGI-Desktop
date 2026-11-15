use inference::ternary_dot;

#[test]
fn unpacked_ternary_reference_checks_values_and_lengths() {
    assert_eq!(ternary_dot(&[-1, 0, 1], &[2.0, 8.0, 3.0]).unwrap(), 1.0);
    assert!(ternary_dot(&[2], &[1.0]).is_err());
    assert!(ternary_dot(&[1], &[]).is_err());
}

#[test]
fn empty_and_invalid_negative_weights() {
    assert_eq!(ternary_dot(&[], &[]).unwrap(), 0.0);
    assert!(ternary_dot(&[-2], &[1.0]).is_err());
}
