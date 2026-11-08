//! Bounded exact-name navigation with selected-file terminal-name recovery.
use super::{GraphStore, ImpactAdmission, ImpactWalkCoverage, NODES, Node};
use bincode::Options;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RECORD_BYTES: usize = 64 * 1024;
const DECODE_BYTES: usize = 2 * 1024 * 1024;
const RAW_KEYS: usize = 64;
const CANDIDATES: usize = 8;
const DEADLINE_MS: u64 = 500;
// Some extractors also represent interfaces as class records. Navigate the
// recorded declaration kind without claiming language-level type binding.
const NAMED_KINDS: [&str; 5] = ["function", "method", "constructor", "property", "class"];

struct Budget {
    started: Instant,
    halted: bool,
    attempted: usize,
    decoded: usize,
    bytes: usize,
    matches: usize,
    coverage: ImpactWalkCoverage,
}

impl Budget {
    fn gap(&mut self, reason: &str) {
        self.coverage.truncated = true;
        if !self.coverage.reasons.iter().any(|item| item == reason) {
            self.coverage.reasons.push(reason.into());
        }
    }

    fn live(&mut self) -> bool {
        if self.started.elapsed() >= Duration::from_millis(DEADLINE_MS) {
            self.gap("named_lookup_deadline");
            self.halted = true;
        }
        !self.halted
    }

    fn decode(&mut self, raw: &[u8], id: &str) -> Option<Node> {
        self.attempted += 1;
        if !self.live() { return None; }
        if id.len() > 2048 || id.chars().any(char::is_control) || raw.len() > RECORD_BYTES {
            self.coverage.unknown_nodes += 1;
            self.gap("named_record_limit");
            return None;
        }
        if raw.len() > DECODE_BYTES.saturating_sub(self.bytes) {
            self.gap("named_decode_budget");
            self.halted = true;
            return None;
        }
        self.bytes += raw.len();
        let parsed = bincode::DefaultOptions::new().with_fixint_encoding()
            .with_limit(RECORD_BYTES as u64).reject_trailing_bytes().deserialize::<Node>(raw);
        match parsed {
            Ok(node) if node.node_id == id => { self.decoded += 1; Some(node) }
            _ => {
                self.coverage.unknown_nodes += 1;
                self.gap("named_record_unavailable");
                None
            }
        }
    }

    fn report(self, name: &str, retained: usize, kinds: Vec<&str>, file_admitted: bool) -> Value {
        json!({"observation":"bounded_exact_name_prefix_lookup",
            "name_match":"exact_case_sensitive_extracted_name","requested_name":name,
            "declaration_inventory":"not_scanned","canonical_file_admitted":file_admitted,
            "record_limit_bytes":RECORD_BYTES,"decode_limit_bytes":DECODE_BYTES,
            "raw_key_limit":RAW_KEYS,"candidate_limit":CANDIDATES,"deadline_ms":DEADLINE_MS,
            "deadline_scope":"Checked between synchronous reads/admission; a single operation can overrun",
            "records_attempted":self.attempted,"records_decoded":self.decoded,
            "raw_record_bytes_decoded":self.bytes,"matching_admitted_records_observed":self.matches,
            "candidate_identities_retained":retained,"kinds_started":kinds,
            "unassessed":["legacy_or_noncanonical_identities","other_node_kinds",
                "case_aliases","qualified_or_suffix_name_aliases","current_source_and_runtime_binding"],
            "declaration_admission":self.coverage})
    }
}

impl GraphStore {
    /// Exact names first, with bounded terminal-name recovery in one selected file.
    pub fn bounded_file_named_nodes(
        &self, project: &str, path: &str, name: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
    ) -> anyhow::Result<(Vec<Node>, Value)> {
        self.bounded_named_nodes(project, &[path], name, admit, false)
    }

    /// One shared work budget across one selected directory page, never a
    /// project-wide name scan. The caller owns file selection/pagination.
    pub fn bounded_files_named_nodes(
        &self, project: &str, paths: &[&str], name: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
    ) -> anyhow::Result<(Vec<Node>, Value)> {
        anyhow::ensure!(paths.len()<=8, "Directory name lookup accepts at most8 selected files");
        self.bounded_named_nodes(project, paths, name, admit, true)
    }

    fn bounded_named_nodes(
        &self, project: &str, paths: &[&str], name: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        directory: bool,
    ) -> anyhow::Result<(Vec<Node>, Value)> {
        anyhow::ensure!(!project.is_empty() && project.len() <= 1024
            && !project.chars().any(char::is_control), "Invalid named lookup project");
        anyhow::ensure!(paths.iter().all(|path|!path.is_empty() && path.len()<=2048
            && !path.chars().any(char::is_control)), "Invalid named lookup file");
        anyhow::ensure!(!name.is_empty() && name.len() <= 256
            && !name.chars().any(char::is_control), "Use exact extracted name1..256bytes without controls");
        let mut budget = Budget { started: Instant::now(), halted: false, attempted: 0,
            decoded: 0, bytes: 0, matches: 0, coverage: ImpactWalkCoverage::default() };
        let tx = self.db.begin_read()?;
        let table = tx.open_table(NODES)?;
        let mut out = Vec::new();
        let mut kinds = Vec::new();
        let mut terminal_kinds = Vec::new();
        let mut attempted_files=0;
        let mut completed_files=0;
        let mut admitted_files=0;
        let mut partial_file=None;
        for path in paths {
            if !budget.live() {break;}
            attempted_files+=1;
            let file_id = format!("file:{path}");
            let file_key = format!("{project}\0{file_id}");
            let file = match table.get(file_key.as_str())? {
                Some(raw) => budget.decode(raw.value(), &file_id),
                None => {
                    budget.attempted += 1;
                    budget.coverage.missing_nodes += 1;
                    budget.gap("named_file_missing");
                    None
                }
            };
            let Some(file) = file else {
                if budget.halted {partial_file=Some(*path);break;}
                completed_files+=1;continue;
            };
            if file.node_type != "file" || file.namespace != "memory" || file.file_path.as_str() != *path
                || admit(&file, Some(&file))? != ImpactAdmission::Selected {
                budget.coverage.unknown_nodes += 1;
                budget.gap("named_file_not_admitted");
                completed_files+=1;continue;
            }
            admitted_files+=1;
            for terminal in [false, true] {
                // Exact successes, qualified requests and directory pages keep
                // their original path. Recovery shares this transaction, file
                // admission and budget; it never starts a project-wide scan.
                if terminal && (directory || name.contains('.') || !out.is_empty() || budget.halted) {
                    break;
                }
                'kinds: for kind in NAMED_KINDS {
                    if !budget.live() { break; }
                    kinds.push(kind);
                    let prefix = if terminal {
                        terminal_kinds.push(kind);
                        format!("{project}\0sym:{kind}:{path}:")
                    } else {
                        format!("{project}\0sym:{kind}:{path}:{name}:")
                    };
                    let mut entries = table.range(prefix.as_str()..)?;
                    loop {
                        if !budget.live() { break 'kinds; }
                        if budget.coverage.raw_rows == RAW_KEYS {
                            budget.gap("named_raw_key_limit");
                            budget.halted=true;
                            break 'kinds;
                        }
                        let Some(entry) = entries.next() else { break; };
                        budget.coverage.raw_rows += 1;
                        let (key, raw) = entry?;
                        if !key.value().starts_with(&prefix) { break; }
                        let id = key.value().strip_prefix(&format!("{project}\0")).expect("project prefix");
                        let Some(mut node) = budget.decode(raw.value(), id) else { continue; };
                        if node.node_type != kind || node.file_path.as_str() != *path
                            || node.namespace != "memory" || node.name.len()>2048
                            || node.node_id != format!("sym:{kind}:{path}:{}:{}", node.name, node.start_line) {
                            budget.coverage.unknown_nodes += 1;
                            budget.gap("named_candidate_identity_mismatch");
                            continue;
                        }
                        let matches = if terminal {
                            node.name.rsplit_once('.').is_some_and(|(owner, member)|
                                !owner.is_empty() && member == name)
                        } else { node.name == name };
                        if !matches { continue; }
                        match admit(&node, Some(&file))? {
                            ImpactAdmission::Selected => {
                                budget.matches += 1;
                                budget.coverage.nodes_observed += 1;
                                if out.len() == CANDIDATES {
                                    budget.gap("named_candidate_limit");
                                    budget.halted=true;
                                    break 'kinds;
                                }
                                node.metadata = None;
                                out.push(node);
                            }
                            ImpactAdmission::NonSelectedRevision => {
                                budget.coverage.non_selected_nodes += 1;
                                budget.gap("named_non_selected_revision");
                            }
                            _ => {
                                budget.coverage.unknown_nodes += 1;
                                budget.gap("named_candidate_admission_unknown");
                            }
                        }
                    }
                }
            }
            if budget.halted {partial_file=Some(*path);break;}
            completed_files+=1;
        }
        let retained=out.len();
        let prefix_seeks=kinds.len();
        let completed=!budget.halted;
        let mut report=if directory {
            json!({"files_selected":paths.len(),"files_attempted":attempted_files,
                "files_completed":completed_files,"files_not_attempted":paths.len()-attempted_files,
                "canonical_files_admitted":admitted_files,"partial_file":partial_file,
                "matching_candidates_observed":budget.matches,"candidates_retained":retained,
                "records_attempted":budget.attempted,"records_decoded":budget.decoded,
                "decoded_bytes":budget.bytes,"raw_keys":budget.coverage.raw_rows,"prefix_seeks":kinds.len(),
                "limits":{"files":8,"record_bytes":RECORD_BYTES,"decode_bytes":DECODE_BYTES,
                    "raw_keys":RAW_KEYS,"prefix_seeks":8*NAMED_KINDS.len(),"candidates":CANDIDATES,"deadline_ms":DEADLINE_MS},
                "truncated":budget.coverage.truncated,"reasons":budget.coverage.reasons,
                "unknown":budget.coverage.unknown_nodes,"non_selected":budget.coverage.non_selected_nodes,
                "missing":budget.coverage.missing_nodes})
        }else{budget.report(name,retained,kinds,admitted_files==1)};
        if !terminal_kinds.is_empty() {
            report["observation"]=json!("bounded_exact_file_terminal_name_fallback");
            report["name_match"]=json!("exact_case_sensitive_terminal_segment");
            report["declaration_inventory"]=json!("bounded_callable_and_class_file_prefixes");
            report["terminal_name_fallback"]=json!({"attempted":true,
                "bounded_procedure_completed":completed,"kinds_started":terminal_kinds,
                "prefix_seeks":prefix_seeks,"exact_extracted_name_priority":true,
                "scope":"Only after zero admitted exact-name candidates; shared work limits over one selected file. Distinct qualified/overload identities remain candidates, not binding or uniqueness proof."});
            report["unassessed"]=json!(["legacy_or_noncanonical_identities","other_node_kinds",
                "case_aliases","other_qualified_or_nonterminal_aliases","current_source_and_runtime_binding"]);
        }
        Ok((out,report))
    }
}
