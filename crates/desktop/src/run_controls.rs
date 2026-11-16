use domain::Project;

pub(crate) fn start_hash(project: &Project, form_clean: bool, pending: bool) -> Option<String> {
    if !form_clean || pending {
        return None;
    }
    project
        .approved
        .as_ref()
        .filter(|a| a.hash == project.draft_hash)
        .map(|a| a.hash.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn start_requires_matching_approved_saved_draft_and_no_pending_edit() {
        let mut project: Project=serde_json::from_value(serde_json::json!({"id":"p","name":"p","path":"x","draft":{"title":"","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"h","approved":{"version":1,"hash":"h","base_revision":"rev","scope":[],"requirements":[],"acceptance":[]}})).unwrap();
        assert_eq!(start_hash(&project, true, false), Some("h".into()));
        assert_eq!(start_hash(&project, false, false), None);
        assert_eq!(start_hash(&project, true, true), None);
        project.draft_hash = "new".into();
        assert_eq!(start_hash(&project, true, false), None);
        project.approved = None;
        assert_eq!(start_hash(&project, true, false), None);
    }
}
