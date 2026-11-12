//! React and Redux Toolkit observations require source imports and declarations.
//! They describe potential static accesses, not render/dispatch execution.
use super::*;

fn property<'t>(object: Node<'t>, name: &str, source: &str) -> Option<Node<'t>> {
    children(object).into_iter().find_map(|child| {
        let key = field(child, &["key", "name"])?;
        let key = literal(key, source).unwrap_or_else(|| text(key, source).into());
        (key == name).then(|| field(child, &["value"]).unwrap_or(child))
    })
}
fn argument(node: Node<'_>, index: usize) -> Option<Node<'_>> {
    field(node, &["arguments"])?.named_child(index)
}
fn callable(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_expression" | "arrow_function" | "method_definition" | "function_declaration"
    )
}
fn nearest_callable(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if callable(parent) {
            return Some(parent);
        }
        node = parent;
    }
    None
}
fn parameter(node: Node<'_>, source: &str) -> Option<String> {
    let mut first = field(node, &["parameter"])
        .or_else(|| field(node, &["parameters"]).and_then(|p| p.named_child(0)))?;
    if matches!(first.kind(), "required_parameter" | "optional_parameter") {
        first = field(first, &["pattern", "name"])?;
    }
    (first.kind() == "identifier").then(|| text(first, source).into())
}
struct Context<'t> {
    name: String,
    call: Node<'t>,
}
struct Action<'t> {
    name: String,
    accesses: Vec<(Node<'t>, String, (bool, bool))>,
}
struct Slice<'t> {
    name: String,
    actions: Vec<Action<'t>>,
}
struct Store {
    name: String,
    mounts: Vec<(String, String)>,
}

fn keys(path: &str, slice: &str, suffix: &str, stores: &[Store]) -> Vec<String> {
    let mounted = stores
        .iter()
        .flat_map(|store| {
            store.mounts.iter().filter_map(move |(mount, name)| {
                (name == slice).then(|| format!("{path}::{}{mount}{suffix}", store.name))
            })
        })
        .collect::<Vec<_>>();
    if mounted.is_empty() {
        vec![format!("{path}::{slice}{suffix}")]
    } else {
        mounted
    }
}

fn state_selection<'t>(node: Node<'t>, source: &str) -> Option<(Node<'t>, String)> {
    if node.kind() == "call_expression" {
        return Some((node, String::new()));
    }
    if !member(node) {
        return None;
    }
    let object = field(node, &["object", "value", "operand"])?;
    let (call, mut suffix) = state_selection(object, source)?;
    if let Some(property) = field(node, &["property", "attribute", "field"]) {
        suffix.push('.');
        suffix.push_str(text(property, source));
    } else {
        let index = field(node, &["index", "subscript"])?;
        suffix.push('[');
        suffix.push_str(&serde_json::to_string(&literal(index, source)?).ok()?);
        suffix.push(']');
    }
    Some((call, suffix))
}

pub(super) fn extract(
    bindings: &Bindings<'_, '_>,
    nodes: &[Node<'_>],
    root: Node<'_>,
    out: &mut Output<'_>,
) {
    let Some(script) = bindings.script else {
        return;
    };
    let source = out.source;
    let mut contexts = Vec::new();
    let mut slices = Vec::new();
    let mut stores = Vec::new();
    for &node in nodes {
        if node.kind() != "variable_declarator" || !top_level(node, root) {
            continue;
        }
        let Some(name) = field(node, &["name"]).filter(|n| n.kind() == "identifier") else {
            continue;
        };
        let Some(call) = field(node, &["value"]).filter(|n| n.kind() == "call_expression") else {
            continue;
        };
        let Some(callee) = field(call, &["function"]) else {
            continue;
        };
        let Some((module, api)) = bindings.api(callee, source) else {
            continue;
        };
        let name = text(name, source).to_owned();
        match (module.as_str(), api.as_str()) {
            ("react", "createContext") => contexts.push(Context { name, call }),
            ("@reduxjs/toolkit", "createSlice") => {
                let mut actions = Vec::new();
                if let Some(reducers) =
                    argument(call, 0).and_then(|options| property(options, "reducers", source))
                {
                    for entry in children(reducers) {
                        let Some(action_name) = field(entry, &["key", "name"]) else {
                            continue;
                        };
                        let function = field(entry, &["value"]).unwrap_or(entry);
                        if !callable(function) {
                            out.issues.insert(format!(
                                "redux_reducer_indirection:{}",
                                node.start_position().row + 1
                            ));
                            continue;
                        }
                        let Some(state) = parameter(function, source) else {
                            continue;
                        };
                        let Some(body) = field(function, &["body"]) else {
                            continue;
                        };
                        let mut accesses = Vec::new();
                        for &access in nodes {
                            if !contains(body, access)
                                || !nearest_callable(access)
                                    .is_some_and(|owner| owner.id() == function.id())
                            {
                                continue;
                            }
                            if access.parent().is_some_and(|parent| member(parent)) {
                                continue;
                            }
                            if let Some((base, suffix)) = access_path(access, source) {
                                if base == state
                                    && !script
                                        .state_binding_shadowed_below(access, &state, function)
                                {
                                    accesses.push((access, suffix, direction(access)));
                                }
                            }
                        }
                        if accesses
                            .iter()
                            .any(|(_, suffix, (_, writes))| suffix.is_empty() && *writes)
                        {
                            out.issues.insert(format!(
                                "redux_state_parameter_rebound:{}",
                                function.start_position().row + 1
                            ));
                            accesses.clear();
                        }
                        actions.push(Action {
                            name: literal(action_name, source)
                                .unwrap_or_else(|| text(action_name, source).into()),
                            accesses,
                        });
                    }
                }
                if argument(call, 0)
                    .and_then(|options| property(options, "extraReducers", source))
                    .is_some()
                {
                    out.issues.insert(format!(
                        "redux_extra_reducers_not_resolved:{}",
                        node.start_position().row + 1
                    ));
                }
                slices.push(Slice { name, actions });
            }
            ("@reduxjs/toolkit", "configureStore") => {
                out.emit(call, "Redux", &format!("{}::{name}", out.path), false, true);
                let mut mounts = Vec::new();
                if let Some(reducer) =
                    argument(call, 0).and_then(|options| property(options, "reducer", source))
                {
                    if let Some((slice, suffix)) = access_path(reducer, source) {
                        if suffix == ".reducer" {
                            mounts.push((String::new(), slice));
                        }
                    } else if reducer.kind() == "object" {
                        for entry in children(reducer) {
                            if let (Some(key), Some(value)) =
                                (field(entry, &["key"]), field(entry, &["value"]))
                            {
                                if let Some((slice, suffix)) = access_path(value, source) {
                                    if suffix == ".reducer" {
                                        let mount = literal(key, source)
                                            .unwrap_or_else(|| text(key, source).into());
                                        mounts.push((format!(".{mount}"), slice));
                                    }
                                }
                            }
                        }
                    }
                }
                if mounts.is_empty() {
                    out.issues.insert(format!(
                        "redux_store_reducer_binding_unknown:{}",
                        node.start_position().row + 1
                    ));
                }
                stores.push(Store { name, mounts });
            }
            _ => {}
        }
    }
    for context in &contexts {
        if contexts.iter().filter(|c| c.name == context.name).count() != 1 {
            continue;
        }
        out.emit(
            context.call,
            "ReactContext",
            &format!("{}::{}", out.path, context.name),
            false,
            true,
        );
    }
    for slice in &slices {
        for action in &slice.actions {
            for (site, suffix, access) in &action.accesses {
                for key in keys(&out.path, &slice.name, suffix, &stores) {
                    out.emit(*site, "Redux", &key, access.0, access.1);
                }
            }
        }
    }
    for &node in nodes {
        if matches!(
            node.kind(),
            "jsx_opening_element" | "jsx_self_closing_element"
        ) {
            if let Some(name) = field(node, &["name"]) {
                if let Some((context, suffix)) = access_path(name, source) {
                    if suffix == ".Provider"
                        && contexts.iter().filter(|c| c.name == context).count() == 1
                        && !script.state_binding_shadowed(node, &context, root, true)
                    {
                        out.emit(
                            name,
                            "ReactContext",
                            &format!("{}::{context}", out.path),
                            false,
                            true,
                        );
                    } else if let Some(prefix) = suffix.strip_suffix(".Provider") {
                        references::record(
                            bindings,
                            root,
                            name,
                            &context,
                            prefix,
                            Some("ReactContext"),
                            (false, true),
                            out,
                        );
                    }
                }
            }
        }
        if node.kind() == "call_expression" {
            if let Some(callee) = field(node, &["function"]) {
                if bindings
                    .api(callee, source)
                    .is_some_and(|(module, api)| module == "react" && api == "useContext")
                {
                    if let Some(arg) = argument(node, 0).filter(|n| n.kind() == "identifier") {
                        let name = text(arg, source);
                        if contexts.iter().filter(|c| c.name == name).count() == 1
                            && !script.state_binding_shadowed(node, name, root, true)
                        {
                            out.emit(
                                node,
                                "ReactContext",
                                &format!("{}::{name}", out.path),
                                true,
                                false,
                            );
                        } else {
                            if !references::record(
                                bindings,
                                root,
                                node,
                                name,
                                "",
                                Some("ReactContext"),
                                (true, false),
                                out,
                            ) {
                                out.issues.insert(format!(
                                    "react_context_binding_unknown:{}",
                                    node.start_position().row + 1
                                ));
                            }
                        }
                    }
                }
                if let Some((store, suffix)) = access_path(callee, source) {
                    if suffix == ".dispatch"
                        && stores.iter().filter(|s| s.name == store).count() == 1
                        && !script.state_binding_shadowed(node, &store, root, true)
                    {
                        let action = argument(node, 0)
                            .filter(|n| n.kind() == "call_expression")
                            .and_then(|call| field(call, &["function"]))
                            .and_then(|callee| access_path(callee, source));
                        let mut resolved = false;
                        if let Some((slice, action)) = action {
                            if !script.state_binding_shadowed(node, &slice, root, true) {
                                for candidate in slices.iter().filter(|s| s.name == slice) {
                                    for reducer in &candidate.actions {
                                        if action == format!(".actions.{}", reducer.name) {
                                            for (_, suffix, (_, writes)) in &reducer.accesses {
                                                if !writes {
                                                    continue;
                                                }
                                                let selected = stores
                                                    .iter()
                                                    .filter(|s| s.name == store)
                                                    .collect::<Vec<_>>();
                                                for configured in selected {
                                                    for (mount, mounted) in &configured.mounts {
                                                        if mounted == &slice {
                                                            out.emit(
                                                                node,
                                                                "Redux",
                                                                &format!(
                                                                    "{}::{store}{mount}{suffix}",
                                                                    out.path
                                                                ),
                                                                false,
                                                                true,
                                                            );
                                                            resolved = true;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !resolved {
                            out.issues.insert(format!(
                                "redux_dispatch_effect_unknown:{}",
                                node.start_position().row + 1
                            ));
                        }
                    } else if let Some(prefix) = suffix.strip_suffix(".dispatch") {
                        let before = out.references.len();
                        if references::record(
                            bindings,
                            root,
                            node,
                            &store,
                            prefix,
                            Some("Redux"),
                            (false, true),
                            out,
                        ) && out.references.len() > before
                        {
                            out.references.last_mut().unwrap().unbound_reason =
                                Some("imported_redux_dispatch_effect_unknown".into());
                        }
                    }
                }
            }
        }
        if member(node) && !node.parent().is_some_and(|parent| member(parent)) {
            if let Some((call, suffix)) = state_selection(node, source) {
                if let Some((store, method)) =
                    field(call, &["function"]).and_then(|callee| access_path(callee, source))
                {
                    if method == ".getState"
                        && stores.iter().filter(|s| s.name == store).count() == 1
                        && !script.state_binding_shadowed(node, &store, root, true)
                    {
                        out.emit(
                            node,
                            "Redux",
                            &format!("{}::{store}{suffix}", out.path),
                            true,
                            false,
                        );
                    } else if let Some(prefix) = method.strip_suffix(".getState") {
                        references::record(
                            bindings,
                            root,
                            node,
                            &store,
                            &format!("{prefix}{suffix}"),
                            Some("Redux"),
                            (true, false),
                            out,
                        );
                    }
                }
            }
        }
    }
}
