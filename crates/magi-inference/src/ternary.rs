/// CPU reference for already-unpacked ternary values; this does not define model storage.
pub fn ternary_dot(weights: &[i8], input: &[f32]) -> Result<f32, String> {
    if weights.len() != input.len() {
        return Err("length mismatch".into());
    }
    let mut result = 0.0;
    for (&w, &x) in weights.iter().zip(input) {
        if !(-1..=1).contains(&w) {
            return Err("weight must be -1, 0 or 1".into());
        }
        result += f32::from(w) * x;
    }
    Ok(result)
}
