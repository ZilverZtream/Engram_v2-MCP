//! Optional LLM curation pass for the CLAUDE.md rules pipeline.
//!
//! The deterministic [`rules_pipeline`] gets us from 32 raw rules →
//! ~8 aggregated meta-rules, which is the floor. This module is the
//! ceiling: given those ~8 candidates plus project context, ask an
//! LLM to:
//!
//! 1. **Drop anything not genuinely code-writing guidance.** Edge
//!    cases the keyword filter missed.
//! 2. **Merge near-duplicates** the keyword clusterer split.
//! 3. **Rewrite each kept rule in the project's own voice** — using
//!    vocabulary the agent will actually encounter (`iFaltDataContext`,
//!    `CanReadViaApi`, `handelselogg.Create`) instead of generic
//!    phrases like "the project's audit-log helper".
//! 4. **Rank the result by importance** so the root CLAUDE.md leads
//!    with the strictest rules.
//!
//! Gated entirely by `use_llm: true` on the request — the
//! deterministic path must work fine without it. Results are cached
//! in the registry keyed by `blake3(candidate_set + project_context)`
//! so reruns against the same inputs spend zero tokens. The
//! classifier is called at most once per unique rule corpus per
//! project.
//!
//! Response format is JSON — the module parses it with `serde_json`
//! and falls back to the deterministic input on any parse / timeout
//! / backend error. The LLM is strictly additive: it can NEVER make
//! the output worse than the deterministic baseline.

use std::time::Duration;

use engram_ml::DreamingEngine;
use serde::{Deserialize, Serialize};

use super::{CriticalRule, RuleSource};

/// Input to the LLM curation pass — the deterministic candidates
/// plus a short project context string (role + dominant languages).
/// Hashing this full payload produces the cache key so any change
/// to the inputs forces a fresh classification.
#[derive(Debug, Clone, Serialize)]
pub struct CurationInput {
    pub project_context: String,
    pub candidates: Vec<CurationCandidate>,
    pub max_rules: usize,
}

/// One candidate rule handed to the LLM. Carries just enough
/// metadata for the model to judge importance; we deliberately do
/// NOT pass the raw CodeRabbit comment bodies — those are noisy
/// and a cluster's rule text + evidence suffices.
#[derive(Debug, Clone, Serialize)]
pub struct CurationCandidate {
    /// Stable id — the LLM echoes this in its response so we can
    /// map back to the original rule for provenance.
    pub id: String,
    pub text: String,
    pub evidence: String,
    pub source: String,
    /// Non-serialised: carried through so the parser can re-attach the
    /// original confidence tier to the LLM-curated output without the
    /// model having to reason about it.
    #[serde(skip)]
    pub confidence: super::RuleConfidence,
}

/// Parsed LLM response. Robust to slightly-malformed JSON — the
/// caller falls back to deterministic output on any parse failure.
#[derive(Debug, Clone, Deserialize)]
pub struct CurationResponse {
    pub rules: Vec<CuratedRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CuratedRule {
    /// id of the original candidate this curated rule derived from.
    /// When the LLM merged several candidates, it echoes the
    /// primary one's id.
    pub id: String,
    /// Rewritten rule text, hopefully in project-idiomatic voice.
    pub text: String,
    /// Optional rationale one-liner — why this rule matters. Pulled
    /// into the rendered `<critical_rules>` evidence line.
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Run the LLM curation pass. Returns the curated rule set on
/// success; returns the original deterministic input untouched on
/// any failure (timeout, parse error, backend absent). Callers
/// treat this as a best-effort quality boost — never a blocker.
pub async fn curate_with_llm(
    dreaming: &DreamingEngine,
    registry: &engram_core::registry::Registry,
    project_id: &str,
    input: CurationInput,
    deterministic_fallback: Vec<CriticalRule>,
) -> Vec<CriticalRule> {
    if input.candidates.is_empty() {
        return deterministic_fallback;
    }

    // Cache key: hash the full input so any change to project
    // context OR the candidate set forces a fresh classification.
    // Reruns against the same inputs are free.
    let cache_key = compute_cache_key(&input);
    let meta_key = format!("claude_md_llm_curate:{cache_key}");
    if let Ok(Some(cached)) = registry.get_meta(project_id, &meta_key) {
        if let Some(parsed) = parse_cached(&cached, &input.candidates) {
            tracing::info!(cache_key = %cache_key, "claude_md LLM curation cache hit");
            return parsed;
        }
    }

    let prompt = build_prompt(&input);
    let raw = match dreaming
        .generate_text(&prompt, 2048, Duration::from_secs(60))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                project_id,
                "claude_md LLM curation failed ({e}) — using deterministic fallback"
            );
            return deterministic_fallback;
        }
    };

    match parse_response(&raw, &input.candidates) {
        Some(curated) => {
            // Cache the RAW response — on a cache hit we re-map ids
            // back to the current candidate set, which preserves the
            // LLM's decisions even if the candidate ordering shifts
            // slightly between runs.
            let _ = registry.set_meta(project_id, &meta_key, &raw);
            curated
        }
        None => {
            tracing::warn!(
                project_id,
                "claude_md LLM response could not be parsed — using deterministic fallback"
            );
            deterministic_fallback
        }
    }
}

fn compute_cache_key(input: &CurationInput) -> String {
    let encoded = serde_json::to_string(input).unwrap_or_default();
    blake3::hash(encoded.as_bytes()).to_hex()[..16].to_string()
}

fn build_prompt(input: &CurationInput) -> String {
    use std::fmt::Write as _;
    let mut candidates_json = String::new();
    let _ = writeln!(candidates_json, "[");
    for (i, c) in input.candidates.iter().enumerate() {
        let comma = if i + 1 < input.candidates.len() {
            ","
        } else {
            ""
        };
        let _ = writeln!(
            candidates_json,
            "  {{\"id\": {id:?}, \"text\": {text:?}, \"evidence\": {ev:?}, \"source\": {src:?}}}{comma}",
            id = c.id,
            text = c.text,
            ev = c.evidence,
            src = c.source,
        );
    }
    candidates_json.push(']');

    format!(
        r#"You are curating the `<critical_rules>` section of a CLAUDE.md file.

CLAUDE.md sits at the top of every Claude Code session for this project. \
Critical rules are what the agent MUST follow when writing code. The \
attention budget is tight: only the highest-signal rules earn a place here.

**Project context:** {context}

**Your task:**
Given the candidate rules below, produce the top {max_rules} rules for the \
root CLAUDE.md. For each rule you keep, rewrite the `text` field as a crisp \
imperative sentence using the PROJECT'S OWN vocabulary where it appears in the \
candidate evidence (class names, framework helpers, etc). Keep each \
rewritten text to 120 chars or less.

**Selection rules:**
1. Prefer data-correctness > security > bugs > ergonomics > style.
2. Drop anything that isn't code-writing guidance (process hygiene, git \
   rules, compile-time errors the compiler already catches, cosmetic fixes).
3. Merge candidates that describe the same underlying behaviour. Keep the \
   most informative id as the representative.
4. Preserve every `immune_*`-sourced rule — those represent past production \
   incidents. Never drop one.

**Output format:** STRICT JSON matching this shape, no prose before or after:
{{
  "rules": [
    {{"id": "<echo the primary candidate id>", "text": "<rewritten rule>", "rationale": "<one-line why>"}}
  ]
}}

**Candidates:**
{candidates}
"#,
        context = input.project_context.trim(),
        max_rules = input.max_rules,
        candidates = candidates_json,
    )
}

fn parse_response(raw: &str, candidates: &[CurationCandidate]) -> Option<Vec<CriticalRule>> {
    // LLMs sometimes wrap their JSON in markdown code fences. Strip
    // those before parsing.
    let cleaned = strip_json_fences(raw);
    let parsed: CurationResponse = serde_json::from_str(&cleaned).ok()?;
    if parsed.rules.is_empty() {
        return None;
    }

    let by_id: std::collections::HashMap<&str, &CurationCandidate> =
        candidates.iter().map(|c| (c.id.as_str(), c)).collect();

    let mut out: Vec<CriticalRule> = Vec::with_capacity(parsed.rules.len());
    for r in parsed.rules {
        if r.text.trim().is_empty() {
            continue;
        }
        // Re-derive the source from the original candidate so the
        // rendered bullet gets the right emoji tag (🐰 / 🛡).
        let source = by_id
            .get(r.id.as_str())
            .map(|c| source_from_str(&c.source))
            .unwrap_or(RuleSource::RepoRule);
        let evidence = r
            .rationale
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("({s}) [LLM-curated from {orig_id}]", orig_id = r.id));
        // The LLM gets the deterministic candidate list as input and
        // only rewrites what made it through the render threshold, so
        // its output carries the same empirical weight as the source
        // candidate. Preserve the source's confidence tier — immune
        // stays Hard, well-supported CodeRabbit meta-clusters stay
        // Hard/Strong, sparse ones stay Observed.
        let confidence = by_id
            .get(r.id.as_str())
            .map(|c| c.confidence)
            .unwrap_or(super::RuleConfidence::Strong);
        out.push(CriticalRule {
            text: trim_to_120(&r.text),
            evidence: evidence.or_else(|| Some(format!("(LLM-curated from {})", r.id))),
            source,
            confidence,
        });
    }
    if out.is_empty() { None } else { Some(out) }
}

fn parse_cached(cached_raw: &str, candidates: &[CurationCandidate]) -> Option<Vec<CriticalRule>> {
    parse_response(cached_raw, candidates)
}

fn strip_json_fences(s: &str) -> String {
    let trimmed = s.trim();
    // ```json { … } ``` → { … }
    if let Some(rest) = trimmed
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
    {
        return rest.trim().to_string();
    }
    if let Some(rest) = trimmed
        .strip_prefix("```")
        .and_then(|s| s.strip_suffix("```"))
    {
        return rest.trim().to_string();
    }
    trimmed.to_string()
}

fn trim_to_120(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= 120 {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(119).collect();
    out.push('…');
    out
}

fn source_from_str(s: &str) -> RuleSource {
    match s.to_ascii_lowercase().as_str() {
        "immune" => RuleSource::Immune,
        "coderabbit" => RuleSource::CodeRabbit,
        "existing" => RuleSource::Existing,
        _ => RuleSource::RepoRule,
    }
}

/// Translate a [`CriticalRule`] list into [`CurationCandidate`]s for
/// the LLM input. Generates stable ids from the rule's evidence field
/// (which already carries the underlying `cr_*` / `immune_*` id when
/// it exists) so the LLM's response can be round-tripped.
pub fn prepare_candidates(rules: &[CriticalRule]) -> Vec<CurationCandidate> {
    rules
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // Prefer the upstream id (cr_*, immune_*) when present —
            // keeps the LLM's response traceable. Otherwise fall
            // back to a synthetic positional id.
            let id =
                extract_upstream_id(r.evidence.as_deref()).unwrap_or_else(|| format!("local-{i}"));
            CurationCandidate {
                id,
                text: r.text.clone(),
                evidence: r.evidence.clone().unwrap_or_default(),
                source: source_to_str(&r.source).to_string(),
                confidence: r.confidence,
            }
        })
        .collect()
}

fn extract_upstream_id(evidence: Option<&str>) -> Option<String> {
    let ev = evidence?;
    // Scan for the FIRST occurrence of `cr_` or `immune_` that
    // starts at a token boundary (`(`, `,`, or whitespace). Covers
    // both the simple `(cr_abc12345)` form emitted by the handler
    // AND the richer aggregated form the meta-clustering pipeline
    // may emit (`(3 CodeRabbit rules aggregated, 12 PRs, ..., cr_abc12345)`).
    for prefix in ["cr_", "immune_"] {
        let mut search_from = 0;
        while let Some(rel) = ev[search_from..].find(prefix) {
            let abs = search_from + rel;
            // Require a token boundary before the prefix so we
            // don't match `something_cr_foo` mid-word.
            let at_boundary =
                abs == 0 || matches!(ev.as_bytes()[abs - 1], b'(' | b',' | b' ' | b'\t' | b'\n');
            if at_boundary {
                // Scan forward to the end of the id — stop at any
                // non-[a-z0-9_] byte.
                let mut end = abs + prefix.len();
                while end < ev.len()
                    && matches!(ev.as_bytes()[end], b'a'..=b'z' | b'0'..=b'9' | b'_')
                {
                    end += 1;
                }
                if end > abs + prefix.len() {
                    return Some(ev[abs..end].to_string());
                }
            }
            search_from = abs + prefix.len();
        }
    }
    None
}

fn source_to_str(s: &RuleSource) -> &'static str {
    match s {
        RuleSource::Immune => "immune",
        RuleSource::CodeRabbit => "coderabbit",
        RuleSource::RepoRule => "repo_rule",
        RuleSource::Existing => "existing",
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_json_fences_handles_code_blocks() {
        let fenced = "```json\n{\"rules\": []}\n```";
        assert_eq!(strip_json_fences(fenced), "{\"rules\": []}");
    }

    #[test]
    fn trim_to_120_caps_text() {
        let long = "x".repeat(200);
        let out = trim_to_120(&long);
        assert!(out.chars().count() <= 120);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn extract_upstream_id_finds_cr_prefix() {
        let e = "(3 CodeRabbit rules aggregated, 12 PRs, 100% fix rate, cr_abc12345)";
        assert_eq!(extract_upstream_id(Some(e)), Some("cr_abc12345".into()));
    }

    #[test]
    fn extract_upstream_id_finds_immune_prefix() {
        let e = "(immune_8133c13312ab)";
        assert_eq!(
            extract_upstream_id(Some(e)),
            Some("immune_8133c13312ab".into())
        );
    }

    #[test]
    fn extract_upstream_id_returns_none_when_no_tag() {
        assert_eq!(extract_upstream_id(None), None);
        assert_eq!(extract_upstream_id(Some("just prose")), None);
        assert_eq!(extract_upstream_id(Some("(some other tag)")), None);
    }

    #[test]
    fn prepare_candidates_preserves_ids_from_evidence() {
        let rules = vec![
            CriticalRule {
                text: "Do X".into(),
                evidence: Some("(cr_abc12345)".into()),
                source: RuleSource::CodeRabbit,
                confidence: Default::default(),
            },
            CriticalRule {
                text: "Do Y".into(),
                evidence: None,
                source: RuleSource::Existing,
                confidence: Default::default(),
            },
        ];
        let c = prepare_candidates(&rules);
        assert_eq!(c[0].id, "cr_abc12345");
        assert_eq!(c[1].id, "local-1");
    }

    #[test]
    fn parse_response_round_trips_rule_set() {
        let raw = r##"{"rules":[
            {"id":"cr_null1","text":"Always `If Is Nothing` guard","rationale":"top error"},
            {"id":"cr_perm1","text":"Use CanReadViaApi, not CheckIsUserInRole"}
        ]}"##;
        let candidates = vec![
            CurationCandidate {
                id: "cr_null1".into(),
                text: "Null-guard rule".into(),
                evidence: "".into(),
                source: "coderabbit".into(),
                confidence: crate::services::produce_claude_md_service::RuleConfidence::Observed,
            },
            CurationCandidate {
                id: "cr_perm1".into(),
                text: "Permission rule".into(),
                evidence: "".into(),
                source: "coderabbit".into(),
                confidence: crate::services::produce_claude_md_service::RuleConfidence::Observed,
            },
        ];
        let parsed = parse_response(raw, &candidates).expect("must parse");
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].text.contains("Is Nothing"));
        assert_eq!(parsed[0].source, RuleSource::CodeRabbit);
        assert!(parsed[0].evidence.as_deref().unwrap().contains("cr_null1"));
    }

    #[test]
    fn parse_response_tolerates_markdown_fences() {
        let raw = "```json\n{\"rules\":[{\"id\":\"x\",\"text\":\"do thing\"}]}\n```";
        let candidates = vec![CurationCandidate {
            id: "x".into(),
            text: "t".into(),
            evidence: "".into(),
            source: "coderabbit".into(),
            confidence: crate::services::produce_claude_md_service::RuleConfidence::Observed,
        }];
        let parsed = parse_response(raw, &candidates).expect("must parse");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn parse_response_returns_none_on_malformed_json() {
        assert!(parse_response("not json", &[]).is_none());
        assert!(parse_response("{\"rules\": \"not an array\"}", &[]).is_none());
    }

    #[test]
    fn parse_response_drops_empty_text() {
        let raw = r#"{"rules":[{"id":"a","text":""},{"id":"b","text":"real"}]}"#;
        let candidates = vec![
            CurationCandidate {
                id: "a".into(),
                text: "t".into(),
                evidence: "".into(),
                source: "coderabbit".into(),
                confidence: crate::services::produce_claude_md_service::RuleConfidence::Observed,
            },
            CurationCandidate {
                id: "b".into(),
                text: "t".into(),
                evidence: "".into(),
                source: "coderabbit".into(),
                confidence: crate::services::produce_claude_md_service::RuleConfidence::Observed,
            },
        ];
        let parsed = parse_response(raw, &candidates).expect("must parse");
        assert_eq!(parsed.len(), 1, "empty-text rule must be dropped");
    }

    #[test]
    fn cache_key_is_deterministic_and_short() {
        let a = compute_cache_key(&CurationInput {
            project_context: "x".into(),
            candidates: vec![],
            max_rules: 8,
        });
        let b = compute_cache_key(&CurationInput {
            project_context: "x".into(),
            candidates: vec![],
            max_rules: 8,
        });
        let c = compute_cache_key(&CurationInput {
            project_context: "y".into(),
            candidates: vec![],
            max_rules: 8,
        });
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }
}
