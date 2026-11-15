//! Literal-key observations for conventional typed settings getters.
use super::{ExtractedEdge, ExtractedSymbol, Regex, vb_settings_code_mask};
use std::{collections::HashMap, sync::LazyLock};

static GETTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)\b(?P<store>[A-Za-z_]\w*(?:setting|config)\w*|settings?|config)\s*\.\s*(?P<getter>Get(?:TextList|Text|Secret|Integer|Int|Boolean|Bool|Decimal|Dec|Double|String|ByID))\s*\(\s*"(?P<key>(?:[^"]|"")*)"\s*[,)]"#
).expect("typed settings getter syntax")
});

#[derive(Debug, Clone, serde::Serialize)]
pub struct Observation {
    pub name: String,
    pub key: String,
    pub store: String,
    pub getter: String,
    pub line: u32,
}

fn read_line(line: &str, line_no: u32) -> impl Iterator<Item = Observation> + '_ {
    let mask = vb_settings_code_mask(line);
    GETTER.captures_iter(line).filter_map(move |cap| {
        if mask.as_bytes()[cap.get(0).unwrap().start()] == b' ' {
            return None;
        }
        let key = cap["key"].replace("\"\"", "\"");
        if key.is_empty() {
            return None;
        }
        let store = cap["store"].to_ascii_lowercase();
        Some(Observation {
            name: format!("{store}[{key:?}]"),
            key,
            store,
            getter: cap["getter"].to_owned(),
            line: line_no,
        })
    })
}

/// Bounded source observations without Roslyn, a project inventory or graph writes.
/// The Boolean reports omitted matching observations, not complete VB semantics.
pub fn source_observations(
    source: &str,
    limit: usize,
    query: Option<&str>,
) -> (Vec<Observation>, bool) {
    let mut rows: Vec<_> = source
        .lines()
        .enumerate()
        .flat_map(|(i, line)| read_line(line, i as u32 + 1))
        .filter(|row| query.is_none_or(|query| row.name == query || row.key == query))
        .take(limit.saturating_add(1))
        .collect();
    let omitted = rows.len() > limit;
    rows.truncate(limit);
    (rows, omitted)
}

pub(super) fn append(
    line: &str,
    line_no: u32,
    owner: String,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    for Observation {
        name,
        key,
        store,
        getter,
        ..
    } in read_line(line, line_no)
    {
        if let Some(existing) = symbols
            .iter_mut()
            .find(|s| s.kind == "setting_key" && s.name == name && s.start_line == line_no)
        {
            let metadata = existing.metadata.as_mut().unwrap();
            let getters = metadata.get_mut("literal_setting_getters").unwrap();
            if !getters.split(';').any(|g| g.eq_ignore_ascii_case(&getter)) {
                getters.push(';');
                getters.push_str(&getter);
            }
            continue;
        }
        let metadata = HashMap::from([
            ("literal_setting_key".into(), key),
            ("literal_setting_store".into(), store),
            ("literal_setting_getters".into(), getter),
            ("fqn".into(), name.clone()),
            ("observation".into(), "Literal first argument of a name-shaped typed settings getter; compiler/store binding and runtime value unverified.".into()),
        ]);
        symbols.push(ExtractedSymbol {
            name: name.clone(),
            kind: "setting_key".into(),
            start_line: line_no,
            end_line: line_no,
            metadata: Some(metadata.clone()),
        });
        edges.push(ExtractedEdge {
            source_name: owner.clone(),
            source_kind: "function".into(),
            source_start_line: line_no,
            source_language: "vb".into(),
            target_name: name,
            target_kind: Some("setting_key".into()),
            target_start_line: Some(line_no),
            kind: "reads_setting".into(),
            metadata: Some(metadata),
        });
    }
}
