//! Bounded qualified-name source leads, separate from inferred call outcomes.
use super::{
    business_logic_service::extract_logic_methods, business_outcome_dependencies as outcomes,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::LazyLock,
};

const VERSION: &str = "vb-qualified-helper-source-leads-v1";
const INTERPRETATION: &str = "Qualified source-name inspection leads only. Namespace/receiver shadowing, imports, project root namespaces, compilation, call binding, reachability, helper completion and effects are unverified. Do not attribute a candidate body to the call or turn it into a test oracle without establishing its applicability. Immediate-return outcome dependencies retain their separate stricter resolver.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperContext {
    pub version: String,
    pub caller_file: String,
    pub caller_source_blake3: Option<String>,
    pub candidates: Vec<Candidate>,
    pub omissions: Vec<String>,
    pub interpretation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub expression: String,
    pub call_lines: Vec<u32>,
    pub identity: String,
    /// A fallback graph label is a locator, not the verified declaration owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_graph_identity: Option<String>,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub file_blake3: String,
    pub body_blake3: String,
    /// Generation gets each complete body once; stored metadata retains its
    /// identity/fingerprints, and QA rereads only matching current source.
    #[serde(skip)]
    pub body: Option<String>,
}

pub fn collect(
    file: &str,
    body: &str,
    owner: &str,
    start: u32,
    caller: Option<&Result<(String, String), String>>,
    context: Option<&outcomes::SourceContext<'_>>,
) -> Option<HelperContext> {
    static CALL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b((?:[A-Za-z_]\w*\.){2,}[A-Za-z_]\w*)\s*\(")
            .expect("qualified source-name mention")
    });
    let masked = outcomes::executable_lines(body, true);
    let mut calls: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut order = Vec::new();
    let mut omitted = 0usize;
    let mut previous = 0usize;
    let mut line = start;
    for capture in CALL.captures_iter(&masked) {
        let expression = &capture[1];
        if expression.len() > 512 {
            omitted += 1;
            continue;
        }
        if expression.to_ascii_lowercase().starts_with("global.") {
            continue;
        }
        let key = expression.to_ascii_lowercase();
        let offset = capture.get(1).unwrap().start();
        line += masked[previous..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32;
        previous = offset;
        if let Some(lines) = calls.get_mut(&key) {
            if !lines.contains(&line) {
                if lines.len() < 16 {
                    lines.push(line);
                } else {
                    omitted += 1;
                }
            }
        } else if calls.len() < 16 {
            calls.insert(key, vec![line]);
            order.push(expression.to_owned());
        } else {
            omitted += 1;
        }
    }
    if order.is_empty() {
        return None;
    }
    let mut result = HelperContext {
        version: VERSION.into(),
        caller_file: file.replace('\\', "/"),
        caller_source_blake3: None,
        candidates: vec![],
        omissions: vec![],
        interpretation: INTERPRETATION.into(),
    };
    if omitted > 0 {
        result.omissions.push(format!(
            "Qualified-name/location inventories capped at16 each; {omitted} additional mentions unexamined."
        ));
    }
    let Some(context) = context else {
        result
            .omissions
            .push("Indexed source context unavailable.".into());
        return Some(result);
    };
    let source = match caller {
        Some(Ok((source, hash)))
            if extract_logic_methods(source, "vb").iter().any(|method| {
                method.owner == owner
                    && method.start_line == start
                    && method.body.replace("\r\n", "\n") == body.replace("\r\n", "\n")
            }) =>
        {
            result.caller_source_blake3 = Some(hash.clone());
            source
        }
        _ => {
            result.omissions.push(
                "Caller fingerprint/declaration unavailable or changed; helper leads withheld."
                    .into(),
            );
            return Some(result);
        }
    };
    let _ = source;
    let mut bytes = 0usize;
    let mut supplied = BTreeSet::new();
    for (attempt, expression) in order.iter().enumerate() {
        if attempt == 4 {
            result.omissions.push(
                "Four candidate lookups reached; remaining qualified names unexamined.".into(),
            );
            break;
        }
        match outcomes::qualified_source_candidate(context, expression) {
            Ok((identity, path, first, last, text, hash, observed_graph_identity)) => {
                if !supplied.insert(identity.clone()) {
                    continue;
                }
                if bytes + text.len() > 8192 {
                    result.omissions.push(format!(
                        "{expression}: complete body exceeds remaining8192-byte helper budget."
                    ));
                    continue;
                }
                bytes += text.len();
                result.candidates.push(Candidate {
                    expression: expression.clone(),
                    call_lines: calls[&expression.to_ascii_lowercase()].clone(),
                    identity,
                    observed_graph_identity,
                    file: path,
                    start_line: first,
                    end_line: last,
                    file_blake3: hash,
                    body_blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
                    body: Some(text),
                });
            }
            Err(reason) => result.omissions.push(format!("{expression}: {reason}")),
        }
    }
    Some(result)
}

impl HelperContext {
    pub fn prompt_context(&self) -> String {
        let mut text = format!(
            "\nQualified helper source leads ({VERSION}): {}\n",
            self.interpretation
        );
        for candidate in &self.candidates {
            if let Some(observed) = &candidate.observed_graph_identity {
                text.push_str(&format!("Indexed locator label {observed}; candidate identity below is checked against the current source declaration, not inferred from this label.\n"));
            }
            if let Some(body) = &candidate.body {
                text.push_str(&format!("Inspection candidate {} for source spelling {} at caller lines {:?}; separate source {}:{}-{}; file BLAKE3 {}; body BLAKE3 {}. This is not a resolved callee:\n{}\n",
                    candidate.identity, candidate.expression, candidate.call_lines, candidate.file, candidate.start_line,
                    candidate.end_line, candidate.file_blake3, candidate.body_blake3, body));
            }
        }
        for omission in &self.omissions {
            text.push_str(&format!("Helper lead gap: {omission}\n"));
        }
        text
    }

    pub fn qa_evidence(&self, root: &Path, caller_hash: Option<&str>, audit: &mut Audit) -> String {
        let mut text = format!("Helper source leads: {}\n", self.interpretation);
        if self.version != VERSION
            || self
                .caller_source_blake3
                .as_deref()
                .zip(caller_hash)
                .is_none_or(|(recorded, current)| recorded != current)
        {
            text.push_str("STALE_OR_UNAVAILABLE_CALLER_SUPPORTING_SOURCE: helper excerpts withheld; refresh this analysis.\n");
            return text;
        }
        let mut shown = 0;
        for candidate in self.candidates.iter().take(4) {
            let current = audit.body(root, candidate);
            match current {
                Ok(body) => {
                    let entry = format!(
                        "- Source-name candidate {} from {}:{}-{}; file BLAKE3 {}; body BLAKE3 {}; caller lines {:?}:\n{}\n",
                        candidate.identity,
                        candidate.file,
                        candidate.start_line,
                        candidate.end_line,
                        candidate.file_blake3,
                        candidate.body_blake3,
                        candidate.call_lines,
                        body
                    );
                    if text.len() + entry.len() + 256 > 4096 {
                        continue;
                    }
                    text.push_str(&entry);
                    shown += 1;
                }
                Err(reason) => text.push_str(&format!(
                    "STALE_OR_UNAVAILABLE_HELPER_SOURCE: {}:{}; excerpt withheld ({reason}).\n",
                    candidate.file, candidate.start_line
                )),
            }
        }
        text.push_str(&format!("Helper excerpts shown {shown}/{}; remaining source and gaps require review. These are supporting inspection leads, not inferred rules or verified test outcomes.\n",self.candidates.len()));
        text
    }
}

pub struct Audit {
    sources: BTreeMap<String, Result<String, String>>,
    bytes: usize,
}
impl Default for Audit {
    fn default() -> Self {
        Self {
            sources: BTreeMap::new(),
            bytes: 0,
        }
    }
}
impl Audit {
    fn body(&mut self, root: &Path, candidate: &Candidate) -> Result<String, String> {
        if !self.sources.contains_key(&candidate.file) {
            if self.sources.len() >= 8 {
                return Err("eight-file matrix helper read limit".into());
            }
            let limit = (2 * 1024 * 1024usize).saturating_sub(self.bytes);
            let source = outcomes::read_bounded_file_limit(root, &candidate.file, limit as u64)
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| "helper source is not UTF-8".into())
                });
            if let Ok(source) = &source {
                self.bytes += source.len();
            }
            self.sources.insert(candidate.file.clone(), source);
        }
        let source = self.sources[&candidate.file]
            .as_ref()
            .map_err(Clone::clone)?;
        if blake3::hash(source.as_bytes()).to_hex().as_str() != candidate.file_blake3 {
            return Err("file fingerprint changed".into());
        }
        let mut methods = extract_logic_methods(source, "vb")
            .into_iter()
            .filter(|method| {
                format!("{}.{}", method.owner, method.name)
                    .eq_ignore_ascii_case(&candidate.identity)
                    && method.start_line == candidate.start_line
                    && method.start_line + method.body.lines().count().saturating_sub(1) as u32
                        == candidate.end_line
                    && blake3::hash(method.body.as_bytes()).to_hex().as_str()
                        == candidate.body_blake3
            });
        let method = methods
            .next()
            .ok_or("recorded helper body/span unavailable")?;
        if methods.next().is_some() {
            return Err("helper identity/body ambiguous".into());
        }
        Ok(method.body)
    }
}
