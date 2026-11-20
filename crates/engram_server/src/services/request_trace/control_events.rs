//! Explicit WebForms control events from authored markup, not browser execution.
use super::execution::without_markup_comments;
use super::routing::{Source, tokens};
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

const ATTRIBUTE: &str = r#"\s+([A-Za-z_:][\w:.-]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|[^\s"'=<>`]+))?"#;
static ATTRIBUTES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(ATTRIBUTE).expect("control attributes"));
static START_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"<[A-Za-z][\w:.-]*(?:{ATTRIBUTE})*\s*/?>"))
        .expect("control start tags")
});

pub(super) fn start_tags(source: &str) -> impl Iterator<Item = regex::Match<'_>> {
    START_TAGS.find_iter(source)
}

pub(super) fn attributes(tag: &str) -> impl Iterator<Item = (&str, &str)> {
    ATTRIBUTES.captures_iter(tag).filter_map(|capture| {
        let name = capture.get(1)?.as_str();
        let value = capture.get(2).or_else(|| capture.get(3))?.as_str();
        Some((name, value))
    })
}

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
pub struct Subscription {
    pub handler: String,
    pub file: String,
    pub line: u32,
    pub source_blake3: String,
    pub excerpt: String,
}

#[derive(Serialize)]
pub struct Evidence {
    pub control: String,
    pub event: String,
    pub markup_bindings: Vec<Binding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vb_subscriptions: Vec<Subscription>,
    pub truncated: bool,
    pub interpretation: &'static str,
}

pub(super) fn collect(
    markup_source: Source<'_>,
    codebehind: Source<'_>,
    control: &str,
    event: &str,
) -> Evidence {
    let Source {
        path: file,
        content: source,
    } = markup_source;
    let mut evidence = Evidence {
        control: control.into(),
        event: event.into(),
        markup_bindings: vec![],
        vb_subscriptions: vec![],
        truncated: false,
        interpretation: "Current authored server-control event declarations and direct, single-line VB AddHandler subscriptions. Corresponding admitted method entries remain static candidates. Naming containers, control creation, postback/event validation, client event wiring, branch reachability, lifecycle order and actual execution are not established. Explicit VB Handles bindings can supply entries independently of these observations.",
    };
    let markup = without_markup_comments(source);
    let attribute_name = format!("On{event}");
    let source_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    let mut bytes = 0;
    for tag in start_tags(&markup) {
        let mut id = None;
        let mut runat = None;
        let mut handler = None;
        for (name, value) in attributes(tag.as_str()) {
            if name.eq_ignore_ascii_case("ID") {
                id = Some(value);
            } else if name.eq_ignore_ascii_case("runat") {
                runat = Some(value);
            } else if name.eq_ignore_ascii_case(&attribute_name) {
                handler = Some(value);
            }
        }
        if !id.is_some_and(|id| id.eq_ignore_ascii_case(control))
            || !runat.is_some_and(|value| value.eq_ignore_ascii_case("server"))
        {
            continue;
        }
        let Some(handler) = handler.filter(|name| identifier(name)) else {
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
            handler: handler.into(),
            attribute: attribute_name.clone(),
            file: file.into(),
            line,
            end_line: line + tag.as_str().bytes().filter(|byte| *byte == b'\n').count() as u32,
            source_blake3: source_hash.clone(),
            excerpt: tag.as_str().into(),
        });
    }
    if codebehind.path.to_ascii_lowercase().ends_with(".vb") {
        let source_hash = blake3::hash(codebehind.content.as_bytes()).to_hex().to_string();
        for (index, line) in codebehind.content.lines().enumerate() {
            let statement = line.trim_start();
            if !statement
                .get(..10)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("AddHandler"))
            {
                continue;
            }
            let statement_tokens = tokens(statement, true);
            let [verb, receiver, dot, binding_event, comma, address_of, handler] =
                statement_tokens.as_slice()
            else {
                continue;
            };
            if statement_tokens.iter().any(|token| token.string)
                || !verb.text.eq_ignore_ascii_case("AddHandler")
                || !receiver.text.eq_ignore_ascii_case(control)
                || dot.text != "."
                || !binding_event.text.eq_ignore_ascii_case(event)
                || comma.text != ","
                || !address_of.text.eq_ignore_ascii_case("AddressOf")
                || !identifier(&handler.text)
            {
                continue;
            }
            if evidence.markup_bindings.len() + evidence.vb_subscriptions.len() == 32
                || bytes + line.len() > 16 * 1024
            {
                evidence.truncated = true;
                break;
            }
            bytes += line.len();
            evidence.vb_subscriptions.push(Subscription {
                handler: handler.text.clone(),
                file: codebehind.path.into(),
                line: index as u32 + 1,
                source_blake3: source_hash.clone(),
                excerpt: statement.into(),
            });
        }
    }
    evidence
}
