use super::*;
use crate::services::instruction_maintenance::{self, MaintenanceRequest};
use serde_json::{Value, json};

#[derive(PartialEq, Eq)]
struct Identity {
    directory: String,
    project_type: String,
    created_at_ms: u64,
    epoch: Option<u64>,
}

fn review_and_confirm(
    root: PathBuf,
    pid: String,
    request: MaintenanceRequest,
) -> anyhow::Result<(bool, instruction_maintenance::Report)> {
    let report = instruction_maintenance::review(&root, &pid, &request)?;
    let confirmed = instruction_maintenance::capture(&root, &pid, &request)?;
    Ok((report.packet == confirmed, report))
}

impl Engram {
    async fn instruction_observation_identity(&self, project_id: &str) -> anyhow::Result<Identity> {
        let registry = self.state.registry.clone();
        let pid = project_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let before = registry.source_identity_epoch(&pid)?;
            let project = registry
                .get_project(&pid)?
                .ok_or_else(|| anyhow::anyhow!("Project is unavailable"))?;
            let epoch = registry.source_identity_epoch(&pid)?;
            anyhow::ensure!(
                before == epoch,
                "Project identity changed while collecting its record"
            );
            Ok(Identity {
                directory: project.directory,
                project_type: project.project_type,
                created_at_ms: project.created_at_ms,
                epoch,
            })
        })
        .await?
    }

    /// Optional observations must never turn a completed source operation into
    /// an error implying rollback. Failures are an explicit separate outcome.
    pub(super) async fn collect_instruction_observation(
        &self,
        project_id: &str,
        request: MaintenanceRequest,
        basis: &'static str,
    ) -> Value {
        self.collect_instruction_observation_with(project_id, request, basis, review_and_confirm)
            .await
    }

    async fn collect_instruction_observation_with<F>(
        &self,
        project_id: &str,
        request: MaintenanceRequest,
        basis: &'static str,
        execute: F,
    ) -> Value
    where
        F: FnOnce(
                PathBuf,
                String,
                MaintenanceRequest,
            ) -> anyhow::Result<(bool, instruction_maintenance::Report)>
            + Send
            + 'static,
    {
        let started = now_ms();
        let mut observation = json!({
            "status":"unavailable", "basis":basis,
            "atomic_index_and_policy_snapshot":false, "authored_files_written":0,
            "project_identity_continuity_established":false,
            "policy_correctness":"not_established_by_reference_observation",
            "collection_started_at_ms":started
        });
        let expected = match self.instruction_observation_identity(project_id).await {
            Ok(identity) => identity,
            Err(error) => {
                observation["reason"] = json!(format!("Project identity unavailable: {error:#}"));
                return json!({"instruction_reference_observation":observation});
            }
        };
        let root = expected.directory.clone();
        let pid = project_id.to_owned();
        let reviewed =
            tokio::task::spawn_blocking(move || execute(root.into(), pid, request)).await;
        observation["collection_completed_at_ms"] = json!(now_ms());
        observation["project_directory"] = json!(expected.directory);
        observation["project_type"] = json!(expected.project_type);
        observation["project_identity_epoch"] = json!(expected.epoch);
        observation["identity_authority"] = json!(if expected.epoch.is_some() {
            "record_and_epoch_checked"
        } else {
            "record_checked_epoch_unavailable"
        });
        match self.instruction_observation_identity(project_id).await {
            Ok(current) if current == expected => {
                observation["project_identity_continuity_established"] =
                    json!(expected.epoch.is_some());
            }
            Ok(_) => {
                observation["status"] = json!("stale");
                observation["reason"] =
                    json!("Project identity changed during instruction observation; recollect");
                return json!({"instruction_reference_observation":observation});
            }
            Err(error) => {
                observation["status"] = json!("stale");
                observation["reason"] = json!(format!(
                    "Project identity could not be confirmed: {error:#}"
                ));
                return json!({"instruction_reference_observation":observation});
            }
        }
        match reviewed {
            Ok(Ok((true, report))) => {
                observation["status"] = json!("observed");
                observation["packet_blake3"] = json!(report.packet_blake3);
                observation["manifest"] = json!(report.packet.instruction_manifest);
                observation["coverage"] = json!(report.coverage);
                observation["proposal_tool"] = json!("pre_push_audit");
            }
            Ok(Ok((false, _))) => {
                observation["status"] = json!("stale");
                observation["reason"] =
                    json!("Instruction evidence changed during collection; recollect");
            }
            Ok(Err(error)) => {
                observation["reason"] =
                    json!(format!("Instruction observation unavailable: {error:#}"))
            }
            Err(error) => {
                observation["reason"] =
                    json!(format!("Instruction observation worker failed: {error}"))
            }
        }
        json!({"instruction_reference_observation":observation})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use engram_core::{Config, ProjectRecord};

    #[tokio::test]
    async fn retargeted_project_cannot_receive_old_roots_instruction_manifest() -> anyhow::Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let result = async {
            let root = temporary.path().join("original");
            let replacement = temporary.path().join("replacement");
            std::fs::create_dir(&root)?;
            std::fs::create_dir(&replacement)?;
            let policy = "---\napplyTo: helper.rs\n---\nUse OldHelper.\n";
            std::fs::write(root.join("area.md"), policy)?;
            std::fs::write(root.join("helper.rs"), "pub fn OldHelper() {}\n")?;
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            let base = repo.commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])?;
            std::fs::write(root.join("helper.rs"), "pub fn NewHelper() {}\n")?;
            let cfg = Config {
                data_dir: temporary.path().join("data"),
                allowed_roots: vec![root.clone(), replacement.clone()],
                embedding_backend: "fts_only".into(),
                ..Default::default()
            };
            std::fs::create_dir(&cfg.data_dir)?;
            let (state, _) = AppState::new(cfg)?;
            let mut project = ProjectRecord {
                project_id: "maintenance".into(),
                project_name: "Fixture".into(),
                project_type: "rust".into(),
                directory: root.to_string_lossy().into_owned(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            };
            state.registry.put_project(&project)?;
            let registry = state.registry.clone();
            let engram = Engram::new(state);
            let request = MaintenanceRequest {
                new_instruction_targets: Vec::new(),
                instruction_paths: vec!["area.md".into()],
                evidence_paths: vec!["helper.rs".into()],
                base_commit: base.to_string(),
                draft: None,
            };
            let response = engram
                .collect_instruction_observation_with(
                    "maintenance",
                    request,
                    "separate_worktree_observation_collected_after_source_update",
                    move |observed_root, pid, request| {
                        // Initial identity has already been captured. Mutate the actual
                        // registry during collection, while the old evidence stays valid.
                        project.directory = replacement.to_string_lossy().into_owned();
                        registry.put_project(&project)?;
                        review_and_confirm(observed_root, pid, request)
                    },
                )
                .await;
            let observation = &response["instruction_reference_observation"];
            anyhow::ensure!(observation["status"] == "stale", "{response}");
            anyhow::ensure!(
                observation.get("manifest").is_none() && observation.get("packet_blake3").is_none()
            );
            anyhow::ensure!(observation["project_identity_continuity_established"] == false);
            anyhow::ensure!(observation["authored_files_written"] == 0);
            anyhow::ensure!(std::fs::read_to_string(root.join("area.md"))? == policy);
            Ok(())
        }
        .await;
        temporary.close()?;
        result
    }
}
