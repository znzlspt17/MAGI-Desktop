use serde_json::Value;

pub(crate) fn context<'a>(run: &'a Value, item: &Value) -> Result<(u64, &'a Value), String> {
    let id = item["verification_id"]
        .as_str()
        .ok_or("invalid verification id")?;
    let (actual, context) = if let Some(raw) = item.get("agent_attempt") {
        let actual = raw.as_u64().ok_or("invalid evidence attempt")?;
        (
            actual,
            &run["evidence_contexts_by_attempt"][actual.to_string()][id],
        )
    } else {
        let context = &run["evidence_contexts"][id];
        (
            context["agent_attempt"]
                .as_u64()
                .ok_or("legacy evidence attempt missing")?,
            context,
        )
    };
    if context["agent_attempt"].as_u64() != Some(actual) {
        return Err("evidence attempt conflict".into());
    }
    Ok((actual, context))
}

// 2026-11-16: 이전 저장 형식의 시도 번호는 명시된 context에서만 해석하고 추정하지 않는다.
pub(crate) fn resolve<'a>(
    run: &'a Value,
    id: &str,
    attempt: u64,
) -> Result<Option<(&'a Value, &'a Value)>, String> {
    let items = run["evidence"].as_array().ok_or("invalid evidence")?;
    let mut selected = None;
    for item in items.iter().filter(|item| item["verification_id"] == id) {
        let (actual, context) = context(run, item)?;
        if actual == attempt {
            if selected.is_some() {
                return Err("duplicate verification id".into());
            }
            selected = Some((item, context));
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_attempt_is_explicit_and_current_attempt_is_never_inferred() {
        let mut run = serde_json::json!({"evidence":[{"verification_id":"v","passed":true}],"evidence_contexts":{"v":{"agent_attempt":1}}});
        assert!(resolve(&run, "v", 2).unwrap().is_none());
        assert!(resolve(&run, "v", 1).unwrap().is_some());
        run["evidence_contexts"]["v"] = serde_json::json!({});
        assert!(resolve(&run, "v", 2).is_err());
        run["evidence"][0]["agent_attempt"] = 2.into();
        run["evidence_contexts_by_attempt"]["2"]["v"]["agent_attempt"] = 1.into();
        assert!(resolve(&run, "v", 2).is_err());
    }
}
