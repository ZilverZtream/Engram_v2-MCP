//! Bounded reference observations, not acceptance of linked policy.
use super::*;
use std::collections::{BTreeMap, VecDeque};

mod markdown_links;
mod symbols;

const DEPENDENCY_LIMIT: usize = 16;
const EDGE_LIMIT: usize = 256;
const DEPTH_LIMIT: usize = 3;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Observation {
    pub status: &'static str,
    pub snapshot: Option<Snapshot>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Dependency {
    pub path: String,
    pub before: Observation,
    pub after: Observation,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Edge {
    pub source_path: String,
    pub source_blake3: String,
    pub version: Version,
    /// Byte range of the actual Markdown use in this exact source snapshot.
    pub byte_offset: usize,
    pub byte_end: usize,
    pub kind: &'static str,
    pub reference_label: Option<String>,
    /// End-exclusive UTF-8 byte range of the chosen document-local CommonMark
    /// definition, in the same source hash/version as the use above.
    pub definition_byte_range: Option<[usize; 2]>,
    pub target: String,
    pub normalized_path: Option<String>,
    /// Inline-code paths have no Markdown relative-base contract. Retain both
    /// document/project candidates and never silently choose conflicting files.
    pub candidate_paths: Vec<String>,
    pub path_basis: &'static str,
    /// A Markdown fragment is not proof that a code symbol or heading exists.
    pub fragment: Option<String>,
    pub status: &'static str,
    pub traversal: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CoverageIssue {
    pub source_path: String,
    pub version: Version,
    pub byte_offset: usize,
    pub status: &'static str,
    pub omitted_occurrences: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MoveCandidate {
    pub old_path: String,
    pub source_paths: Vec<String>,
    pub candidate_paths: Vec<String>,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Scope {
    pub status: &'static str,
    /// Exact frontmatter bytes, without interpreting applyTo/paths as authority.
    /// Complete guidance is retained even if this duplicate display is omitted.
    pub raw_frontmatter: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub before_blake3: Option<String>,
    pub after_blake3: Option<String>,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InstructionState {
    pub instruction_path: String,
    pub source_blake3: String,
    pub before_blake3: Option<String>,
    pub evidence_commit: String,
    pub scope: Scope,
    pub before_scope: Option<Scope>,
    pub dependent_changes: Vec<Change>,
    pub line_ending_differences: Vec<Change>,
    pub unresolved_edge_indices: Vec<usize>,
    pub coverage_issue_indices: Vec<usize>,
    pub symbol_reference_indices: Vec<usize>,
    pub symbol_changes: Vec<symbols::Change>,
    pub status: &'static str,
    pub coverage_status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Manifest {
    /// Only explicit instructions are editable; these are evidence dependencies.
    pub dependencies: Vec<Dependency>,
    pub edges: Vec<Edge>,
    pub coverage_issues: Vec<CoverageIssue>,
    pub move_candidates: Vec<MoveCandidate>,
    pub symbol_references: Vec<symbols::Reference>,
    pub symbol_inventory: Vec<symbols::Inventory>,
    pub instructions: Vec<InstructionState>,
    pub coverage: &'static str,
}

struct View<'a> {
    snapshot: Option<&'a Snapshot>,
    status: &'static str,
}

fn view<'a>(
    path: &str,
    version: &Version,
    instructions: &'a [Instruction],
    evidence: &'a [Evidence],
    dependencies: &'a BTreeMap<String, Dependency>,
) -> Option<View<'a>> {
    if let Some(input) = instructions.iter().find(|i| i.path == path) {
        let snapshot = match version {
            Version::Before => input.before.as_ref(),
            Version::After => Some(&input.current),
        };
        return Some(View {
            status: if snapshot.is_some() {
                "complete"
            } else {
                "absent"
            },
            snapshot,
        });
    }
    if let Some(input) = evidence.iter().find(|i| i.path == path) {
        let snapshot = match version {
            Version::Before => input.before.as_ref(),
            Version::After => input.after.as_ref(),
        };
        return Some(View {
            status: if snapshot.is_some() {
                "complete"
            } else {
                "absent"
            },
            snapshot,
        });
    }
    dependencies.get(path).map(|input| {
        let observation = match version {
            Version::Before => &input.before,
            Version::After => &input.after,
        };
        View {
            snapshot: observation.snapshot.as_ref(),
            status: observation.status,
        }
    })
}

fn observe(
    remaining: &mut usize,
    read: impl FnOnce(&mut usize) -> Result<Option<Snapshot>>,
) -> Observation {
    let previous = *remaining;
    match read(remaining) {
        Ok(snapshot) => Observation {
            status: if snapshot.is_some() {
                "complete"
            } else {
                "absent"
            },
            snapshot,
            detail: None,
        },
        Err(error) => {
            // Failed UTF-8 conversion can consume the byte reservation without
            // retaining a snapshot. Only complete returned bodies use the budget.
            *remaining = previous;
            Observation {
                status: if format!("{error:#}").contains("byte budget") {
                    "byte_budget_omitted"
                } else {
                    "unreadable_or_unsafe"
                },
                snapshot: None,
                detail: Some(format!("{error:#}")),
            }
        }
    }
}

fn guidance(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md") || path.ends_with(".mdc") || path.ends_with(".markdown")
}

fn line_ending_equivalent(before: &str, after: &str) -> bool {
    fn normalized(text: &str) -> impl Iterator<Item = u8> + '_ {
        text.bytes()
            .enumerate()
            .filter(move |(i, byte)| {
                !(*byte == b'\r' && text.as_bytes().get(i + 1) == Some(&b'\n'))
            })
            .map(|(_, byte)| byte)
    }
    normalized(before).eq(normalized(after))
}

fn normalize(source: &str, target: &str) -> Option<(String, Option<String>)> {
    let (file, fragment) = target
        .split_once('#')
        .map_or((target, None), |(f, a)| (f, Some(a.to_owned())));
    if file.starts_with('/')
        || file.contains([':', '\\', '?', '%', '<', '>'])
        || file.chars().any(char::is_control)
    {
        return None;
    }
    let mut parts: Vec<_> = source.split('/').collect();
    if !file.is_empty() {
        parts.pop();
        for part in file.split('/') {
            match part {
                ".." => {
                    parts.pop()?;
                }
                "" | "." => {}
                part => parts.push(part),
            }
        }
    }
    let path = parts.join("/");
    if path.is_empty() || path.len() > 512 {
        return None;
    }
    Some((path, fragment))
}

fn collect_dependency(
    root: &Path,
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    path: &str,
    version: &Version,
    depth: usize,
    instructions: &[Instruction],
    evidence: &[Evidence],
    dependencies: &mut BTreeMap<String, Dependency>,
    excluded_paths: &[String],
    remaining: &mut usize,
) -> Option<&'static str> {
    if excluded_paths.iter().any(|scope| super::area_evidence::within(path, scope)) {
        return Some("excluded_area_boundary");
    }
    if view(path, version, instructions, evidence, dependencies).is_some() {
        return None;
    }
    if depth >= DEPTH_LIMIT {
        return Some("depth_budget_omitted");
    }
    if dependencies.len() >= DEPENDENCY_LIMIT {
        return Some("dependency_budget_omitted");
    }
    let before = observe(remaining, |r| committed(repo, tree, path, r));
    let after = observe(remaining, |r| disk(root, path, r));
    dependencies.insert(
        path.to_owned(),
        Dependency {
            path: path.to_owned(),
            before,
            after,
        },
    );
    None
}

pub(super) fn scope(snapshot: &Snapshot) -> Scope {
    scope_with_limit(snapshot, 16 * 1024)
}

pub(super) fn scope_with_limit(snapshot: &Snapshot, display_limit: usize) -> Scope {
    let content = &snapshot.content;
    let mut lines = content.split_inclusive('\n');
    if lines.next().map(|s| {
        s.trim_start_matches('\u{feff}')
            .trim_end_matches(['\r', '\n'])
    }) != Some("---")
    {
        return Scope {
            status: "no_frontmatter",
            raw_frontmatter: None,
        };
    }
    let mut end = content.find('\n').map_or(content.len(), |at| at + 1);
    for line in lines {
        end += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Scope {
                status: if end <= display_limit {
                    "raw_uninterpreted"
                } else {
                    "display_budget_omitted_see_complete_source"
                },
                raw_frontmatter: (end <= display_limit).then(|| content[..end].to_owned()),
            };
        }
    }
    Scope {
        status: "unterminated_frontmatter_uninterpreted",
        raw_frontmatter: None,
    }
}

pub(super) fn capture(
    root: &Path,
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    base: &str,
    instructions: &[Instruction],
    evidence: &[Evidence],
    additional_roots: &[String],
    excluded_paths: &[String],
    remaining: &mut usize,
) -> Result<Manifest> {
    let mut dependencies = BTreeMap::new();
    let mut edges = Vec::new();
    let mut issues = Vec::new();
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    for instruction in instructions {
        for version in [Version::Before, Version::After] {
            queue.push_back((instruction.path.clone(), version, 0));
        }
    }
    for path in additional_roots {
        for version in [Version::Before, Version::After] {
            queue.push_back((path.clone(), version, 0));
        }
    }
    while let Some((source, version, depth)) = queue.pop_front() {
        if !visited.insert((source.clone(), version.clone())) {
            continue;
        }
        let Some(snapshot) =
            view(&source, &version, instructions, evidence, &dependencies).and_then(|v| v.snapshot)
        else {
            continue;
        };
        let hash = snapshot.blake3.clone();
        // Parser input is the complete bounded snapshot. Retained occurrences
        // obey the shared edge limit; omissions remain explicit and scoped.
        let available = EDGE_LIMIT.saturating_sub(edges.len());
        let parsed = markdown_links::parse(&snapshot.content, available);
        for (status, (offset, count)) in parsed.issues {
            issues.push(CoverageIssue {
                source_path: source.clone(),
                version: version.clone(),
                byte_offset: offset,
                status,
                omitted_occurrences: count,
            });
        }
        // All borrows of source bodies end before dependency insertion.
        for link in parsed.links {
            let mut edge = Edge {
                source_path: source.clone(),
                source_blake3: hash.clone(),
                version: version.clone(),
                byte_offset: link.offset,
                byte_end: link.end,
                kind: link.kind,
                reference_label: link.reference_label,
                definition_byte_range: link.definition_byte_range,
                target: link.target,
                normalized_path: None,
                candidate_paths: Vec::new(),
                path_basis: "document_relative_markdown",
                fragment: None,
                status: "unresolved_reference_outside_project_or_unsupported",
                traversal: "not_followed",
            };
            if let Some(status) = link.unresolved_status {
                edge.status = status;
            } else if edge.kind == "inline_code_path" {
                edge.path_basis = "document_and_project_relative_candidates";
                let literal = edge.target.replace('\\', "/");
                edge.fragment = literal
                    .split_once('#')
                    .map(|(_, fragment)| fragment.to_owned());
                if literal.contains(['*', '?']) {
                    edge.status = "literal_path_pattern_unexpanded";
                } else {
                    let candidates: BTreeSet<_> = [source.as_str(), "project-root-reference.md"]
                        .into_iter()
                        .filter_map(|base| normalize(base, &literal).map(|(path, _)| path))
                        .collect();
                    edge.candidate_paths = candidates.iter().cloned().collect();
                    let mut present = Vec::new();
                    let mut incomplete = None;
                    for path in candidates {
                        let omitted = collect_dependency(
                            root,
                            repo,
                            tree,
                            &path,
                            &version,
                            depth,
                            instructions,
                            evidence,
                            &mut dependencies,
                            excluded_paths,
                            remaining,
                        );
                        let status = omitted.unwrap_or_else(|| {
                            view(&path, &version, instructions, evidence, &dependencies)
                                .map_or("unavailable", |v| v.status)
                        });
                        match status {
                            "complete" => present.push(path),
                            "absent" => {}
                            status => {
                                incomplete.get_or_insert(status);
                            }
                        }
                    }
                    if present.len() > 1 {
                        edge.status = "ambiguous_literal_path_base";
                    } else if let Some(status) = incomplete {
                        edge.status = status;
                    } else if let Some(path) = present.pop() {
                        edge.normalized_path = Some(path);
                        edge.status = "complete";
                        if edge.fragment.is_some() {
                            edge.status = "content_complete_fragment_unverified";
                        }
                        edge.path_basis = "only_present_document_or_project_relative_candidate";
                    } else if !edge.candidate_paths.is_empty() {
                        edge.status = "absent";
                    }
                }
            } else if edge.target.contains("://") || edge.target.starts_with("mailto:") {
                edge.status = "external_reference_unverified";
            } else if let Some((path, fragment)) = normalize(&source, &edge.target) {
                edge.normalized_path = Some(path.clone());
                edge.fragment = fragment;
                if let Some(status) = collect_dependency(
                    root,
                    repo,
                    tree,
                    &path,
                    &version,
                    depth,
                    instructions,
                    evidence,
                    &mut dependencies,
                    excluded_paths,
                    remaining,
                ) {
                    edge.status = status;
                }
                if let Some(observation) =
                    view(&path, &version, instructions, evidence, &dependencies)
                {
                    edge.status = if observation.status == "complete" && edge.fragment.is_some() {
                        "content_complete_fragment_unverified"
                    } else {
                        observation.status
                    };
                }
            }
            if let Some(path) = edge.normalized_path.as_ref()
                && view(path, &version, instructions, evidence, &dependencies)
                    .is_some_and(|v| v.snapshot.is_some())
                && guidance(path)
            {
                if visited.contains(&(path.clone(), version.clone())) {
                    edge.traversal = "already_visited";
                } else if depth >= DEPTH_LIMIT {
                    edge.traversal = "depth_budget_omitted";
                } else {
                    queue.push_back((path.clone(), version.clone(), depth + 1));
                    edge.traversal = "queued";
                }
            }
            edges.push(edge);
        }
    }
    let (symbol_references, symbol_inventory) =
        symbols::capture(instructions, evidence, &dependencies, &mut issues);
    let paths: BTreeSet<_> = instructions
        .iter()
        .map(|i| i.path.clone())
        .chain(evidence.iter().map(|e| e.path.clone()))
        .chain(dependencies.keys().cloned())
        .collect();
    let mut moves = Vec::new();
    for old in &paths {
        let before =
            view(old, &Version::Before, instructions, evidence, &dependencies).expect("known path");
        let after =
            view(old, &Version::After, instructions, evidence, &dependencies).expect("known path");
        if let (Some(before), None, "absent") = (before.snapshot, after.snapshot, after.status) {
            let candidates: Vec<_> = paths
                .iter()
                .filter(|new| {
                    let previous =
                        view(new, &Version::Before, instructions, evidence, &dependencies)
                            .expect("known path");
                    let current = view(new, &Version::After, instructions, evidence, &dependencies)
                        .expect("known path");
                    previous.status == "absent"
                        && current.snapshot.is_some_and(|s| s.blake3 == before.blake3)
                })
                .cloned()
                .collect();
            if !candidates.is_empty() {
                let sources: Vec<_> = paths
                    .iter()
                    .filter(|path| {
                        let original = view(
                            path,
                            &Version::Before,
                            instructions,
                            evidence,
                            &dependencies,
                        )
                        .expect("known path");
                        let current =
                            view(path, &Version::After, instructions, evidence, &dependencies)
                                .expect("known path");
                        current.status == "absent"
                            && original.snapshot.is_some_and(|s| s.blake3 == before.blake3)
                    })
                    .cloned()
                    .collect();
                moves.push(MoveCandidate {
                    old_path: old.clone(),
                    status: if candidates.len() == 1 && sources.len() == 1 {
                        "exact_content_move_candidate"
                    } else {
                        "ambiguous_exact_content_matches"
                    },
                    source_paths: sources,
                    candidate_paths: candidates,
                });
            }
        }
    }
    let mut states = Vec::new();
    for input in instructions {
        let mut reached = BTreeSet::from([input.path.clone()]);
        let mut relevant_edges = BTreeSet::new();
        let mut reached_versions = BTreeSet::new();
        for version in [Version::Before, Version::After] {
            let mut frontier = VecDeque::from([input.path.clone()]);
            let mut seen = BTreeSet::new();
            while let Some(source) = frontier.pop_front() {
                if !seen.insert(source.clone()) {
                    continue;
                }
                reached_versions.insert((source.clone(), version.clone()));
                for (index, edge) in edges
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.source_path == source && e.version == version)
                {
                    relevant_edges.insert(index);
                    for target in edge
                        .normalized_path
                        .iter()
                        .chain(edge.candidate_paths.iter())
                    {
                        reached.insert(target.clone());
                        frontier.push_back(target.clone());
                    }
                }
            }
        }
        let mut changes = Vec::new();
        let mut line_endings = Vec::new();
        for path in &reached {
            let Some(before) = view(
                path,
                &Version::Before,
                instructions,
                evidence,
                &dependencies,
            ) else {
                continue;
            };
            let Some(after) = view(path, &Version::After, instructions, evidence, &dependencies)
            else {
                continue;
            };
            if !matches!(before.status, "complete" | "absent")
                || !matches!(after.status, "complete" | "absent")
            {
                continue;
            }
            let before_hash = before.snapshot.map(|s| s.blake3.clone());
            let after_hash = after.snapshot.map(|s| s.blake3.clone());
            if before_hash != after_hash {
                let representation_only = match (before.snapshot, after.snapshot) {
                    (Some(before), Some(after)) => {
                        line_ending_equivalent(&before.content, &after.content)
                    }
                    _ => false,
                };
                let change = Change {
                    path: path.clone(),
                    status: if representation_only {
                        "line_endings_only"
                    } else {
                        match (&before_hash, &after_hash) {
                            (None, Some(_)) => "added",
                            (Some(_), None) => "deleted",
                            _ => "modified",
                        }
                    },
                    before_blake3: before_hash,
                    after_blake3: after_hash,
                };
                if representation_only {
                    line_endings.push(change);
                } else {
                    changes.push(change);
                }
            }
        }
        let unresolved: Vec<_> = relevant_edges
            .into_iter()
            .filter(|i| {
                edges[*i].status != "complete" || edges[*i].traversal == "depth_budget_omitted"
            })
            .collect();
        let coverage_issues: Vec<_> = issues
            .iter()
            .enumerate()
            .filter(|(_, i)| reached_versions.contains(&(i.source_path.clone(), i.version.clone())))
            .map(|(i, _)| i)
            .collect();
        let symbol_indices: Vec<_> = symbol_references
            .iter()
            .enumerate()
            .filter(|(_, reference)| {
                reached_versions
                    .contains(&(reference.source_path.clone(), reference.version.clone()))
            })
            .map(|(index, _)| index)
            .collect();
        let symbol_changes = symbols::changes(&symbol_references, &symbol_indices);
        let changed_symbols = symbol_changes.iter().any(|change| {
            matches!(
                change.status,
                "changed_observed_candidate_bytes"
                    | "candidate_not_located_after_no_absence_proof"
                    | "candidate_only_observed_after"
                    | "observed_candidate_binding_changed"
                    | "candidate_file_changed_body_coverage_unavailable"
            )
        });
        let incomplete = !unresolved.is_empty()
            || !coverage_issues.is_empty()
            || symbol_indices
                .iter()
                .any(|i| symbol_references[*i].status != "single_observed_declaration_candidate");
        states.push(InstructionState {
            instruction_path: input.path.clone(),
            source_blake3: input.current.blake3.clone(),
            before_blake3: input.before.as_ref().map(|s| s.blake3.clone()),
            evidence_commit: base.to_ascii_lowercase(),
            scope: scope(&input.current),
            before_scope: input.before.as_ref().map(scope),
            status: if !changes.is_empty() || changed_symbols {
                "review_changed_dependencies"
            } else if incomplete {
                "needs_reference_evidence"
            } else if !line_endings.is_empty() {
                "line_ending_differences_only"
            } else {
                "no_collected_dependency_changes"
            },
            coverage_status: if incomplete {
                "incomplete_reference_coverage"
            } else {
                "complete_for_supported_collected_references"
            },
            dependent_changes: changes,
            line_ending_differences: line_endings,
            unresolved_edge_indices: unresolved,
            coverage_issue_indices: coverage_issues,
            symbol_reference_indices: symbol_indices,
            symbol_changes,
        });
    }
    Ok(Manifest {
        dependencies: dependencies.into_values().collect(),
        edges,
        coverage_issues: issues,
        move_candidates: moves,
        symbol_references,
        symbol_inventory,
        instructions: states,
        coverage: concat!(
            "CommonMark inline/full/collapsed/shortcut links are resolved independently at exact base commit and current disk. ",
            "Definitions are document/version-local with first-definition precedence; unused definitions and fenced examples add no dependencies. ",
            "Inline-code paths retain project/document-relative candidates and disclose conflicts or unavailable alternatives. ",
            "Source use/definition ranges are end-exclusive UTF-8 bytes into their exact source hash/version. ",
            "Limits: 16 automatic documents, depth 3 from selected roots with shared nodes, 256 edges, 768KiB complete packet input and 128KiB per snapshot. ",
            "Undefined explicit labels, HTML/wiki/image references, patterns, URI encodings and heading fragments remain named gaps. ",
            "Symbol observations search only captured code: 1024 declaration candidates total, 256 per file, 256 mentions and four returned matches per mention, with observed counts/omissions. ",
            "Inline identifiers and plain text matching collected names are candidate references, not certified bindings or exhaustive prose discovery. ",
            "Unsupported/unavailable/truncated inventories leave coverage gaps even when no mention is found. Existing extractors provide positive line-range candidates only; VB uses source-only regex declaration lines without Roslyn. ",
            "No candidate or empty scan proves symbol absence, deletion, semantic identity or unchanged behavior. Compared excerpt hashes and candidate-location changes have only their literal observed meaning. ",
            "Whole-file LF/CRLF-only differences remain separate; raw hashes are exact. Move candidates cover collected exact-content paths only. ",
            "No reference establishes policy acceptance, ownership or authorization to edit."
        ),
    })
}
