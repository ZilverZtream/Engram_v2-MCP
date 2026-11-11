//! Positive source observations, never semantic binding or proof of absence.
//! No index, filesystem, subprocess, Roslyn project or live HEAD is consulted.
use super::*;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const CANDIDATE_LIMIT: usize = 1024;
const REFERENCE_LIMIT: usize = 256;
const MATCH_LIMIT: usize = 4;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub source_blake3: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub byte_range: [usize; 2],
    pub start_line: u32,
    pub end_line: u32,
    pub evidence_blake3: String,
    pub span_kind: &'static str,
    pub absence_proof: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Inventory {
    pub path: String,
    pub version: Version,
    pub source_blake3: String,
    pub status: &'static str,
    pub retained_candidates: usize,
    pub omitted_candidates: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reference {
    pub source_path: String,
    pub source_blake3: String,
    pub version: Version,
    pub byte_range: [usize; 2],
    pub literal: String,
    pub form: &'static str,
    pub status: &'static str,
    pub candidates: Vec<Candidate>,
    pub observed_candidate_count: usize,
    pub omitted_candidates: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Change {
    pub literal: String,
    pub status: &'static str,
    pub before_candidates: Vec<Candidate>,
    pub after_candidates: Vec<Candidate>,
}

struct Record {
    version: Version,
    candidate: Candidate,
    vb: bool,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.replace("::", ".").split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '$'))
                && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
        })
}

fn names(candidate: &Candidate) -> Vec<String> {
    let mut result = BTreeSet::new();
    for name in [&candidate.name, &candidate.qualified_name] {
        result.insert(name.replace("::", "."));
        if let Some(short) = name.rsplit(['.', ':']).next() {
            result.insert(short.to_owned());
        }
    }
    result.into_iter().filter(|n| identifier(n)).collect()
}

fn line_range(content: &str, first: u32, last: u32) -> Option<[usize; 2]> {
    if first == 0 || last < first {
        return None;
    }
    let mut offset = 0;
    let mut start = None;
    let mut end = None;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if number == first {
            start = Some(offset);
        }
        offset += line.len();
        if number == last {
            end = Some(offset);
            break;
        }
    }
    Some([start?, end?])
}

pub(super) fn capture(
    instructions: &[Instruction],
    evidence: &[Evidence],
    dependencies: &BTreeMap<String, Dependency>,
    issues: &mut Vec<CoverageIssue>,
) -> (Vec<Reference>, Vec<Inventory>) {
    let paths: BTreeSet<_> = evidence
        .iter()
        .map(|e| e.path.clone())
        .chain(dependencies.keys().cloned())
        .collect();
    let mut records = Vec::new();
    let mut inventory = Vec::new();
    for path in paths.iter().filter(|p| !guidance(p)) {
        for version in [Version::Before, Version::After] {
            let Some(snapshot) =
                view(path, &version, instructions, evidence, dependencies).and_then(|v| v.snapshot)
            else {
                continue;
            };
            let extension = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let vb = extension == "vb";
            let supported = vb
                || matches!(
                    extension.as_str(),
                    "rs" | "py"
                        | "go"
                        | "java"
                        | "ts"
                        | "tsx"
                        | "js"
                        | "jsx"
                        | "cs"
                        | "c"
                        | "h"
                        | "cpp"
                        | "hpp"
                        | "cc"
                        | "cxx"
                        | "hh"
                );
            let extracted = if supported {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if vb {
                        engram_index::vb_extractor::snapshot_declaration_candidates(
                            &snapshot.content,
                        )
                    } else {
                        engram_index::parsing::SymbolExtractor::new()
                            .extract(Path::new(path), &snapshot.content)
                            .0
                    }
                }))
                .ok()
            } else {
                None
            };
            let mut status = if !supported {
                "unsupported_symbol_extractor"
            } else if extracted.is_none() {
                "symbol_extractor_unavailable"
            } else if vb {
                "regex_declaration_line_candidates_no_absence_proof"
            } else {
                "tree_sitter_line_range_candidates_no_absence_proof"
            };
            let mut retained = 0;
            let mut omitted = 0;
            for symbol in extracted.unwrap_or_default() {
                let qualified = symbol
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .unwrap_or(&symbol.name);
                if records.len() >= CANDIDATE_LIMIT
                    || retained >= 256
                    || qualified.len() > 512
                    || symbol.name.len() > 512
                {
                    omitted += 1;
                    continue;
                }
                let Some(range) = line_range(&snapshot.content, symbol.start_line, symbol.end_line)
                else {
                    omitted += 1;
                    continue;
                };
                let candidate = Candidate {
                    path: path.clone(),
                    source_blake3: snapshot.blake3.clone(),
                    name: symbol.name.clone(),
                    qualified_name: qualified.clone(),
                    kind: symbol.kind,
                    byte_range: range,
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    evidence_blake3: blake3::hash(snapshot.content[range[0]..range[1]].as_bytes())
                        .to_hex()
                        .to_string(),
                    span_kind: if vb {
                        "regex_declaration_line_only"
                    } else {
                        "extracted_declaration_line_range"
                    },
                    absence_proof: false,
                };
                if names(&candidate).is_empty() {
                    continue;
                }
                records.push(Record {
                    version: version.clone(),
                    candidate,
                    vb,
                });
                retained += 1;
            }
            if omitted > 0 {
                status = "symbol_candidate_budget_or_range_omitted";
            }
            inventory.push(Inventory {
                path: path.clone(),
                version,
                source_blake3: snapshot.blake3.clone(),
                status,
                retained_candidates: retained,
                omitted_candidates: omitted,
            });
        }
    }
    let mut lookup: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut vb_lookup: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        for name in names(&record.candidate) {
            lookup.entry(name.clone()).or_default().push(index);
            if record.vb {
                vb_lookup
                    .entry(name.to_ascii_lowercase())
                    .or_default()
                    .push(index);
            }
        }
    }
    let sources: BTreeSet<_> = instructions
        .iter()
        .map(|i| i.path.clone())
        .chain(dependencies.keys().filter(|p| guidance(p)).cloned())
        .chain(
            evidence
                .iter()
                .filter(|e| guidance(&e.path))
                .map(|e| e.path.clone()),
        )
        .collect();
    let mut references = Vec::new();
    for source in sources {
        for version in [Version::Before, Version::After] {
            let Some(snapshot) = view(&source, &version, instructions, evidence, dependencies)
                .and_then(|v| v.snapshot)
            else {
                continue;
            };
            // Candidate limits/unsupported inputs can hide plain-text mentions
            // as well as additional matches. Zero discovered mentions is not a
            // complete result when its name-discovery inventory is incomplete.
            let inventory_gaps = inventory
                .iter()
                .filter(|row| {
                    row.version == version
                        && (row.omitted_candidates > 0
                            || matches!(
                                row.status,
                                "unsupported_symbol_extractor" | "symbol_extractor_unavailable"
                            ))
                })
                .count();
            if inventory_gaps > 0 {
                issues.push(CoverageIssue {
                    source_path: source.clone(),
                    version: version.clone(),
                    byte_offset: 0,
                    status: "symbol_inventory_incomplete_for_plain_text_discovery",
                    omitted_occurrences: inventory_gaps,
                });
            }
            let mut code_block = false;
            let mut image_depth = 0usize;
            let mut omitted = None;
            for (event, span) in
                Parser::new_ext(&snapshot.content, Options::ENABLE_WIKILINKS).into_offset_iter()
            {
                match &event {
                    Event::Start(Tag::CodeBlock(_)) => {
                        code_block = true;
                        continue;
                    }
                    Event::End(TagEnd::CodeBlock) => {
                        code_block = false;
                        continue;
                    }
                    Event::Start(Tag::Image { .. }) => {
                        image_depth += 1;
                        continue;
                    }
                    Event::End(TagEnd::Image) => {
                        image_depth = image_depth.saturating_sub(1);
                        continue;
                    }
                    _ if code_block || image_depth > 0 => continue,
                    _ => {}
                }
                let mut mentions = Vec::new();
                match event {
                    Event::Code(value)
                        if !markdown_links::path_candidate(&value) && identifier(&value) =>
                    {
                        mentions.push((span.start, span.end, value.into_string(), "inline_code"));
                    }
                    Event::Text(_) => {
                        let raw = &snapshot.content[span.clone()];
                        let mut start = None;
                        for (at, character) in
                            raw.char_indices().chain(std::iter::once((raw.len(), ' ')))
                        {
                            if character.is_alphanumeric()
                                || matches!(character, '_' | '$' | '.' | ':')
                            {
                                start.get_or_insert(at);
                            } else if let Some(first) = start.take() {
                                let token = raw[first..at].trim_end_matches(['.', ':']);
                                // Prose is not VB source: folding "resolved
                                // style" into the type Resolved invents a
                                // dependency. Explicit code retains VB lookup.
                                if identifier(token)
                                    && lookup.contains_key(&token.replace("::", "."))
                                {
                                    mentions.push((
                                        span.start + first,
                                        span.start + first + token.len(),
                                        token.to_owned(),
                                        "plain_text_matching_collected_candidate",
                                    ));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                for (start, end, literal, form) in mentions {
                    if references.len() >= REFERENCE_LIMIT {
                        let row = omitted.get_or_insert((start, 0usize));
                        row.1 += 1;
                        continue;
                    }
                    let canonical = literal.replace("::", ".");
                    let mut matching: BTreeSet<_> = lookup
                        .get(&canonical)
                        .into_iter()
                        .flatten()
                        .copied()
                        .chain(
                            vb_lookup
                                .get(&canonical.to_ascii_lowercase())
                                .filter(|_| form == "inline_code")
                                .into_iter()
                                .flatten()
                                .copied(),
                        )
                        .filter(|index| records[*index].version == version)
                        .collect();
                    let mut qualified_suffix = false;
                    if matching.is_empty() && canonical.contains('.') {
                        let short = canonical.rsplit('.').next().unwrap();
                        matching = lookup
                            .get(short)
                            .into_iter()
                            .flatten()
                            .copied()
                            .chain(
                                vb_lookup
                                    .get(&short.to_ascii_lowercase())
                                    .filter(|_| form == "inline_code")
                                    .into_iter()
                                    .flatten()
                                    .copied(),
                            )
                            .filter(|index| records[*index].version == version)
                            .collect();
                        qualified_suffix = !matching.is_empty();
                    }
                    let count = matching.len();
                    let candidates: Vec<_> = matching
                        .iter()
                        .take(MATCH_LIMIT)
                        .map(|i| records[*i].candidate.clone())
                        .collect();
                    let status = if count == 0 {
                        "unresolved_symbol_in_collected_evidence"
                    } else if count > 1 {
                        "ambiguous_symbol_candidates"
                    } else if qualified_suffix {
                        "qualification_not_verified"
                    } else if candidates[0].span_kind == "regex_declaration_line_only" {
                        "unverified_vb_declaration_candidate"
                    } else {
                        "single_observed_declaration_candidate"
                    };
                    references.push(Reference {
                        source_path: source.clone(),
                        source_blake3: snapshot.blake3.clone(),
                        version: version.clone(),
                        byte_range: [start, end],
                        literal,
                        form,
                        status,
                        omitted_candidates: count.saturating_sub(candidates.len()),
                        observed_candidate_count: count,
                        candidates,
                    });
                }
            }
            if let Some((offset, count)) = omitted {
                issues.push(CoverageIssue {
                    source_path: source.clone(),
                    version,
                    byte_offset: offset,
                    status: "symbol_reference_budget_omitted",
                    omitted_occurrences: count,
                });
            }
        }
    }
    (references, inventory)
}

pub(super) fn changes(references: &[Reference], indices: &[usize]) -> Vec<Change> {
    let mut pairs: BTreeMap<String, (Vec<Candidate>, Vec<Candidate>)> = BTreeMap::new();
    for index in indices {
        let reference = &references[*index];
        let pair = pairs.entry(reference.literal.clone()).or_default();
        let selected = if reference.version == Version::Before {
            &mut pair.0
        } else {
            &mut pair.1
        };
        for candidate in &reference.candidates {
            if !selected.contains(candidate) {
                selected.push(candidate.clone());
            }
        }
    }
    pairs
        .into_iter()
        .map(|(literal, (before, after))| {
            let status = match (before.as_slice(), after.as_slice()) {
                ([old], [new])
                    if old.path == new.path && old.qualified_name == new.qualified_name =>
                {
                    if old.span_kind == "regex_declaration_line_only"
                        || new.span_kind == "regex_declaration_line_only"
                    {
                        if old.source_blake3 != new.source_blake3 {
                            "candidate_file_changed_body_coverage_unavailable"
                        } else {
                            "body_coverage_unavailable_for_vb_candidates"
                        }
                    } else if old.evidence_blake3 == new.evidence_blake3 {
                        "same_observed_candidate_bytes"
                    } else {
                        "changed_observed_candidate_bytes"
                    }
                }
                ([..], []) if !before.is_empty() => "candidate_not_located_after_no_absence_proof",
                ([], [..]) if !after.is_empty() => "candidate_only_observed_after",
                ([], []) => "unresolved_in_both_collected_versions",
                ([_], [_]) => "observed_candidate_binding_changed",
                _ => "ambiguous_candidate_comparison",
            };
            Change {
                literal,
                status,
                before_candidates: before,
                after_candidates: after,
            }
        })
        .collect()
}
