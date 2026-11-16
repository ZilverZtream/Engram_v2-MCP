//! Same-owner VB constructor observations, not field binding or effective state.
use super::{
    business_logic_service::extract_logic_methods, business_outcome_dependencies::executable_lines,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::LazyLock};

const MAX_OBSERVATIONS: usize = 16;
const MAX_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstructorContext {
    pub source_file: String,
    pub source_blake3: Option<String>,
    pub status: String,
    pub observations: Vec<Assignment>,
    pub omissions: Vec<String>,
    pub interpretation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub member_spelling: String,
    pub constructor_owner: String,
    pub constructor_line: u32,
    pub line: u32,
    pub source: String,
    pub source_blake3: String,
}

impl ConstructorContext {
    /// Setup leads stay separate from inferred rules and effective runtime values.
    /// Complete assignments are shown or omitted whole, once per matrix document.
    pub fn qa_evidence(&self, current_file_hash: Option<&str>) -> String {
        let mut text = format!("Constructor input evidence: {}\n{}\n",
            qualification(Some(self), current_file_hash), self.interpretation);
        if self.source_blake3.as_deref().zip(current_file_hash)
            .is_none_or(|(recorded, current)| recorded != current) {
            text.push_str("Constructor source assignments withheld. Refresh the source analysis before using these setup leads; the complete stored document remains recoverable.\n");
            return text;
        }
        let mut shown = 0;
        for observation in &self.observations {
            let entry = format!("- Constructor {} at line {}; member spelling {}; supporting source {}:{}; file BLAKE3 {}; assignment BLAKE3 {}:\n  {}\n",
                observation.constructor_owner, observation.constructor_line, observation.member_spelling,
                self.source_file, observation.line, self.source_blake3.as_deref().unwrap_or("unavailable"),
                observation.source_blake3, observation.source);
            if shown == 4 || text.len() + entry.len() + 256 > 4096 { continue; }
            text.push_str(&entry); shown += 1;
        }
        text.push_str(&format!("Constructor setup leads shown {shown}/{}; omitted {}. Recover the complete evidence document above for all recorded assignments and gaps. These are not additional rules, chosen configuration values or executable test oracles.\n",
            self.observations.len(), self.observations.len().saturating_sub(shown)));
        text
    }

    pub fn unavailable(file: &str, reason: &str) -> Self {
        Self {
            source_file: file.replace('\\', "/"), source_blake3: None,
            status: "unavailable".into(), observations: vec![], omissions: vec![reason.into()],
            interpretation: "Same-owner single-line constructor assignment name matches only. No compiler field binding, complete initialization/write inventory, constructor selection, branch reachability, effective configuration value or runtime execution is established. Local shadowing and later writes remain possible. Original member rule anchors do not refer to these separate context lines.".into(),
        }
    }

    pub fn prompt_context(&self) -> String {
        let mut out = format!(
            "\nConstructor member-source observations: {}. {}\n",
            self.status, self.interpretation
        );
        for item in &self.observations {
            out.push_str(&format!("Separate supporting source {}:{} (constructor {} at line {}; file hash {}; excerpt hash {}), name match {}:\n{}\n",
                self.source_file, item.line, item.constructor_owner, item.constructor_line,
                self.source_blake3.as_deref().unwrap_or("unavailable"), item.source_blake3, item.member_spelling, item.source));
        }
        for gap in &self.omissions {
            out.push_str(&format!("Constructor context gap: {gap}\n"));
        }
        out
    }
}

pub fn collect(
    file: &str,
    source: &str,
    body: &str,
    owner: &str,
    start: u32,
) -> ConstructorContext {
    let mut result = ConstructorContext::unavailable(file, "");
    result.omissions.clear();
    let methods = extract_logic_methods(source, "vb");
    if !methods.iter().any(|method| {
        method.owner == owner
            && method.start_line == start
            && method.body.replace("\r\n", "\n") == body.replace("\r\n", "\n")
    }) {
        result.omissions.push("Selected member no longer matches the supplied source; constructor observations withheld.".into());
        return result;
    }
    result.source_blake3 = Some(blake3::hash(source.as_bytes()).to_hex().to_string());
    result.status = "source_observations_only".into();
    let executable = executable_lines(body, true);
    let names: BTreeSet<_> = executable
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:Me\s*\.\s*)?([A-Za-z_]\w*)\s*=\s*[^=]").expect("VB assignment")
    });
    let mut bytes = 0;
    for constructor in methods.iter().filter(|method| {
        method.owner == owner
            && method.name.eq_ignore_ascii_case("New")
            && method.start_line != start
    }) {
        let masked = executable_lines(&constructor.body, true);
        for (index, (code, original)) in masked.lines().zip(constructor.body.lines()).enumerate() {
            let Some(capture) = ASSIGNMENT.captures(code) else {
                continue;
            };
            if !names.contains(&capture[1].to_ascii_lowercase()) {
                continue;
            }
            // Multiline initializers need a full statement reader; never show a
            // clipped statement as complete initialization evidence.
            if original.trim_end().ends_with('_') {
                if result.omissions.len() < 16 {
                    result.omissions.push(format!(
                        "{}: multiline constructor assignment not expanded",
                        constructor.start_line + index as u32
                    ));
                }
                continue;
            }
            if result.observations.len() == MAX_OBSERVATIONS || bytes + original.len() > MAX_BYTES {
                result.omissions.push("Constructor observations capped at 16 assignments / 8 KiB original source; remaining assignments unexamined.".into());
                return result;
            }
            bytes += original.len();
            result.observations.push(Assignment {
                member_spelling: capture[1].into(),
                constructor_owner: constructor.owner.clone(),
                constructor_line: constructor.start_line,
                line: constructor.start_line + index as u32,
                source: original.into(),
                source_blake3: blake3::hash(original.as_bytes()).to_hex().to_string(),
            });
        }
    }
    result
}

/// A constructor can change while the analyzed method's own hash stays valid.
pub fn qualification(
    context: Option<&ConstructorContext>,
    current_file_hash: Option<&str>,
) -> String {
    let Some(context) = context else {
        return "Constructor member context: unknown (legacy analysis); no initialization evidence established.".into();
    };
    let status = match (context.source_blake3.as_deref(), current_file_hash) {
        (Some(recorded), Some(current)) if recorded == current => "source_current",
        (Some(_), Some(_)) => {
            "STALE_SUPPORTING_SOURCE: constructor/file changed; refresh analysis before relying on initialization observations"
        }
        _ => "unverified: supporting source unavailable",
    };
    format!(
        "Constructor member context: {status}; {} name-matched assignments, {} recorded gaps. Separate constructor source spans and hashes remain in full_document. No field binding, effective value or execution proof.",
        context.observations.len(),
        context.omissions.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_setting_identity_has_its_own_source_span_and_owner() {
        let source = "Class ImportPolicy\n Private denied As String\n Sub New()\n  denied = Settings.GetText(\"blocked_regions\")\n End Sub\n Function Admit(region As String) As Boolean\n  Return Not denied.Contains(region)\n End Function\nEnd Class\nClass Other\n Sub New()\n  denied = \"unrelated\"\n End Sub\nEnd Class\n";
        let method = extract_logic_methods(source, "vb")
            .into_iter()
            .find(|m| m.name == "Admit")
            .unwrap();
        let context = collect(
            "Policy.vb",
            source,
            &method.body,
            &method.owner,
            method.start_line,
        );
        assert_eq!(context.observations.len(), 1);
        let assignment = &context.observations[0];
        assert_eq!(assignment.line, 4);
        assert_eq!(assignment.source, source.lines().nth(3).unwrap());
        assert_eq!(
            assignment.source_blake3,
            blake3::hash(assignment.source.as_bytes())
                .to_hex()
                .to_string()
        );
        assert!(context.prompt_context().contains("blocked_regions"));
        assert!(
            context
                .prompt_context()
                .contains("No compiler field binding")
        );
        assert!(!context.prompt_context().contains("unrelated"));
        let changed = source.replace("blocked_regions", "disabled_regions");
        let updated = collect(
            "Policy.vb",
            &changed,
            &method.body,
            &method.owner,
            method.start_line,
        );
        assert_ne!(context.source_blake3, updated.source_blake3);
        assert!(updated.prompt_context().contains("disabled_regions"));
    }

    #[test]
    fn comment_and_string_mentions_do_not_import_constructor_state() {
        let source = "Class Policy\n Sub New()\n  secret = \"not relevant\"\n End Sub\n Function Describe() As String\n  ' secret\n  Return \"secret\"\n End Function\nEnd Class\n";
        let method = extract_logic_methods(source, "vb")
            .into_iter()
            .find(|m| m.name == "Describe")
            .unwrap();
        let context = collect(
            "Policy.vb",
            source,
            &method.body,
            &method.owner,
            method.start_line,
        );
        assert!(context.observations.is_empty());
        let changed = source.replace("Return \"secret\"", "Return \"changed\"");
        let withheld = collect(
            "Policy.vb",
            &changed,
            &method.body,
            &method.owner,
            method.start_line,
        );
        assert_eq!(withheld.status, "unavailable");
        assert!(withheld.observations.is_empty());
    }
}
