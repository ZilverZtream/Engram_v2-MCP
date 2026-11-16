//! Explicit WebForms control events from authored markup, not browser execution.
use super::execution::{attribute, without_markup_comments};
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

fn identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 128
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Existing action spellings stay with their current readers. Only the explicit
/// `event control.event` form is validated and interpreted here.
pub fn requested(action: Option<&str>) -> anyhow::Result<Option<(&str, &str)>> {
    let Some(action) = action.map(str::trim) else {
        return Ok(None);
    };
    let (verb, rest) = action.split_once(' ').unwrap_or((action, ""));
    if !verb.eq_ignore_ascii_case("event") {
        return Ok(None);
    }
    let (control, event) = rest.trim().split_once('.').ok_or_else(|| {
        anyhow::anyhow!("control event action must be 'event controlID.EventName'")
    })?;
    anyhow::ensure!(
        control.split('/').count() <= 8 && control.split('/').all(identifier) && identifier(event),
        "control event action requires1..8simple control path identifiers and an event, e.g. event panel/ddlType.SelectedIndexChanged"
    );
    Ok(Some((control, event)))
}

#[derive(Serialize)]
pub struct Binding {
    pub handler: String,
    pub attribute: String,
    pub file: String,
    pub line: u32,
    pub end_line: u32,
    pub source_blake3: String,
    pub excerpt: String,
}

#[derive(Serialize)]
pub struct Evidence {
    pub control: String,
    pub event: String,
    pub markup_bindings: Vec<Binding>,
    pub truncated: bool,
    pub interpretation: &'static str,
}

pub fn collect(file: &str, source: &str, control: &str, event: &str) -> Evidence {
    let mut evidence = Evidence {
        control: control.into(),
        event: event.into(),
        markup_bindings: vec![],
        truncated: false,
        interpretation: "Current authored server-control event declarations. Corresponding admitted method entries remain static candidates. Naming containers, control creation, postback/event validation, client event wiring, branch reachability, lifecycle order and actual execution are not established. Explicit VB Handles bindings can supply entries independently of these markup observations.",
    };
    let markup = without_markup_comments(source);
    static TAGS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<[A-Za-z][^>]*>").expect("markup tags"));
    let attribute_name = format!("On{event}");
    let source_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    let mut bytes = 0;
    for tag in TAGS.find_iter(&markup) {
        if !attribute(tag.as_str(), "ID").is_some_and(|id| id.eq_ignore_ascii_case(control))
            || !attribute(tag.as_str(), "runat")
                .is_some_and(|value| value.eq_ignore_ascii_case("server"))
        {
            continue;
        }
        let Some(handler) =
            attribute(tag.as_str(), &attribute_name).filter(|name| identifier(name))
        else {
            continue;
        };
        if evidence.markup_bindings.len() == 32 || bytes + tag.len() > 16 * 1024 {
            evidence.truncated = true;
            break;
        }
        bytes += tag.len();
        let line = markup[..tag.start()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1;
        evidence.markup_bindings.push(Binding {
            handler,
            attribute: attribute_name.clone(),
            file: file.into(),
            line,
            end_line: line + tag.as_str().bytes().filter(|byte| *byte == b'\n').count() as u32,
            source_blake3: source_hash.clone(),
            excerpt: tag.as_str().into(),
        });
    }
    evidence
}
