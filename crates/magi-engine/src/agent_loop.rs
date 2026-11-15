use magi_core::ApprovedSpec;
use std::collections::{HashMap, HashSet};

// 2026-11-14: 승인된 작업의 소유 경로와 의존성을 먼저 검증해 병렬 변경 충돌과 교착을 막는다.
pub fn validate_plan(spec: &ApprovedSpec) -> Result<Vec<String>, String> {
    if spec.hash.trim().is_empty()
        || spec.version == 0
        || spec.base_revision.trim().is_empty()
        || spec.tasks.is_empty()
    {
        return Err("approved specification and tasks required".into());
    }
    let mut ids = HashMap::new();

    for (i, task) in spec.tasks.iter().enumerate() {
        if task.id.trim().is_empty()
            || ids.insert(task.id.as_str(), i).is_some()
            || task.validation.is_empty()
            || task.validation.iter().any(|v| v.trim().is_empty())
            || task.owned_paths.is_empty()
        {
            return Err("invalid task".into());
        }
        for raw in &task.owned_paths {
            let path = raw.replace('\\', "/").to_lowercase();
            if path.is_empty()
                || path.starts_with('/')
                || path.contains(':')
                || path
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
            {
                return Err("invalid owned path".into());
            }
            if !spec.scope.iter().any(|s| {
                let s = s.replace('\\', "/").trim_end_matches('/').to_lowercase();
                !s.is_empty() && (path == s || path.starts_with(&(s + "/")))
            }) {
                return Err("owned path outside approved scope".into());
            }
        }
    }
    let mut binding_ids = HashSet::new();
    for binding in &spec.task_bindings {
        let valid_id = |id: &str, prefix: &str, len: usize| {
            id.strip_prefix(prefix)
                .and_then(|v| v.parse::<usize>().ok())
                .is_some_and(|i| i < len && id == format!("{prefix}{i}"))
        };
        if !ids.contains_key(binding.task_id.as_str())
            || !binding_ids.insert(binding.task_id.as_str())
            || binding.goal.trim().is_empty()
            || binding.requirement_ids.is_empty()
            || binding.acceptance_ids.is_empty()
            || binding
                .requirement_ids
                .iter()
                .any(|id| !valid_id(id, "requirement-", spec.requirements.len()))
            || binding
                .acceptance_ids
                .iter()
                .any(|id| !valid_id(id, "acceptance-", spec.acceptance.len()))
        {
            return Err("invalid task binding".into());
        }
    }
    if binding_ids.len() != spec.tasks.len() {
        return Err("missing task binding".into());
    }
    let mut done = HashSet::new();
    let mut ordered = Vec::new();
    while ordered.len() < spec.tasks.len() {
        let mut progress = false;
        for task in &spec.tasks {
            if done.contains(task.id.as_str()) {
                continue;
            }
            if task
                .dependencies
                .iter()
                .any(|d| !ids.contains_key(d.as_str()))
            {
                return Err("missing dependency".into());
            }
            if task.dependencies.iter().all(|d| done.contains(d.as_str())) {
                done.insert(task.id.as_str());
                ordered.push(task.id.clone());
                progress = true;
            }
        }
        if !progress {
            return Err("dependency cycle".into());
        }
    }
    Ok(ordered)
}
