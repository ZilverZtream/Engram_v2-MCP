//! Source-bound enclosing-If spelling checks, independent of return-path availability.
//! Literal spelling absence is a scope-review lead, never semantic equivalence or polarity proof.
use super::business_outcome_dependencies::OutcomeEvidence;
use serde::Deserialize;

const VERSION: &str = "vb-branch-contexts-v1";
const LIMIT: usize = 256;
const QUALIFIER: &str = "Lexical spelling only; polarity, model alignment, semantic equivalence, branch feasibility and completeness of rule conditions are not assessed. Only structured WHEN with a unique supported anchor is checked; plain, missing or malformed WHEN and other fields remain unassessed.";

#[derive(Clone, Debug, Deserialize)]
struct Condition {
    expression: String,
    line: u32,
    end_line: u32,
    outcome: String,
}
#[derive(Clone, Debug, Deserialize)]
struct Entry {
    statement_kind: String,
    start_line: u32,
    end_line: u32,
    conditions: Vec<Condition>,
}
#[derive(Deserialize)]
struct Inventory {
    version: String,
    status: String,
    source_sha256: String,
    method_start_line: u32,
    method_end_line: u32,
    entries: Vec<Entry>,
    unavailable_lines: Vec<u32>,
}

/// No entry (including a legacy/invalid inventory) establishes absence of enclosing guards.
pub struct Checklist {
    entries: Vec<Entry>,
    unavailable_lines: Vec<u32>,
    query_predicates: Vec<(u32, String)>,
    reason: String,
    available: bool,
}
impl Checklist {
    fn unknown(reason: &str) -> Self {
        Self { entries: vec![], unavailable_lines: vec![], query_predicates: vec![], reason: reason.into(), available: false }
    }
    pub fn coverage(&self) -> String {
        format!("Branch scope coverage: {}; {}", self.reason, QUALIFIER)
    }
    pub fn warnings(&self, ordinal: usize, when: &str, anchor: u32) -> Vec<String> {
        if !self.available || self.unavailable_lines.contains(&anchor) { return vec![]; }
        let matching: Vec<_> = self.entries.iter().filter(|e| e.start_line <= anchor && anchor <= e.end_line).collect();
        if matching.len() != 1 { return vec![]; }
        let Some(when) = tokens(when) else { return vec![]; };
        if when.is_empty() { return vec![]; }
        let mut warnings: Vec<_> = matching[0].conditions.iter().filter_map(|guard| {
            let expression = tokens(&guard.expression)?;
            if expression.is_empty() || when.windows(expression.len()).enumerate().any(|(i,w)| {
                w == expression.as_slice() && !matches!(i.checked_sub(1).and_then(|n|when.get(n)), Some(Token::Punctuation('.'|'!')))
                    && !matches!(when.get(i+expression.len()), Some(Token::Punctuation('.'|'!')))
            }) { return None; }
            Some(format!("Rule {ordinal}: anchored enclosing-condition spelling absent from structured WHEN: source lines {}-{}, `{}` (source outcome {}). Scope review required; THEN and refs do not supply WHEN prerequisites. {}", guard.line, guard.end_line, guard.expression.replace(['\r','\n'], " "), guard.outcome, QUALIFIER))
        }).collect();
        for (line, expression) in &self.query_predicates {
            if *line != anchor { continue; }
            let Some(predicate) = tokens(expression) else { continue; };
            if !when.windows(predicate.len()).enumerate().any(|(i,w)| w == predicate.as_slice()
                && !matches!(i.checked_sub(1).and_then(|n| when.get(n)), Some(Token::Punctuation('.'|'!')))
                && !matches!(when.get(i+predicate.len()), Some(Token::Punctuation('.'|'!')))) {
                warnings.push(format!("Rule {ordinal}: structured WHEN does not quote the complete compound query predicate at source line {line}: `{expression}`. Predicate scope review required: a partial condition may describe a necessary component, but does not establish that a row qualifies for the whole query. Joins, other clauses, binding, evaluation and semantic equivalence remain unverified; raw rule text is preserved."));
            }
        }
        warnings
    }
    pub fn query_coverage(&self) -> String {
        format!("Query predicate coverage: {}; {} supported source-bound single-line compound Where clauses shown (caps: 16 clauses, 4096 UTF-8 expression bytes, 1024 lines per declaration). Only simple Dim name = From declarations, unique statement ownership, balanced predicates and an immediately following Order By/Select clause are inspected. Declarations containing unquoted < are excluded, including XML literals and some comparisons. Multiline predicates, other declaration forms, unlisted or capped-out anchors, joins and semantic sufficiency remain unassessed. Missing literal coverage is a review lead, not proof a rule is false.", if self.available { "bounded inventory available" } else { "unknown: source-bound branch inventory unavailable" }, self.query_predicates.len())
    }
    pub fn prompt_context(&self) -> String {
        let mut text = format!("\n{}\n", self.coverage());
        text.push_str(&self.query_coverage()); text.push('\n');
        if !self.available { return text; }
        for (line, predicate) in &self.query_predicates {
            text.push_str(&format!("Complete source Where predicate at {line}: {predicate}\n"));
        }
        text.push_str("Anchored enclosing-condition checklist (an unlisted/ambiguous anchor is unknown, not unguarded). For each standalone structured WHEN, explicitly preserve applicable enclosing branch conditions; a condition mentioned only in THEN/refs or another rule is insufficient. Do not turn these syntactic contexts into semantic verification.\n");
        let mut shown = 0;
        for entry in &self.entries {
            if entry.conditions.is_empty() { continue; }
            let row = format!("Statement {} at {}-{}: {}\n", entry.statement_kind, entry.start_line, entry.end_line,
                entry.conditions.iter().map(|c| format!("[{}-{}: ({}) was {}]", c.line, c.end_line, c.expression, c.outcome)).collect::<Vec<_>>().join("; "));
            if text.len() + row.len() > 12_000 { break; }
            text.push_str(&row); shown += 1;
        }
        let total = self.entries.iter().filter(|e| !e.conditions.is_empty()).count();
        text.push_str(&format!("Checklist display: {shown}/{total} guarded entries; {} source lines explicitly unavailable. Omitted entries and all unlisted lines remain unexamined in this display.\n", self.unavailable_lines.len()));
        text
    }
}

/// A deliberately narrow lexical slice inside independently source-bound Roslyn
/// statement spans. The whole predicate is retained; no conjunction is promoted
/// into a sufficient condition and no alias/Boolean semantic binding is inferred.
fn query_predicates(body: &str, start: u32, entries: &[Entry], unavailable: &[u32]) -> Vec<(u32, String)> {
    let executable = super::business_outcome_dependencies::executable_lines(body, true);
    let uncommented = super::business_outcome_dependencies::source_without_comments(body, true);
    let code: Vec<_> = executable.lines().collect();
    let raw: Vec<_> = uncommented.lines().collect();
    let mut result = Vec::new();
    let mut bytes = 0;
    for entry in entries {
        if entry.statement_kind != "LocalDeclarationStatement" { continue; }
        let first = (entry.start_line-start) as usize;
        let last = (entry.end_line-start) as usize;
        // Declaration ownership does not establish ownership of XML literal
        // text inside that declaration. This lexical slice cannot distinguish
        // every XML token from a comparison: conservatively exclude both.
        // Strings/comments are already masked, so quoted '<' remains usable.
        if last-first>=1024 || code.get(first..=last).is_none_or(|lines| lines.iter().any(|line| line.contains('<'))) { continue; }
        let Some(header) = code.get(first).and_then(|line| tokens(line)) else { continue; };
        if !matches!(header.as_slice(), [Token::Word(dim), Token::Word(_), Token::Punctuation('='), Token::Word(from), ..] if dim == "dim" && from == "from") { continue; }
        for n in first..=last {
            let anchor = start+n as u32;
            if unavailable.contains(&anchor) || entries.iter().filter(|e| e.start_line<=anchor && anchor<=e.end_line).count()!=1 { continue; }
            let Some(line) = code.get(n) else { continue; };
            let line = line.trim_start();
            if !line.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("where")) || !line.as_bytes().get(5).is_some_and(u8::is_ascii_whitespace) { continue; }
            let Some(next) = code.get(n+1).and_then(|line| tokens(line)) else { continue; };
            if n == last || !matches!(next.as_slice(), [Token::Word(select), ..] if select == "select") && !matches!(next.as_slice(), [Token::Word(order),Token::Word(by),..] if order == "order" && by == "by") { continue; }
            let Some(expression) = raw.get(n).and_then(|line| line.trim_start().get(5..)).map(str::trim) else { continue; };
            if expression.len()>2048 { continue; }
            let Some(parts) = tokens(expression) else { continue; };
            let mut depth = 0i32; let mut compound = false; let mut valid = !parts.is_empty();
            for part in &parts {
                match part {
                    Token::Punctuation('(') => depth+=1,
                    Token::Punctuation(')') => { depth-=1; if depth<0 { valid=false; } },
                    Token::Punctuation(':'|'['|']'|'{'|'}') => valid=false,
                    Token::Word(w) if w == "_" || w == "function" || w == "from" || w == "select" || w == "where" => valid=false,
                    Token::Word(w) if depth==0 && (w=="and" || w=="andalso") => compound=true,
                    Token::Word(w) if depth==0 && (w=="or" || w=="orelse" || w=="xor") => valid=false,
                    _ => {}
                }
            }
            if !valid || depth!=0 || !compound { continue; }
            if result.len()>=16 || bytes+expression.len()>4096 { return result; }
            bytes+=expression.len();result.push((anchor,expression.to_string()));
        }
    }
    result
}

/// A token is either an entire quoted VB literal, an identifier, or punctuation.
/// Whitespace/identifier case normalize; literal case and escaped quotes do not.
#[derive(PartialEq, Eq)]
enum Token { Word(String), Literal(String), Punctuation(char) }
fn tokens(text: &str) -> Option<Vec<Token>> {
    if text.len() > 16_384 { return None; }
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0; let mut out = vec![];
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() { i += 1; continue; }
        if c == '\'' { break; }
        if c == '"' {
            let start = i; i += 1; let mut closed = false;
            while i < chars.len() {
                if chars[i] == '"' {
                    if chars.get(i+1) == Some(&'"') { i += 2; continue; }
                    i += 1; closed = true; break;
                }
                i += 1;
            }
            if !closed { return None; }
            out.push(Token::Literal(chars[start..i].iter().collect()));
        } else if c.is_alphanumeric() || c == '_' {
            let start = i; i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') { i += 1; }
            out.push(Token::Word(chars[start..i].iter().collect::<String>().to_lowercase()));
        } else { out.push(Token::Punctuation(c)); i += 1; }
    }
    Some(out)
}
fn hex64(text: &str) -> bool { text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) }
fn valid_condition(c: &Condition, lines: &[&str], start: u32, end: u32) -> bool {
    if c.line < start || c.end_line > end || c.line > c.end_line || c.expression.is_empty()
        || c.expression.len() > 1024 || !matches!(c.outcome.as_str(), "true"|"false") { return false; }
    let span = lines[(c.line-start) as usize..=(c.end_line-start) as usize].concat();
    // Verify exact expression source bytes first, then that it is the header expression,
    // not a spelling occurring only in a body statement, comment or literal.
    if !span.contains(&c.expression) { return false; }
    let (Some(header), Some(expr)) = (tokens(&span), tokens(&c.expression)) else { return false; };
    !expr.is_empty() && matches!(header.first(), Some(Token::Word(w)) if w == "if" || w == "elseif")
        && header.get(1..1+expr.len()) == Some(expr.as_slice())
        && matches!(header.get(1+expr.len()), Some(Token::Word(w)) if w == "then")
}

pub fn inspect(evidence: &OutcomeEvidence, body: &str, start: u32, language: &str, prompt_version: &str) -> Checklist {
    if !matches!(language,"vb"|"vbnet") { return Checklist::unknown("not applicable to this language"); }
    if body.is_empty() || start == 0 { return Checklist::unknown("unknown: invalid method body/range"); }
    // Preserve exact Roslyn expression CRLF bytes, including multiline guards.
    let lines: Vec<_> = body.split_inclusive('\n').collect();
    let Some(end) = start.checked_add(lines.len().saturating_sub(1) as u32) else { return Checklist::unknown("unknown: method range overflow"); };
    let mut checked = evidence.clone(); checked.fingerprint(body,prompt_version);
    if evidence.analysis_fingerprint.is_empty() || evidence.analysis_fingerprint != checked.analysis_fingerprint || evidence.caller_start_line != start {
        return Checklist::unknown("unknown: stale/mismatched body, evidence or prompt fingerprint");
    }
    let Some(paths) = &evidence.return_paths else { return Checklist::unknown("unknown: legacy or missing branch inventory"); };
    if paths.start_line != start || paths.end_line != end || !hex64(&paths.source_blake3) {
        return Checklist::unknown("unknown: source/range identity mismatch");
    }
    let Some(result) = &paths.result else { return Checklist::unknown("unknown: no structured source report"); };
    if result.get("version").and_then(|v|v.as_str()) != Some("vb-return-paths-v1") {
        return Checklist::unknown("unknown: unsupported parent report version");
    }
    let Some(raw) = result.get("branch_contexts") else { return Checklist::unknown("unknown: legacy, omitted or unavailable branch inventory"); };
    let Ok(inv) = serde_json::from_value::<Inventory>(raw.clone()) else { return Checklist::unknown("unknown: malformed branch inventory"); };
    if inv.version != VERSION || inv.status != "available" { return Checklist::unknown("unknown: unsupported version or unavailable branch inventory"); }
    if inv.method_start_line != start || inv.method_end_line != end
        || result.get("method_start_line").and_then(|v|v.as_u64()) != Some(start as u64)
        || result.get("method_end_line").and_then(|v|v.as_u64()) != Some(end as u64)
        || !hex64(&inv.source_sha256) || result.get("source_sha256").and_then(|v|v.as_str()) != Some(inv.source_sha256.as_str()) {
        return Checklist::unknown("unknown: inventory source fingerprint or method range mismatch");
    }
    if inv.entries.len() > LIMIT || inv.unavailable_lines.len() > 1024
        || inv.unavailable_lines.iter().any(|n| *n < start || *n > end)
        || inv.entries.iter().any(|e| e.statement_kind.is_empty() || e.statement_kind.len()>128 || e.start_line < start || e.end_line > end || e.start_line > e.end_line || e.conditions.len()>16
            || e.conditions.iter().any(|c| !valid_condition(c,&lines,start,end) || c.end_line > e.start_line)) {
        return Checklist::unknown("unknown: invalid/bounded-out entries or condition source spans");
    }
    let query_predicates = query_predicates(body,start,&inv.entries,&inv.unavailable_lines);
    Checklist { reason: format!("source-bound available inventory of {} entries; {} lines explicitly unavailable; only a unique matching entry supports a spelling check; unlisted/ambiguous anchors remain unknown", inv.entries.len(), inv.unavailable_lines.len()), entries:inv.entries, unavailable_lines:inv.unavailable_lines, query_predicates, available:true }
}
