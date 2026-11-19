//! Follow unresolved direct calls through authored WebForms control registrations.
use super::{control_routes, execution, routing};
use engram_graph::{GraphStore, Node};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct Resolver<'a> {
    graph: &'a GraphStore,
    project: &'a str,
    page: routing::Source<'a>,
    codebehind: routing::Source<'a>,
    sources: &'a [routing::Source<'a>],
    selected: &'a BTreeMap<String, u64>,
    bindings: BTreeMap<String, Option<control_routes::Binding>>,
}

impl<'a> Resolver<'a> {
    pub(super) fn new(
        graph: &'a GraphStore,
        project: &'a str,
        page: &str,
        codebehind: Option<&str>,
        sources: &'a [routing::Source<'a>],
        selected: &'a BTreeMap<String, u64>,
    ) -> Option<Self> {
        let codebehind = codebehind?;
        Some(Self {
            graph,
            project,
            page: *sources.iter().find(|source| source.path == page)?,
            codebehind: *sources.iter().find(|source| source.path == codebehind)?,
            sources,
            selected,
            bindings: BTreeMap::new(),
        })
    }

    pub(super) fn resolve(
        &mut self,
        caller: &Node,
        unresolved: &str,
    ) -> anyhow::Result<Option<(Node, Value)>> {
        if caller.file_path.as_str() != self.codebehind.path
            || !matches!(caller.language.as_str(), "vb" | "vbnet")
        {
            return Ok(None);
        }
        let Some((receiver, method)) = unresolved
            .strip_prefix("::")
            .and_then(|name| name.split_once('.'))
        else {
            return Ok(None);
        };
        let Some((call_line, call_source)) = self
            .codebehind
            .content
            .lines()
            .enumerate()
            .skip(caller.start_line.saturating_sub(1) as usize)
            .take(caller.end_line.saturating_sub(caller.start_line) as usize + 1)
            .find(|(_, line)| direct_call(line, receiver, method))
        else {
            return Ok(None);
        };
        let key = receiver.to_ascii_lowercase();
        if !self.bindings.contains_key(&key) {
            let route = control_routes::resolve_instances(
                self.graph,
                self.project,
                &self.page,
                receiver,
                &[receiver],
                self.sources,
                self.selected,
            )?;
            let unique = if route.bindings.len() == 1 && !route.truncated && route.gaps.is_empty() {
                route.bindings.into_iter().next()
            } else {
                None
            };
            self.bindings.insert(key.clone(), unique);
        }
        let Some(binding) = self.bindings[&key].as_ref() else {
            return Ok(None);
        };
        let Some(markup) = self
            .sources
            .iter()
            .find(|source| source.path == binding.file)
        else {
            return Ok(None);
        };
        let Some(code) = execution::control_codebehind(*markup, self.sources, &mut Vec::new())
        else {
            return Ok(None);
        };
        let markup_text = execution::without_markup_comments(markup.content);
        let Some(class) = execution::attribute(&markup_text, "Inherits") else {
            return Ok(None);
        };
        let class = class.rsplit('.').next().unwrap_or(&class);

        let nodes =
            self.graph
                .query_nodes_in_file(self.project, Some("function"), code.path, 257)?;
        if nodes.len() > 256 {
            return Ok(None);
        }
        let mut candidates = nodes.into_iter().filter(|node| {
            let name = node.name.rsplit_once('.');
            name.is_some_and(|(owner, name)| {
                owner
                    .rsplit('.')
                    .next()
                    .is_some_and(|owner| owner.eq_ignore_ascii_case(class))
                    && name.eq_ignore_ascii_case(method)
            })
        });
        let Some(target) = candidates.next().filter(|_| candidates.next().is_none()) else {
            return Ok(None);
        };
        if !execution::verified_node(
            self.graph,
            self.project,
            &target,
            self.sources,
            self.selected,
        )? || !code
            .content
            .lines()
            .nth(target.start_line.saturating_sub(1) as usize)
            .is_some_and(|header| public_parameterless_method(header, method))
        {
            return Ok(None);
        }
        let evidence = json!({
            "call_file": self.codebehind.path, "call_line": call_line + 1,
            "call_excerpt": call_source.trim(),
            "registration": binding,
            "interpretation": "Direct authored call and unique literal registered-control source candidate. Runtime receiver binding and execution remain unverified."
        });
        Ok(Some((target, evidence)))
    }
}

fn direct_call(line: &str, receiver: &str, method: &str) -> bool {
    let tokens = routing::tokens(line, true);
    let mut tokens = tokens.as_slice();
    if let [first, dot, rest @ ..] = tokens
        && !first.string
        && !dot.string
        && first.text.eq_ignore_ascii_case("Me")
        && dot.text == "."
    {
        tokens = rest;
    }
    let expected = [receiver, ".", method, "(", ")"];
    tokens.len() == expected.len()
        && tokens
            .iter()
            .zip(expected)
            .all(|(token, want)| !token.string && token.text.eq_ignore_ascii_case(want))
}

fn public_parameterless_method(header: &str, method: &str) -> bool {
    let tokens = routing::tokens(header, true);
    let Some(slot) = tokens.iter().position(|token| {
        !token.string
            && (token.text.eq_ignore_ascii_case("Sub")
                || token.text.eq_ignore_ascii_case("Function"))
    }) else {
        return false;
    };
    let modifiers = &tokens[..slot];
    let arguments = tokens.get(slot + 1..slot + 4);
    arguments.is_some_and(|tokens| {
        tokens
            .iter()
            .zip([method, "(", ")"])
            .all(|(token, want)| !token.string && token.text.eq_ignore_ascii_case(want))
    }) && modifiers
        .iter()
        .any(|token| !token.string && token.text.eq_ignore_ascii_case("Public"))
        && !modifiers
            .iter()
            .any(|token| !token.string && token.text.eq_ignore_ascii_case("Shared"))
}
