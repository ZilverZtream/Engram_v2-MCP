//! Mechanical current-worktree routing, separate from historical proposal evidence.
use super::Instruction;
use crate::services::project_policy;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

const PATH_LIMIT: usize = 50_000;
const EXAMPLE_LIMIT: usize = 8;

fn worktree_files(root: &Path) -> anyhow::Result<(Vec<String>, usize, bool)> {
    let repo = git2::Repository::open(root)?;
    let index = repo.index()?;
    let mut paths = BTreeSet::new();
    let mut unreadable = 0usize;
    let mut capped = false;
    for entry in index.iter() {
        if paths.len() >= PATH_LIMIT { capped = true; break; }
        match std::str::from_utf8(&entry.path) {
            Ok(path) => { paths.insert(path.to_owned()); }
            Err(_) => unreadable += 1,
        }
    }
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false)
        .renames_index_to_workdir(false).renames_head_to_index(false);
    // libgit2 enumerates status; PATH_LIMIT bounds retained paths and matching work,
    // not libgit2's internal status collection time or memory.
    match repo.statuses(Some(&mut options)) {
        Ok(statuses) => for entry in statuses.iter().filter(|e| e.status().contains(git2::Status::WT_NEW)) {
            if paths.len() >= PATH_LIMIT { capped = true; break; }
            if let Some(path) = entry.path() { paths.insert(path.to_owned()); }
            else { unreadable += 1; }
        },
        Err(_) => unreadable += 1,
    }
    let mut files = Vec::new();
    for path in paths {
        let Ok(full) = engram_core::safe_join(root, &path) else { unreadable += 1; continue; };
        match std::fs::symlink_metadata(full) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => files.push(path),
            Ok(_) => unreadable += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, // Tracked path deleted on disk.
            Err(_) => unreadable += 1,
        }
    }
    Ok((files, unreadable, capped))
}

pub(crate) fn observe(root: &Path, instructions: &[Instruction]) -> Value {
    let inventory = worktree_files(root);
    let (files, unreadable, capped, error) = match inventory {
        Ok((files, unreadable, capped)) => (files, unreadable, capped, None),
        Err(error) => (Vec::new(), 1, false, Some(error.to_string())),
    };
    let complete = unreadable == 0 && !capped;
    let rows: Vec<_> = instructions.iter().map(|instruction| {
        let mut row = json!({"instruction_path":instruction.path,
            "source_blake3":instruction.current.blake3,"status":"not_explicitly_scoped",
            "matching_files":null,"matching_files_is_exact":false,"examples":[],
            "interpretation":"Mechanical routing only. Scope matching does not interpret prose exclusions, prove policy correctness, confer authority or permit editing guidance."});
        let patterns = match project_policy::explicit_scope_patterns(&instruction.current.content) {
            Ok(Some(patterns)) => patterns,
            Ok(None) => { row["detail"] = json!("No explicit applyTo/globs/paths scope; implicit directory/shared guidance is not counted here."); return row; },
            Err(error) => { row["status"] = json!("invalid_scope"); row["detail"] = json!(error); return row; },
        };
        let compiled = patterns.iter().map(|p| project_policy::compile_scope(p)).collect::<Result<Vec<_>,_>>();
        row["patterns"] = json!(patterns);
        let compiled = match compiled {
            Ok(value) => value,
            Err(error) => { row["status"] = json!("invalid_scope"); row["detail"] = json!(error); return row; },
        };
        let matches: Vec<_> = files.iter().filter(|path| {
            project_policy::has_directory_authority(&instruction.path, std::slice::from_ref(*path))
                && compiled.iter().any(|pattern| pattern.is_match(path))
        }).collect();
        row["matching_files"] = json!(matches.len());
        row["matching_files_is_exact"] = json!(complete);
        row["examples"] = json!(matches.iter().take(EXAMPLE_LIMIT).collect::<Vec<_>>());
        row["examples_omitted"] = json!(matches.len().saturating_sub(EXAMPLE_LIMIT));
        row["status"] = json!(if !complete {"inventory_incomplete"} else if matches.is_empty() {"no_matching_files"} else {"matches_observed"});
        row["detail"] = json!(if !complete {"Inventory incomplete: count is a lower bound; zero does not establish missing routing."}
            else if matches.is_empty() {"Routing/coverage warning: this explicit scope matches no inventoried current files. Check spelling and intended future scope; this does not mean the instruction is wrong."}
            else {"Current files match the declared scope; semantic applicability and prose compliance remain unverified."});
        row
    }).collect();
    json!({"basis":"separate_current_worktree_routing_observation","inventory_complete":complete,
        "regular_files_observed":files.len(),"unreadable_or_unsupported_paths":unreadable,"retained_path_cap_reached":capped,"inventory_error":error,
        "inventory_scope":"Existing Git-tracked regular files plus nonignored untracked files; deleted paths are excluded. Symbolic links and unsupported/unreadable entries leave coverage incomplete. Matching uses Engram's existing scope dialect and directory authority.",
        "source_scope":"Instruction bytes and hashes come from the confirmed maintenance capture; path inventory is a separate current-worktree observation, not an atomic snapshot or historical inventory. Proposal packet/hash is unchanged by this observation.",
        "limits":{"retained_paths":PATH_LIMIT,"examples_per_instruction":EXAMPLE_LIMIT,"status_collection":"libgit2 enumerates current status before retained-path limiting; no total traversal time/memory bound claimed"},
        "instructions":rows})
}
