//! Inverse literal-reference discovery for maintenance, never scope/authority adoption.
use super::super::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const DOCUMENT_LIMIT: usize = 64;
const READ_LIMIT: usize = 2 * 1024 * 1024;
const LINK_LIMIT: usize = 256;
const DEPTH_LIMIT: usize = 3;
const MATCH_LIMIT: usize = 16;
const STEP_LIMIT: usize = 4096;

pub(super) struct Candidate {
    pub path: String,
    pub current_blake3: String,
    pub current_bytes: usize,
    pub matches: Vec<Value>,
    pub selection_basis: &'static str,
}
pub(super) struct Discovery {
    pub candidates: Vec<Candidate>,
    pub omissions: Vec<Value>,
    pub notes: Vec<String>,
}

struct Reader<'a> {
    root: &'a Path,
    repo: &'a git2::Repository,
    tree: &'a git2::Tree<'a>,
    cache: BTreeMap<String, (Option<Snapshot>, Option<Snapshot>)>,
    remaining: usize,
    omissions: Vec<Value>,
}
impl Reader<'_> {
    fn load(&mut self, path: &str) -> bool {
        if self.cache.contains_key(path) {
            return true;
        }
        if self.cache.len() >= DOCUMENT_LIMIT {
            self.omissions
                .push(json!({"path":path,"reason":"reference_document_limit"}));
            return false;
        }
        let mut pair = (None, None);
        for before in [true, false] {
            let previous = self.remaining;
            let read = if before {
                committed(self.repo, self.tree, path, &mut self.remaining, FILE_LIMIT)
            } else {
                disk(self.root, path, &mut self.remaining, FILE_LIMIT)
            };
            match read {
                Ok(snapshot) => {
                    if before {
                        pair.0 = snapshot;
                    } else {
                        pair.1 = snapshot;
                    }
                }
                Err(error) => {
                    self.remaining = previous;
                    self.omissions.push(json!({"path":path,"version":if before{"before"}else{"after"},"reason":"complete_reference_source_unavailable","detail":format!("{error:#}")}));
                }
            }
        }
        self.cache.insert(path.into(), pair);
        true
    }
}
fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}
fn guidance(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "md" | "mdc" | "markdown"
        )
    })
}
fn paths_for(source: &str, link: &manifest::markdown_links::Link) -> Vec<String> {
    if link.unresolved_status.is_some() || !matches!(link.kind, "link" | "inline_code_path") {
        return vec![];
    }
    let target = link.target.replace('\\', "/");
    if link.kind == "inline_code_path" {
        if target.contains(['*', '?']) {
            return vec![];
        }
        [source, "project-root-reference.md"]
            .into_iter()
            .filter_map(|base| manifest::normalize(base, &target).map(|(path, _)| path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        manifest::normalize(source, &link.target)
            .map(|(path, _)| vec![path])
            .unwrap_or_default()
    }
}

pub(super) fn discover(
    root: &Path,
    req: &MaintenanceRequest,
    changed: &[String],
    selected: &[String],
) -> Result<Discovery> {
    let (paths, notes) = crate::services::project_policy::maintenance_candidates(root, changed);
    let repo = git2::Repository::open(root)?;
    let tree = repo
        .find_commit(git2::Oid::from_str(&req.base_commit)?)?
        .tree()?;
    let symbol_inputs = if paths.iter().any(|path| !selected.iter().any(|selected| same_path(path, selected))) {
        changed
    } else {
        &[]
    };
    let symbols = super::symbols::Discovery::new(root, &repo, &tree, symbol_inputs);
    let mut reader = Reader {
        root,
        repo: &repo,
        tree: &tree,
        cache: BTreeMap::new(),
        remaining: READ_LIMIT,
        omissions: symbols.omissions.clone(),
    };
    let mut candidates = Vec::new();
    let mut steps = 0usize;
    let mut exhausted = false;
    for instruction in paths {
        if selected.iter().any(|path| same_path(path, &instruction)) {
            continue;
        }
        if !reader.load(&instruction) {
            continue;
        }
        let Some(current) = reader.cache[&instruction].1.as_ref() else {
            continue;
        };
        let (current_blake3, current_bytes) = (current.blake3.clone(), current.content.len());
        if changed.iter().any(|path| same_path(path, &instruction)) {
            candidates.push(Candidate {
                path: instruction.clone(), current_blake3, current_bytes,
                matches: vec![json!({"kind":"changed_instruction_source","path":instruction,
                    "detail":"Reviewing the changed guide does not extend its scope to other files."})],
                selection_basis: "directly_changed_conventional_instruction",
            });
            continue;
        }
        let instruction_source = Instruction {
            path: instruction.clone(),
            current: current.clone(),
            before: reader.cache[&instruction].0.clone(),
        };
        let symbol_matches = symbols.matches(&instruction_source, &mut reader.omissions);
        let mut matches = Vec::new();
        let mut queue = VecDeque::from([
            (instruction.clone(), true, Vec::<Value>::new()),
            (instruction.clone(), false, Vec::<Value>::new()),
        ]);
        let mut seen = BTreeSet::new();
        'walk: while let Some((source, before, chain)) = queue.pop_front() {
            if !seen.insert((source.clone(), before)) || !reader.load(&source) {
                continue;
            }
            let snapshot = if before {
                &reader.cache[&source].0
            } else {
                &reader.cache[&source].1
            };
            let Some(snapshot) = snapshot else {
                continue;
            };
            let hash = snapshot.blake3.clone();
            let parsed = manifest::markdown_links::parse(&snapshot.content, LINK_LIMIT);
            for (status, (offset, count)) in parsed.issues {
                reader.omissions.push(json!({"path":source,"version":if before{"before"}else{"after"},"byte_offset":offset,"reason":status,"occurrences":count}));
            }
            for link in parsed.links {
                if steps >= STEP_LIMIT {
                    reader
                        .omissions
                        .push(json!({"reason":"reference_visit_limit","limit":STEP_LIMIT}));
                    exhausted = true;
                    break 'walk;
                }
                steps += 1;
                let targets = paths_for(&source, &link);
                if let Some(status) = link.unresolved_status {
                    reader.omissions.push(json!({"path":source,"version":if before{"before"}else{"after"},"byte_offset":link.offset,"reason":status}));
                }
                let proof = json!({"source_path":source,"source_blake3":hash,"version":if before{"before"}else{"after"},
                    "byte_range":[link.offset,link.end],"target":link.target,"candidate_paths":targets,
                    "path_basis":if link.kind=="inline_code_path"{"document_and_project_candidates_not_binding"}else{"document_relative_markdown"}});
                let mut next_chain = chain.clone();
                next_chain.push(proof);
                for target in targets {
                    if let Some(changed_path) = changed.iter().find(|path| same_path(path, &target))
                    {
                        if matches.len() < MATCH_LIMIT {
                            matches.push(json!({"changed_path":changed_path,"reference_chain":next_chain,
                                "interpretation":"Literal reference candidate for maintenance review. Inline path candidates may be ambiguous; a reference is not scope authority, symbol binding or semantic impact."}));
                        } else {
                            reader.omissions.push(json!({"path":instruction,"reason":"reference_match_limit","limit":MATCH_LIMIT}));
                        }
                    } else if guidance(&target) && !seen.contains(&(target.clone(), before)) {
                        if next_chain.len() < DEPTH_LIMIT {
                            queue.push_back((target, before, next_chain.clone()));
                        } else {
                            reader.omissions.push(json!({"path":target,"reason":"reference_depth_limit","source_path":source}));
                        }
                    }
                }
            }
        }
        for proof in symbol_matches {
            if matches.len() >= MATCH_LIMIT {
                reader.omissions.push(json!({"path":instruction,"reason":"combined_reference_match_limit","limit":MATCH_LIMIT}));
                break;
            }
            matches.push(proof);
        }
        if !matches.is_empty() {
            let has_symbols = matches.iter().any(|proof| proof["kind"] == "symbol_reference");
            let has_paths = matches.iter().any(|proof| proof["kind"] != "symbol_reference");
            candidates.push(Candidate {
                path: instruction,
                current_blake3,
                current_bytes,
                matches,
                selection_basis: if has_symbols && has_paths {
                    "literal_reference_and_changed_symbol"
                } else if has_symbols {
                    "changed_symbol_reference"
                } else {
                    "literal_reference_to_changed_evidence"
                },
            });
        }
        if exhausted {
            break;
        }
    }
    // Retain aggregate gaps rather than hundreds of repeated budget rows.
    let mut omissions = reader.omissions;
    let omitted_details = omissions.len().saturating_sub(32);
    omissions.truncate(32);
    if omitted_details > 0 {
        omissions
            .push(json!({"reason":"additional_reference_discovery_gaps","count":omitted_details}));
    }
    Ok(Discovery {
        candidates,
        omissions,
        notes,
    })
}
