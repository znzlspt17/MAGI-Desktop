use crate::{err, PROJECTS};
use domain::{ApprovedSpec, Project, ResponseData, VerificationEvidence};
use integrations::PackageInput;
use redb::{Database, TableDefinition};
use std::path::Path;
const RUNS: TableDefinition<&str, &str> = TableDefinition::new("runs");

fn list(items: &[String]) -> String {
    items.iter().map(|item| format!("- {item}\n")).collect()
}
fn package(
    spec: &ApprovedSpec,
    readme: String,
    evidence: &[serde_json::Value],
    test_status: &str,
) -> Result<PackageInput, String> {
    let tasks = spec
        .tasks
        .iter()
        .map(|task| {
            format!(
                "## {}\n\n경로: {}\n의존성: {}\n검증: {}\n\n",
                task.id,
                task.owned_paths.join(", "),
                task.dependencies.join(", "),
                task.validation.join(", ")
            )
        })
        .collect::<String>();
    let verification = if evidence.is_empty() {
        format!(
            "# 검증 계획\n\n{}\n실제 실행 결과는 수집되지 않았습니다.\n",
            list(&spec.acceptance)
        )
    } else {
        format!(
            "# 검증 결과\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(evidence).map_err(err)?
        )
    };
    Ok(PackageInput {
        completed_id: spec.hash.clone(),
        readme,
        spec: if spec.markdown.is_empty() {
            format!(
                "# 승인 명세\n\n## 요구사항\n{}\n## 수용 기준\n{}",
                list(&spec.requirements),
                list(&spec.acceptance)
            )
        } else {
            spec.markdown.clone()
        },
        tasks: format!("# 작업\n\n{tasks}"),
        constraints: format!("# 제약\n\n{}", list(&spec.constraints)),
        verification,
        completeness: "limited".into(),
        test_status: test_status.into(),
    })
}
fn publish(input: &PackageInput, destination: &Path) -> Result<ResponseData, String> {
    let manifest = if destination
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.eq_ignore_ascii_case("zip"))
    {
        integrations::export_zip(input, destination)?
    } else {
        integrations::export_package(input, destination)?
    };
    Ok(ResponseData::Data(manifest))
}

// 2026-11-16: 승인된 명세만 내보내고 아직 수집하지 않은 검증 결과를 성공으로 표시하지 않는다.
pub(crate) fn export_project(
    db: &Database,
    project_id: &str,
    destination: &Path,
) -> Result<ResponseData, String> {
    let tx = db.begin_read().map_err(err)?;
    let project: Project = {
        let table = tx.open_table(PROJECTS).map_err(err)?;
        let row = table
            .get(project_id)
            .map_err(err)?
            .ok_or("project not found")?;
        serde_json::from_str(row.value()).map_err(err)?
    };
    drop(tx);
    let spec = project.approved.as_ref().ok_or("approval required")?;
    if project.draft_hash != spec.hash {
        return Err("approved specification changed".into());
    }
    let input = package(
        spec,
        format!(
            "# 구현 자료\n\n승인 해시: {}\n기준 revision: {}\n",
            spec.hash, spec.base_revision
        ),
        &[],
        "not_collected",
    )?;
    publish(&input, destination)
}

// 2026-11-16: 실행에 결속된 승인 버전과 근거만 골라 다른 시도나 revision의 결과가 섞이지 않게 한다.
pub(crate) fn export_run(
    db: &Database,
    run_id: &str,
    destination: &Path,
) -> Result<ResponseData, String> {
    let tx = db.begin_read().map_err(err)?;
    let run: serde_json::Value = {
        let table = tx.open_table(RUNS).map_err(err)?;
        let row = table.get(run_id).map_err(err)?.ok_or("run not found")?;
        serde_json::from_str(row.value()).map_err(err)?
    };
    let project_id = run["project_id"].as_str().ok_or("invalid run project")?;
    let project: Project = {
        let table = tx.open_table(PROJECTS).map_err(err)?;
        let row = table
            .get(project_id)
            .map_err(err)?
            .ok_or("project not found")?;
        serde_json::from_str(row.value()).map_err(err)?
    };
    drop(tx);
    let hash = run["spec_hash"].as_str().ok_or("invalid run spec")?;
    let version = run["spec_version"]
        .as_u64()
        .ok_or("invalid run spec version")?;
    let revision = run["revision"].as_str().ok_or("invalid run revision")?;
    let spec = project
        .approval_history
        .iter()
        .find(|spec| spec.hash == hash && spec.version == version && spec.base_revision == revision)
        .ok_or("run approval snapshot missing")?;
    let mut evidence = Vec::new();
    let mut exported_evidence = Vec::new();
    let mut total_evidence = 0usize;
    if let Some(items) = run["evidence"].as_array() {
        for item in items {
            let entry: VerificationEvidence = serde_json::from_value(item.clone()).map_err(err)?;
            let (attempt, context) = match crate::evidence::context(&run, item) {
                Ok(value) => value,
                Err(_) => {
                    total_evidence += 1;
                    continue;
                }
            };
            if crate::evidence::resolve(&run, &entry.verification_id, attempt).is_err() {
                total_evidence += 1;
                continue;
            }
            let current = run["agent_attempt"].as_u64() == Some(attempt);
            if current {
                total_evidence += 1;
            }
            if !entry.verification_id.is_empty()
                && !entry.worktree_hash.is_empty()
                && entry.spec_hash == hash
                && entry.revision == revision
                && context["schema_version"] == 1
                && context["spec_version"] == version
                && run["context_version"].as_u64().is_some()
                && run["agent_attempt"].as_u64().is_some()
                && context["context_version"] == run["context_version"]
                && context["task_id"].as_str().is_some_and(|v| !v.is_empty())
            {
                // 2026-11-16: 명령과 저장된 맥락을 함께 남겨 각 결과의 출처를 추적한다. 출력 원문은 포함하지 않는다.
                exported_evidence.push(serde_json::json!({
                    "verification_id": entry.verification_id,
                    "command": entry.command,
                    "passed": entry.passed,
                    "spec_hash": entry.spec_hash,
                    "spec_version": context["spec_version"],
                    "context_version": context["context_version"],
                    "agent_attempt": context["agent_attempt"],
                    "task_id": context["task_id"],
                    "revision": entry.revision,
                    "worktree_hash": entry.worktree_hash,
                    "requirement_ids": entry.requirement_ids,
                    "acceptance_ids": entry.acceptance_ids,
                }));
                if current {
                    evidence.push(entry);
                }
            }
        }
    }
    // 2026-11-16: 일부 검증만 통과한 결과가 전체 테스트 성공으로 보이지 않도록 승인된 작업별 근거를 확인한다.
    let mut expected = 0usize;
    let mut covered = true;
    for task in &spec.tasks {
        let mut bindings = spec
            .task_bindings
            .iter()
            .filter(|binding| binding.task_id == task.id);
        let binding = bindings.next();
        if binding.is_none() || bindings.next().is_some() {
            covered = false;
            continue;
        }
        let binding = binding.unwrap();
        if binding.requirement_ids.is_empty() || binding.acceptance_ids.is_empty() {
            covered = false;
        }
        for (position, command) in task.validation.iter().enumerate() {
            expected += 1;
            let verification_id =
                serde_json::to_string(&(&task.id, "final_validation", position)).map_err(err)?;
            let matches: Vec<_> = evidence
                .iter()
                .filter(|entry| entry.verification_id == verification_id)
                .collect();
            if matches.len() != 1
                || matches[0].command != *command
                || matches[0].requirement_ids != binding.requirement_ids
                || matches[0].acceptance_ids != binding.acceptance_ids
                || crate::evidence::resolve(
                    &run,
                    &verification_id,
                    run["agent_attempt"].as_u64().ok_or("invalid attempt")?,
                )?
                .is_none_or(|(_, context)| context["task_id"] != task.id)
            {
                covered = false;
            }
        }
    }
    covered &= expected > 0;
    let status = if evidence.iter().any(|entry| !entry.passed) {
        "failed"
    } else if !covered
        || evidence.is_empty()
        || evidence.len() != total_evidence
        || evidence
            .iter()
            .any(|entry| entry.worktree_hash != evidence[0].worktree_hash)
    {
        "not_collected"
    } else {
        "passed"
    };
    let input = package(
        spec,
        format!(
            "# 구현 자료\n\n승인 해시: {}\n실행 ID: {}\n기준 revision: {}\n상태: {}\n",
            spec.hash,
            run_id,
            revision,
            run["status"].as_str().ok_or("invalid run status")?
        ),
        &exported_evidence,
        status,
    )?;
    publish(&input, destination)
}
