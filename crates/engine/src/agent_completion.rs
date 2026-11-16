use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Mutex;

const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");
const COMPLETIONS: TableDefinition<&str, u8> = TableDefinition::new("completion_commits_v1");
const CALLS: TableDefinition<&str, &str> = TableDefinition::new("calls");
pub(crate) fn review_call_id(attempt: u64) -> String {
    if attempt == 1 {
        "final-review".into()
    } else {
        format!("final-review:{attempt}")
    }
}
#[cfg(test)]
type SnapshotHook = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
static TEST_AFTER_SNAPSHOT: Mutex<Option<SnapshotHook>> = Mutex::new(None);

// 2026-11-16: 최종 review가 어느 승인과 실행 시도의 코드 및 검증을 보았는지 명시한다.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReviewBinding {
    pub(crate) schema_version: u32,
    pub(crate) spec_hash: String,
    pub(crate) spec_version: u64,
    pub(crate) context_version: u64,
    pub(crate) agent_attempt: u64,
    pub(crate) revision: String,
    pub(crate) worktree_hash: String,
    pub(crate) verification_ids: Vec<String>,
}

// 2026-11-16: 실행자와 저장자가 동일한 전체 입력에서 digest를 계산한다.
pub(crate) fn review_input(
    spec: &domain::ApprovedSpec,
    evidence: &[domain::VerificationEvidence],
    diff: &serde_json::Value,
    binding: &ReviewBinding,
) -> Result<(serde_json::Value, String), String> {
    use sha2::{Digest, Sha256};
    if binding.schema_version != 1 {
        return Err("unsupported review binding version".into());
    }
    let request = serde_json::json!({
        "role": "final_reviewer",
        "instruction": "최종 변경 내용과 검증 근거 및 명세를 확인하고 approved boolean JSON만 반환하세요.",
        "spec": spec,
        "verification_evidence": evidence,
        "changes": diff,
        "binding": binding,
    });
    let input = serde_json::json!([{"role":"user", "content":request.to_string()}]);
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&input).map_err(|_| "invalid review input")?)
    );
    Ok((input, digest))
}

// 2026-11-16: 승인 작업의 정확한 검증과 review 입력을 현재 작업 공간에 결속한다.
fn review_proof(
    db: &Database,
    id: &str,
    run: &serde_json::Value,
    project: &domain::Project,
) -> Result<Option<(ReviewBinding, String)>, String> {
    let Some(spec) = project.approved.as_ref() else {
        return Ok(None);
    };
    if run["spec_hash"] != spec.hash
        || run["spec_version"] != spec.version
        || project.draft_hash != spec.hash
        || run["context_version"] != project.context_version
    {
        return Ok(None);
    }
    let Some(_items) = run["evidence"].as_array() else {
        return Ok(None);
    };
    let mut selected = Vec::new();
    let mut ids = Vec::new();
    for task in &spec.tasks {
        for (index, command) in task.validation.iter().enumerate() {
            // 2026-11-16: 검증 결과가 승인된 작업의 요구사항과 인수 조건에 귀속됐는지 확인한다.
            let mut bindings = spec
                .task_bindings
                .iter()
                .filter(|binding| binding.task_id == task.id);
            let Some(task_binding) = bindings.next() else {
                return Ok(None);
            };
            if bindings.next().is_some()
                || task_binding.requirement_ids.is_empty()
                || task_binding.acceptance_ids.is_empty()
            {
                return Ok(None);
            }
            let verification_id = serde_json::to_string(&(&task.id, "final_validation", index))
                .map_err(|_| "invalid verification id")?;
            let attempt = run["agent_attempt"].as_u64().ok_or("invalid attempt")?;
            let Some((item, context)) = crate::evidence::resolve(run, &verification_id, attempt)?
            else {
                return Ok(None);
            };
            let evidence: domain::VerificationEvidence =
                serde_json::from_value(item.clone()).map_err(|_| "invalid evidence")?;
            if !evidence.passed
                || evidence.spec_hash != spec.hash
                || evidence.command != *command
                || evidence.requirement_ids != task_binding.requirement_ids
                || evidence.acceptance_ids != task_binding.acceptance_ids
                || context["schema_version"] != 1
                || context["spec_version"] != spec.version
                || context["context_version"] != project.context_version
                || context["agent_attempt"] != run["agent_attempt"]
                || context["task_id"] != task.id
            {
                return Ok(None);
            }
            ids.push(verification_id);
            selected.push(evidence);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let diff = crate::runs::diff(db, id)?;
    let revision = diff["current_revision"]
        .as_str()
        .ok_or("invalid diff revision")?
        .to_owned();
    let worktree_hash = diff["worktree_hash"]
        .as_str()
        .ok_or("invalid diff snapshot")?
        .to_owned();
    if selected
        .iter()
        .any(|e| e.revision != revision || e.worktree_hash != worktree_hash)
    {
        return Ok(None);
    }
    let binding = ReviewBinding {
        schema_version: 1,
        spec_hash: spec.hash.clone(),
        spec_version: spec.version,
        context_version: project.context_version,
        agent_attempt: run["agent_attempt"].as_u64().ok_or("invalid attempt")?,
        revision,
        worktree_hash,
        verification_ids: ids,
    };
    let (_, digest) = review_input(spec, &selected, &diff, &binding)?;
    Ok(Some((binding, digest)))
}

// 2026-11-16: 완료 marker와 상태를 함께 저장해 복구 시 같은 결과를 중복 반영하지 않는다.
pub(crate) fn finish_agent(
    db: &Database,
    gate: &Mutex<()>,
    record: &serde_json::Value,
) -> Result<bool, String> {
    let _guard = gate.lock().map_err(|_| "mutation gate unavailable")?;
    let id = record["run_id"].as_str().ok_or("invalid run id")?;
    let project_id = record["project_id"].as_str().ok_or("invalid project id")?;
    let completion_id = record["id"].as_str().ok_or("invalid completion id")?;
    let attempt = record["outcome"]["result"]["attempt"]
        .as_u64()
        .ok_or("invalid attempt")?;
    let success = record["outcome"]["result"]["success"]
        .as_bool()
        .ok_or("invalid result")?;
    // 2026-11-16: 기존 marker가 있으면 현재 프로젝트나 파일 상태와 무관하게 내구성을 재확인한다.
    {
        let tx = db.begin_write().map_err(|e| e.to_string())?;
        let previous = {
            let table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
            let value = table
                .get(completion_id)
                .map_err(|e| e.to_string())?
                .map(|v| v.value());
            value
        };
        if let Some(value) = previous {
            {
                let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
                table
                    .insert(completion_id, value)
                    .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            return Ok(value != 0);
        }
    }
    let verified_snapshot = if success {
        let run = {
            let read = db.begin_read().map_err(|e| e.to_string())?;
            let runs = read.open_table(RUNS).map_err(|e| e.to_string())?;
            let run: serde_json::Value = serde_json::from_str(
                runs.get(id)
                    .map_err(|e| e.to_string())?
                    .ok_or("run not found")?
                    .value(),
            )
            .map_err(|e| e.to_string())?;
            run
        };
        let eligible = run["project_id"] == project_id
            && run["agent_active"] == true
            && run["agent_attempt"] == attempt
            && run["status"] == "awaiting_implementation";
        if eligible {
            let project_json = {
                let read = db.begin_read().map_err(|e| e.to_string())?;
                let projects = read
                    .open_table(crate::PROJECTS)
                    .map_err(|e| e.to_string())?;
                let json = projects
                    .get(project_id)
                    .map_err(|e| e.to_string())?
                    .ok_or("project not found")?
                    .value()
                    .to_owned();
                json
            };
            let project: domain::Project =
                serde_json::from_str(&project_json).map_err(|e| e.to_string())?;
            let current_spec = project.approved.as_ref().is_some_and(|spec| {
                run["spec_hash"] == spec.hash
                    && run["spec_version"] == spec.version
                    && project.draft_hash == spec.hash
                    && run["context_version"] == project.context_version
            });
            let proof = if current_spec {
                review_proof(db, id, &run, &project)?
            } else {
                None
            };
            Some((run, project_json, current_spec, proof))
        } else {
            None
        }
    } else {
        None
    };
    #[cfg(test)]
    {
        let hook = {
            let mut hook = TEST_AFTER_SNAPSHOT.lock().unwrap();
            if hook.as_ref().is_some_and(|item| item.0 == id) {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, reached, proceed)) = hook {
            reached.send(()).unwrap();
            proceed.recv().unwrap();
        }
    }
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    let previous = {
        let table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        let value = table
            .get(completion_id)
            .map_err(|e| e.to_string())?
            .map(|v| v.value());
        value
    };
    if let Some(value) = previous {
        {
            let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
            table
                .insert(completion_id, value)
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(value != 0);
    }
    let applied = {
        let mut table = tx.open_table(RUNS).map_err(|e| e.to_string())?;
        let mut run: serde_json::Value = {
            let value = table
                .get(id)
                .map_err(|e| e.to_string())?
                .ok_or("run not found")?;
            serde_json::from_str(value.value()).map_err(|e| e.to_string())?
        };
        // 2026-11-16: 완료된 동일 시도의 자원 표시만 정리하고 취소 상태와 이벤트는 보존한다.
        let cancelled_cleanup = run["project_id"] == project_id
            && run["agent_active"] == true
            && run["agent_attempt"] == attempt
            && run["status"] == "cancelled";
        if cancelled_cleanup {
            run["agent_active"] = false.into();
            table
                .insert(id, run.to_string().as_str())
                .map_err(|e| e.to_string())?;
        }
        let mut applied = run["project_id"] == project_id
            && run["agent_active"] == true
            && run["agent_attempt"] == attempt
            && run["status"] != "cancelled"
            && (!success
                || (verified_snapshot.is_some() && run["status"] == "awaiting_implementation"));
        if applied && !success {
            let projects = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
            let project = projects.get(project_id).map_err(|e| e.to_string())?;
            let project: Option<domain::Project> = project
                .map(|row| serde_json::from_str(row.value()))
                .transpose()
                .map_err(|_| "invalid project")?;
            applied = project.is_some_and(|p| {
                p.approved.as_ref().is_some_and(|spec| {
                    run["spec_hash"] == spec.hash
                        && run["spec_version"] == spec.version
                        && p.draft_hash == spec.hash
                        && run["context_version"] == p.context_version
                })
            });
        }
        if applied {
            let verified =
                if let Some((snapshot, project_json, current_spec, proof)) = &verified_snapshot {
                    let table = tx.open_table(crate::PROJECTS).map_err(|e| e.to_string())?;
                    let current = table
                        .get(project_id)
                        .map_err(|e| e.to_string())?
                        .map(|v| v.value().to_owned());
                    if !current_spec
                        || &run != snapshot
                        || current.as_deref() != Some(project_json.as_str())
                    {
                        applied = false;
                        false
                    } else if let Some((binding, digest)) = proof {
                        let calls = tx.open_table(CALLS).map_err(|e| e.to_string())?;
                        let key = format!("{id}:{}", review_call_id(binding.agent_attempt));
                        let call = calls.get(key.as_str()).map_err(|e| e.to_string())?;
                        let call: Option<serde_json::Value> = call
                            .map(|v| serde_json::from_str(v.value()))
                            .transpose()
                            .map_err(|_| "invalid review call")?;
                        call.is_some_and(|c| {
                            c["status"] == "completed"
                                && c["hash"] == *digest
                                && c["result"]["approved"] == true
                                && serde_json::from_value::<ReviewBinding>(
                                    c["result"]["binding"].clone(),
                                )
                                .ok()
                                .as_ref()
                                    == Some(binding)
                        })
                    } else {
                        false
                    }
                } else {
                    false
                };
            // 2026-11-16: snapshot 이후 바뀐 실행은 기존 상태를 덮지 않는다.
            if applied {
                run["agent_active"] = false.into();
                let current = run["status"].as_str().ok_or("invalid run status")?;
                let (status, kind, message) = if ["executing", "cancelling"].contains(&current) {
                    (
                        "uncertain",
                        "uncertain",
                        "중단된 외부 작업의 결과를 확인해야 합니다.",
                    )
                } else if success && verified {
                    ("completed", "completed", "검증과 검토가 완료되었습니다.")
                } else if success {
                    (
                        "failed",
                        "failed",
                        "검증 근거나 최종 검토가 현재 작업 내용과 일치하지 않습니다.",
                    )
                } else {
                    (
                        if [
                            "awaiting_approval",
                            "approved_exec",
                            "approved_mcp",
                            "uncertain",
                            "cancelled",
                        ]
                        .contains(&current)
                        {
                            current
                        } else {
                            "interrupted"
                        },
                        "interrupted",
                        "실행이 중단되었습니다. 저장된 checkpoint를 검토하세요.",
                    )
                };
                let status = status.to_owned();
                run["status"] = status.into();
                let events = run["events"].as_array_mut().ok_or("invalid run events")?;
                let seq = events
                    .last()
                    .map_or(Some(1), |e| {
                        e["sequence"].as_u64().and_then(|n| n.checked_add(1))
                    })
                    .ok_or("invalid event sequence")?;
                events.push(serde_json::json!({"sequence":seq,"run_id":id,"attempt":attempt,"kind":kind,"message":message}));
                table
                    .insert(id, run.to_string().as_str())
                    .map_err(|e| e.to_string())?;
            }
        }
        // 2026-11-16: 결과가 stale이어도 이미 끝난 동일 시도의 자원 표시는 남기지 않는다.
        if !applied
            && !cancelled_cleanup
            && run["project_id"] == project_id
            && run["agent_active"] == true
            && run["agent_attempt"] == attempt
        {
            run["agent_active"] = false.into();
            table
                .insert(id, run.to_string().as_str())
                .map_err(|e| e.to_string())?;
        }
        applied || cancelled_cleanup
    };
    {
        let mut table = tx.open_table(COMPLETIONS).map_err(|e| e.to_string())?;
        table
            .insert(completion_id, u8::from(applied))
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn bound_review_completes_once_with_real_git_snapshot() {
        use std::os::windows::process::CommandExt;
        let root = std::env::temp_dir().join(format!("agent-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .creation_flags(0x08000000)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.txt"), "before").unwrap();
        git(&["add", "src/a.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        let revision = git(&["rev-parse", "HEAD"]);
        git(&["worktree", "add", "--detach", "--", "../work", &revision]);
        let worktree = root.join("work");
        std::fs::write(worktree.join("src/a.txt"), "after").unwrap();
        let engine = crate::Engine::open(&root.join("state.redb")).unwrap();
        let db = engine.db.get().unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        let project_id = uuid::Uuid::new_v4().to_string();
        let command = "read_contains:src/a.txt:after";
        let spec = serde_json::json!({"version":1,"hash":"fixture","base_revision":revision,"scope":["src"],"requirements":["req"],"acceptance":["accept"],"tasks":[{"id":"task","dependencies":[],"owned_paths":["src"],"validation":[command]}],"task_bindings":[{"task_id":"task","goal":"goal","requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]}]});
        let project = serde_json::json!({"id":project_id,"name":"fixture","path":repo,"draft":{"title":"fixture","scope":[],"requirements":[],"acceptance":[],"mandatory_questions":[],"required_failures":[],"optional_issues":[],"base_revision":""},"draft_hash":"fixture","approved":spec,"context_version":2});
        let mut run = serde_json::json!({"id":run_id,"project_id":project_id,"spec_hash":"fixture","spec_version":1,"revision":revision,"scope":["src"],"worktree":worktree,"status":"awaiting_implementation","events":[],"context_version":2,"agent_active":true,"agent_attempt":1,"evidence":[],"custom":"preserved"});
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(crate::PROJECTS).unwrap();
            t.insert(project_id.as_str(), project.to_string().as_str())
                .unwrap();
        }
        {
            let mut t = tx.open_table(RUNS).unwrap();
            t.insert(run_id.as_str(), run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let diff = crate::runs::diff(&db, &run_id).unwrap();
        let id = serde_json::to_string(&("task", "final_validation", 0)).unwrap();
        let evidence = serde_json::json!({"verification_id":id,"revision":revision,"spec_hash":"fixture","command":command,"passed":true,"output":"fixture","worktree_hash":diff["worktree_hash"],"requirement_ids":["requirement-0"],"acceptance_ids":["acceptance-0"]});
        run["evidence"] = serde_json::json!([evidence]);
        run["evidence_contexts"] = serde_json::json!({id.clone():{"schema_version":1,"spec_version":1,"context_version":2,"agent_attempt":1,"task_id":"task"}});
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(RUNS).unwrap();
            t.insert(run_id.as_str(), run.to_string().as_str()).unwrap();
        }
        tx.commit().unwrap();
        let spec: domain::ApprovedSpec = serde_json::from_value(spec).unwrap();
        let evidence: Vec<domain::VerificationEvidence> =
            serde_json::from_value(run["evidence"].clone()).unwrap();
        let binding = ReviewBinding {
            schema_version: 1,
            spec_hash: "fixture".into(),
            spec_version: 1,
            context_version: 2,
            agent_attempt: 1,
            revision: revision.clone(),
            worktree_hash: diff["worktree_hash"].as_str().unwrap().into(),
            verification_ids: vec![id],
        };
        let (_, digest) = review_input(&spec, &evidence, &diff, &binding).unwrap();
        let mut old_only = run.clone();
        old_only["agent_attempt"] = 2.into();
        let saved_project: domain::Project = serde_json::from_value(project.clone()).unwrap();
        assert!(review_proof(&db, &run_id, &old_only, &saved_project)
            .unwrap()
            .is_none());
        let mut missing_binding: domain::Project = serde_json::from_value(project.clone()).unwrap();
        missing_binding
            .approved
            .as_mut()
            .unwrap()
            .task_bindings
            .clear();
        assert!(review_proof(&db, &run_id, &run, &missing_binding)
            .unwrap()
            .is_none());
        let second_id = uuid::Uuid::new_v4().to_string();
        let mut second = run.clone();
        second["id"] = second_id.clone().into();
        second["agent_attempt"] = 2.into();
        second["evidence_contexts"][&binding.verification_ids[0]]["agent_attempt"] = 2.into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table
                .insert(second_id.as_str(), second.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        let mut second_binding = binding.clone();
        second_binding.agent_attempt = 2;
        let (_, second_digest) = review_input(&spec, &evidence, &diff, &second_binding).unwrap();
        crate::runs::claim_call(&db, &second_id, "final-review", &second_digest).unwrap();
        crate::runs::complete_call(
            &db,
            &second_id,
            "final-review",
            serde_json::json!({"approved":true,"binding":second_binding}),
        )
        .unwrap();
        let second_manager = crate::completion::CompletionManager::empty(
            root.join("second-journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let second_record =
            crate::completion::CompletionManager::agent_record(&second_id, &project_id, true, 2);
        second_manager.preserve_failed_handoff(second_record.clone());
        assert_eq!(second_manager.retry(&db, &second_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &second_id).unwrap()["events"][0]["attempt"],
            2
        );
        second_manager.preserve_failed_handoff(second_record);
        assert_eq!(second_manager.retry(&db, &second_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &second_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for mode in [
            "failed_verification",
            "wrong_snapshot",
            "wrong_context",
            "wrong_binding",
            "missing_requirements",
            "wrong_requirements",
            "missing_acceptance",
            "wrong_acceptance",
        ] {
            let bad_id = uuid::Uuid::new_v4().to_string();
            let mut bad_run = run.clone();
            bad_run["id"] = bad_id.clone().into();
            match mode {
                "failed_verification" => bad_run["evidence"][0]["passed"] = false.into(),
                "wrong_snapshot" => bad_run["evidence"][0]["worktree_hash"] = "stale".into(),
                "wrong_context" => {
                    bad_run["evidence_contexts"][&binding.verification_ids[0]]["agent_attempt"] =
                        2.into()
                }
                "missing_requirements" => {
                    bad_run["evidence"][0]["requirement_ids"] = serde_json::json!([])
                }
                "wrong_requirements" => {
                    bad_run["evidence"][0]["requirement_ids"] =
                        serde_json::json!(["requirement-other"])
                }
                "missing_acceptance" => {
                    bad_run["evidence"][0]["acceptance_ids"] = serde_json::json!([])
                }
                "wrong_acceptance" => {
                    bad_run["evidence"][0]["acceptance_ids"] =
                        serde_json::json!(["acceptance-other"])
                }
                _ => {}
            }
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx.open_table(RUNS).unwrap();
                table
                    .insert(bad_id.as_str(), bad_run.to_string().as_str())
                    .unwrap();
            }
            tx.commit().unwrap();
            let bad_evidence: Vec<domain::VerificationEvidence> =
                serde_json::from_value(bad_run["evidence"].clone()).unwrap();
            let (_, bad_digest) = review_input(&spec, &bad_evidence, &diff, &binding).unwrap();
            crate::runs::claim_call(&db, &bad_id, "final-review", &bad_digest).unwrap();
            let mut bad_binding = binding.clone();
            if mode == "wrong_binding" {
                bad_binding.agent_attempt = 2;
            }
            crate::runs::complete_call(
                &db,
                &bad_id,
                "final-review",
                serde_json::json!({"approved":true,"binding":bad_binding}),
            )
            .unwrap();
            let manager = crate::completion::CompletionManager::empty(
                root.join(format!("{mode}-journal")),
                std::sync::Arc::new(std::sync::Mutex::new(())),
            );
            manager.preserve_failed_handoff(crate::completion::CompletionManager::agent_record(
                &bad_id,
                &project_id,
                true,
                1,
            ));
            assert_eq!(manager.retry(&db, &bad_id).unwrap(), "saved");
            assert_eq!(
                crate::runs::get(&db, &bad_id).unwrap()["status"],
                "failed",
                "{mode}"
            );
        }
        let legacy_run = uuid::Uuid::new_v4().to_string();
        let mut legacy = run.clone();
        legacy["id"] = legacy_run.clone().into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table
                .insert(legacy_run.as_str(), legacy.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        crate::runs::claim_call(&db, &legacy_run, "final-review", &digest).unwrap();
        crate::runs::complete_call(
            &db,
            &legacy_run,
            "final-review",
            serde_json::json!({"approved":true}),
        )
        .unwrap();
        let legacy_manager = crate::completion::CompletionManager::empty(
            root.join("legacy-journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let legacy_record =
            crate::completion::CompletionManager::agent_record(&legacy_run, &project_id, true, 1);
        legacy_manager.preserve_failed_handoff(legacy_record);
        assert_eq!(legacy_manager.retry(&db, &legacy_run).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &legacy_run).unwrap()["status"],
            "failed"
        );
        let racing_id = uuid::Uuid::new_v4().to_string();
        let mut racing = run.clone();
        racing["id"] = racing_id.clone().into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table
                .insert(racing_id.as_str(), racing.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        crate::runs::claim_call(&db, &racing_id, "final-review", &digest).unwrap();
        crate::runs::complete_call(
            &db,
            &racing_id,
            "final-review",
            serde_json::json!({"approved":true,"binding":binding}),
        )
        .unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        *TEST_AFTER_SNAPSHOT.lock().unwrap() = Some((racing_id.clone(), reached_tx, proceed_rx));
        let racing_record =
            crate::completion::CompletionManager::agent_record(&racing_id, &project_id, true, 1);
        let worker_db = std::sync::Arc::clone(&db);
        let worker = std::thread::spawn(move || {
            finish_agent(&worker_db, &Mutex::new(()), &racing_record).unwrap()
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            racing["status"] = "cancelled".into();
            table
                .insert(racing_id.as_str(), racing.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        proceed_tx.send(()).unwrap();
        assert!(worker.join().unwrap());
        let cancelled = crate::runs::get(&db, &racing_id).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["agent_active"], false);
        assert!(cancelled["events"].as_array().unwrap().is_empty());
        let stale_id = uuid::Uuid::new_v4().to_string();
        let mut stale = run.clone();
        stale["id"] = stale_id.clone().into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(RUNS).unwrap();
            table
                .insert(stale_id.as_str(), stale.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        crate::runs::claim_call(&db, &stale_id, "final-review", &digest).unwrap();
        crate::runs::complete_call(
            &db,
            &stale_id,
            "final-review",
            serde_json::json!({"approved":true,"binding":binding}),
        )
        .unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        *TEST_AFTER_SNAPSHOT.lock().unwrap() = Some((stale_id.clone(), reached_tx, proceed_rx));
        let record =
            crate::completion::CompletionManager::agent_record(&stale_id, &project_id, true, 1);
        let worker_db = std::sync::Arc::clone(&db);
        let worker =
            std::thread::spawn(move || finish_agent(&worker_db, &Mutex::new(()), &record).unwrap());
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let mut changed_project = project.clone();
        changed_project["context_version"] = 3.into();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::PROJECTS).unwrap();
            table
                .insert(project_id.as_str(), changed_project.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        proceed_tx.send(()).unwrap();
        assert!(!worker.join().unwrap());
        assert_eq!(
            crate::runs::get(&db, &stale_id).unwrap()["agent_active"],
            false
        );
        assert!(crate::runs::get(&db, &stale_id).unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty());
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::PROJECTS).unwrap();
            table
                .insert(project_id.as_str(), project.to_string().as_str())
                .unwrap();
        }
        tx.commit().unwrap();
        crate::runs::claim_call(&db, &run_id, "final-review", &digest).unwrap();
        crate::runs::complete_call(
            &db,
            &run_id,
            "final-review",
            serde_json::json!({"approved":true,"binding":binding}),
        )
        .unwrap();
        let manager = crate::completion::CompletionManager::empty(
            root.join("journal"),
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        let record =
            crate::completion::CompletionManager::agent_record(&run_id, &project_id, true, 1);
        manager.preserve_failed_handoff(record.clone());
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        let completed = crate::runs::get(&db, &run_id).unwrap();
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["events"].as_array().unwrap().len(), 1);
        assert_eq!(completed["custom"], "preserved");
        manager.preserve_failed_handoff(record);
        assert_eq!(manager.retry(&db, &run_id).unwrap(), "saved");
        assert_eq!(
            crate::runs::get(&db, &run_id).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(db);
        drop(engine);
        let reopened = redb::Database::open(root.join("state.redb")).unwrap();
        let stale = crate::runs::get(&reopened, &stale_id).unwrap();
        assert_eq!(stale["agent_active"], false);
        assert_eq!(stale["status"], "awaiting_implementation");
        assert!(stale["events"].as_array().unwrap().is_empty());
    }

    #[test]
    fn review_digest_binds_attempt_and_rejects_unknown_schema() {
        let spec: domain::ApprovedSpec = serde_json::from_value(serde_json::json!({
            "version":1,"hash":"spec","base_revision":"base","scope":[],"requirements":[],"acceptance":[]
        })).unwrap();
        let mut binding = ReviewBinding {
            schema_version: 1,
            spec_hash: "spec".into(),
            spec_version: 1,
            context_version: 2,
            agent_attempt: 1,
            revision: "revision".into(),
            worktree_hash: "snapshot".into(),
            verification_ids: vec!["v".into()],
        };
        let (input, digest) = review_input(&spec, &[], &serde_json::json!({}), &binding).unwrap();
        assert_eq!(input[0]["role"], "user");
        let request: serde_json::Value =
            serde_json::from_str(input[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(request["binding"]["agent_attempt"], 1);
        binding.agent_attempt = 2;
        assert_ne!(
            review_input(&spec, &[], &serde_json::json!({}), &binding)
                .unwrap()
                .1,
            digest
        );
        binding.schema_version = 2;
        assert!(review_input(&spec, &[], &serde_json::json!({}), &binding).is_err());
    }
}
