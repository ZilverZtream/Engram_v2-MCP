//! The ordinary pre-push audit reuses advisory pre-commit decision evidence.
use super::{head_commit, prior_context, source_snapshot};
use crate::{
    services::pre_commit_review_service::{ChangeType, DiffFile, parse_unified_diff},
    tools::Engram,
};
use serde_json::{Value, json};

fn exact_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.chars().any(char::is_control)
        && !path.starts_with(['/', '\\'])
        && !path.contains([':', '*', '?'])
        && !path
            .split(['/', '\\'])
            .any(|part| matches!(part, "" | "." | ".."))
}

fn report(context: Value, binding: Value) -> Value {
    json!({"prior_review_context":context,"prior_context_input_binding":binding})
}

impl Engram {
    pub(crate) async fn pre_push_prior_context(
        &self,
        project: &str,
        runtime: &crate::state::ProjectState,
        code: &str,
        file_path: Option<&str>,
    ) -> Value {
        let binding = json!({"input_bytes":code.len(),
            "input_blake3":blake3::hash(code.as_bytes()).to_hex().to_string(),
            "scope_origin":"not_assessed","patch_application_verified":false,
            "registered_project_directory":runtime.info.directory,
            "source_snapshot_phase":"after_quality_rule_retrieval; independent observations"});
        // This component's bound does not change acceptance of the original
        // audit input or replace an already-returned rule outcome.
        if code.len() > 4 * 1024 * 1024 {
            return report(
                prior_context::unavailable(
                    "Pre-push decision context input exceeds4MiB; rule retrieval above is unchanged",
                ),
                binding,
            );
        }
        let state = self.state.clone();
        let search = runtime.search.clone();
        let root = std::path::PathBuf::from(&runtime.info.directory);
        let project = project.to_owned();
        let code = code.to_owned();
        let hint = file_path.map(str::to_owned);
        let unavailable_binding = binding.clone();
        tokio::task::spawn_blocking(move || {
            let mut binding = binding;
            let mut files = parse_unified_diff(&code);
            if files.is_empty() {
                let Some(path) = hint.as_deref().filter(|path| exact_path(path)) else {
                    return report(prior_context::unavailable("No parsed diff paths or exact repository-relative file_path; decision scope was not assessed"), binding);
                };
                files.push(DiffFile {
                    path: path.replace('\\', "/"), change_type: ChangeType::Modified,
                    added_lines: vec![], removed_lines: vec![],
                    added_content: code.chars().take(1024).collect(), removed_content: String::new(),
                    hunks: vec![], is_binary: false,
                });
                binding["scope_origin"] = json!("explicit_file_path");
            } else {
                if files.iter().any(|file| !exact_path(&file.path)) {
                    return report(prior_context::unavailable("Diff contains unsupported non-relative or non-exact paths; decision scope was not assessed"), binding);
                }
                binding["scope_origin"] = json!("parsed_unified_diff_paths");
                if hint.is_some() {
                    binding["file_path_hint"] = json!("not_used_for_decision_scope; parsed diff paths take precedence");
                }
            }
            let _reservation = match engram_core::memory::AllocationGuard::try_new(
                &state.memory_budget, 64 * 1024 * 1024,
                engram_core::memory::Subsystem::Graph, "pre-push prior review evidence",
            ) {
                Ok(reservation) => reservation,
                Err(error) => return report(prior_context::unavailable(&format!("Prior-review budget unavailable: {error}")), binding),
            };
            // Match Input::from_diff's16-path bound before any source reads.
            let compared: Vec<_> = files.iter().take(16).cloned().collect();
            let snapshots = source_snapshot(&root, &compared);
            let head = head_commit(&root);
            binding["submitted_path_count"] = json!(files.len());
            binding["snapshot_path_count"] = json!(compared.len());
            let input = prior_context::Input::from_diff(&files, &snapshots);
            report(prior_context::collect(&state, &search, &project, head.as_deref(), &root, input), binding)
        }).await.unwrap_or_else(|error| report(
            prior_context::unavailable(&format!("Pre-push evidence worker unavailable: {error}")),
            unavailable_binding,
        ))
    }
}
