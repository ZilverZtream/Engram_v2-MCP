//! Literal VB generic registrations in the selected route's own source file.
//! These declarations do not establish a runtime container or resolve type names.
use super::routing::{Source, Token, tokens};
use serde_json::{Value, json};

const MAX_REGISTRATIONS: usize = 64;

fn word(token: &Token, expected: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(expected)
}

fn type_spelling(tokens: &[Token]) -> Option<String> {
    if tokens.is_empty() || tokens.len() % 2 == 0 {return None;}
    for (i, token) in tokens.iter().enumerate() {
        if token.string {return None;}
        if i % 2 == 1 {
            if token.text != "." {return None;}
        } else if token.text.is_empty() || !token.text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
    }
    Some(tokens.iter().map(|token| token.text.as_str()).collect())
}

pub(super) fn in_route_source(path: &str, sources: &[Source<'_>]) -> Value {
    let mut rows = Vec::new(); let mut gaps = Vec::new(); let mut truncated = false;
    let Some(source) = sources.iter().find(|source| source.path == path) else {
        return json!({"status":"unavailable","registrations":[],"gaps":["Selected route source was not loaded; dependency registrations are unknown."]});
    };
    let tokens = tokens(source.content, true);
    let hash = blake3::hash(source.content.as_bytes()).to_hex().to_string();
    let lines = source.content.lines().collect::<Vec<_>>();
    for i in 2..tokens.len().saturating_sub(4) {
        if !word(&tokens[i], "RegisterType") || !word(&tokens[i-1], ".")
            || tokens[i-2].string || !word(&tokens[i+1], "(") || !word(&tokens[i+2], "Of") {continue;}
        let Some(end) = tokens[i+3..].iter().take(64).position(|token| word(token, ")")) else {
            gaps.push(format!("{}:{}: generic registration syntax not resolved", path, tokens[i].line));continue;
        };
        let end = i+3+end; let arguments = &tokens[i+3..end];
        let parts = arguments.split(|token| word(token, ",")).collect::<Vec<_>>();
        let types = match parts.as_slice() {
            [contract, implementation] => type_spelling(contract).zip(type_spelling(implementation)),
            _ => None,
        };
        let Some((contract, implementation)) = types else {
            gaps.push(format!("{}:{}: only two literal qualified type names are supplied; registration not interpreted", path, tokens[i].line));continue;
        };
        if !tokens.get(end+1).is_some_and(|token| word(token, "(")) {continue;}
        if rows.len() == MAX_REGISTRATIONS {truncated = true;break;}
        let line = tokens[i].line;
        rows.push(json!({"receiver_spelling":tokens[i-2].text,
            "contract_type_spelling":contract,"implementation_type_spelling":implementation,
            "source_path":path,"source_line":line,"source_blake3":hash,
            "evidence":lines.get(line-1).copied().unwrap_or_default()}));
    }
    if truncated {gaps.push(format!("Registration inventory capped at {MAX_REGISTRATIONS}; remaining declarations were not supplied"));}
    json!({"status":"source_declarations_only","registrations":rows,"truncated":truncated,"gaps":gaps,
        "scope":"Literal generic RegisterType(Of Contract, Implementation) invocations in the selected route configuration file only; other configuration files, factories, dynamic registrations and container modifications are not covered.",
        "interpretation":"Type/receiver spellings and current source citations are supplied without resolving imports, namespaces, container provider, active registration order, conditions, lifetime, receiver selection or execution. Registrations are not matched to or used to suppress interface implementation candidates; inspect them with the cited call/Implements evidence. Source declarations do not prove this configuration executes."})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_trace_supplies_current_declared_contract_implementation_without_claiming_runtime_selection() {
        let source = "' container.RegisterType(Of IWrong, Wrong)()\r\nDim example = \"container.RegisterType(Of IQuoted, Quoted)()\"\r\ncontainer.RegisterType(Of svc.IStore, svc.Store)(New HierarchicalLifetimeManager())\r\ncontainer.RegisterType(Of svc.IStore, svc.Replacement)()\r\n";
        let outside = "other.RegisterType(Of IForeign, Foreign)()\n";
        let sources = [Source{path:"Site/WebApiConfig.vb",content:source},Source{path:"Other/Config.vb",content:outside}];
        let result = in_route_source("Site/WebApiConfig.vb", &sources);
        let rows = result["registrations"].as_array().unwrap();assert_eq!(rows.len(),2);
        assert_eq!(rows[0]["contract_type_spelling"],"svc.IStore");assert_eq!(rows[0]["implementation_type_spelling"],"svc.Store");
        assert_eq!(rows[0]["source_line"],3);assert_eq!(rows[0]["source_blake3"],blake3::hash(source.as_bytes()).to_hex().to_string());
        assert_eq!(rows[1]["implementation_type_spelling"],"svc.Replacement");assert_eq!(result["status"],"source_declarations_only");
        assert!(result["interpretation"].as_str().unwrap().contains("not matched to or used to suppress"));
        assert_eq!(in_route_source("Unavailable.vb", &sources)["status"],"unavailable");
    }
}
