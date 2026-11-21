use super::*;
fn label(s: &str) -> String {
    s.trim()
        .lines()
        .next()
        .unwrap_or("")
        .split(" — ")
        .next()
        .unwrap_or(s)
        .chars()
        .take(130)
        .collect()
}
fn applies(tags: &str, path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let known: Vec<_> = tags
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|t| match t {
            "ts" | "typescript" => Some("ts"),
            "javascript" | "js" | "jquery" => Some("js"),
            "vbnet" | "vb" => Some("vb"),
            "csharp" => Some("cs"),
            "sql" => Some("sql"),
            "java" => Some("java"),
            "python" => Some("py"),
            "rust" => Some("rs"),
            "go" => Some("go"),
            "html" => Some("html"),
            "aspx" => Some("aspx"),
            _ => None,
        })
        .collect();
    known.is_empty()
        || known.iter().any(|t| {
            *t == ext
                || (*t == "js" && matches!(ext.as_str(), "jsx" | "ts" | "tsx"))
                || (*t == "ts" && ext == "tsx")
        })
}
fn sections(s: &str) -> Vec<(usize, &str, String)> {
    let mut blocks = Vec::new();
    let mut start = 1;
    let mut title = "";
    let mut block = String::new();
    let mut code = false;
    for (i, l) in s.lines().enumerate() {
        if !code && l.starts_with('#') {
            blocks.push((start, title, std::mem::take(&mut block)));
            start = i + 1;
            title = l
        }
        if l.trim_start().starts_with("```") {
            code = !code
        }
        block.push_str(l);
        block.push('\n')
    }
    blocks.push((start, title, block));
    blocks
}
fn fenced(section: &str) -> BTreeSet<Check> {
    let mut side = "";
    let mut code = false;
    let mut bad = String::new();
    let mut good = String::new();
    for l in section.lines().map(str::trim) {
        if l.starts_with("```") {
            code = !code;
            side = "";
            continue;
        }
        if !code {
            continue;
        }
        let marker = l
            .trim_start_matches(['\'', '/', '#', ' '])
            .to_ascii_lowercase();
        if marker.starts_with("wrong") || marker.starts_with("bad ") {
            side = "bad";
            continue;
        }
        if marker.starts_with("correct") || marker.starts_with("good ") {
            side = "good";
            continue;
        }
        if side == "good" {
            good.push_str(l);
            good.push('\n')
        } else if side == "bad" {
            bad.push_str(l);
            bad.push('\n')
        }
    }
    let mut required = BTreeSet::new();
    let heading = section.lines().next().unwrap_or("").to_lowercase();
    if heading.contains("required") || heading.contains("must") {
        let mut active = false;
        let mut statements: Vec<Shape> = Vec::new();
        for line in section.lines().chain([""]) {
            let line = line.trim();
            let delimiter = line.is_empty() || line.starts_with("//") || line.starts_with("```");
            if delimiter {
                if statements.len() == 2 && statements[0] != statements[1] {
                    required.insert(Check {
                        shape: statements[0].clone(),
                        companion: Some(statements[1].clone()),
                        in_loop: false,
                    });
                }
                statements.clear();
                if line.starts_with("```") {
                    active = !active
                }
            } else if active {
                let t = tokens(line, false);
                let shapes = expressions(&t);
                if shapes.len() == 1 && t.last().is_some_and(|v| v == ";") {
                    statements.push(shapes[0].clone())
                } else {
                    statements.clear()
                }
            }
        }
    }
    let mut checks = required;
    if !bad.is_empty() && !good.is_empty() {
        paired(
            section.lines().next().unwrap_or(""),
            &bad,
            &good,
            &mut checks,
        )
    }
    checks
}
pub(in super::super) fn audit(
    engine: &HybridSearchEngine,
    project: &str,
    files: &[DiffFile],
    verifier: &mut SourceVerifier<'_>,
    root: &Path,
    builtin: &[super::super::diff_checks::LineFinding],
    offset: Option<usize>,
) -> Report {
    let mut report = Report::default();
    let mut docs = match engine.quality_gate_documents(project) {
        Ok(d) => d,
        Err(e) => {
            report.summary = format!("Rule inventory unavailable: {e}\n");
            return report;
        }
    };
    docs.sort_by(|a, b| a.0.cmp(&b.0));
    docs.dedup_by(|a, b| a.0 == b.0);
    let total = docs.len();
    let mut inventory = Vec::new();
    let mut derived = 0;
    let mut observed = 0;
    let mut guides = 0;
    let inputs: Vec<_> = files.iter().map(Input::new).collect();
    let mut matched = BTreeMap::new();
    let mut run = |checks: &BTreeSet<Check>,
                   tags: &str,
                   scope: Option<&str>,
                   src: &str,
                   title: &str,
                   status: &str| {
        for input in &inputs {
            let path = &input.file.path;
            if !applies(tags, path)
                || scope.is_some_and(|p| !crate::utils::files::pattern_match(path, p))
            {
                continue;
            }
            for c in checks {
                for n in input.hits(c) {
                    matched
                        .entry((path.clone(), n, title.to_owned()))
                        .or_insert_with(|| {
                            format!("- {path}:{n} [source pattern] {title} — {src} ({status})")
                        });
                }
            }
        }
    };
    for (id, path, content) in docs {
        let body = if content.starts_with("Quality-rule source:") {
            content.split_once("\n\n").map_or("", |(_, b)| b)
        } else {
            &content
        };
        let scope = content
            .lines()
            .take(3)
            .find_map(|l| l.strip_prefix("Repository-rule pattern: "))
            .filter(|s| *s != "none supplied");
        let status = verifier.document_status(&id, &content);
        let title = label(body);
        let checks = compile(body);
        let blocked = status.starts_with("retired:")
            || status.starts_with("stale:")
            || status.starts_with("unavailable:")
            || (content.starts_with("Quality-rule source:") && status.starts_with("unverified:"));
        let class = if blocked {
            "manual: source stale or unavailable"
        } else if !checks.is_empty() {
            derived += 1;
            let tags = TAGS
                .captures(body)
                .map(|c| c[1].to_lowercase())
                .unwrap_or_default();
            run(
                &checks,
                &tags,
                scope,
                &format!("{path} #{id}"),
                &title,
                if status.starts_with("current:") {
                    "current policy"
                } else {
                    "advisory example"
                },
            );
            "example check"
        } else if (BAD
            .find(body)
            .and_then(|b| GOOD.find_at(body, b.end()).map(|g| (b, g)))
            .is_some_and(|(b, g)| {
                super::super::diff_checks::example_support(
                    body,
                    &body[b.end()..g.start()],
                    &body[g.end()..],
                )
            }))
            || builtin.iter().any(|f| f.source.contains(&id))
        {
            observed += 1;
            "built-in check evidenced"
        } else {
            "manual: no derived check"
        };
        inventory.push(format!("{class} | {id} | {path} | {title}"));
    }
    for file in ["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md"] {
        let Ok(source) = engram_core::safe_read_to_string(root, file) else {
            continue;
        };
        for (base, title, section) in sections(&source) {
            let tags = title.to_lowercase();
            let mut table = false;
            let mut fence = false;
            for (i, line) in section.lines().enumerate() {
                let l = line.trim();
                let lower = l.to_lowercase();
                if l.starts_with("```") {
                    fence = !fence;
                    continue;
                }
                if fence {
                    continue;
                }
                if lower.starts_with("| never do")
                    || lower.starts_with("| bad ")
                    || lower.starts_with("| avoid ")
                {
                    table = true;
                    continue;
                }
                if !l.starts_with('|') {
                    table = false
                }
                if (l.starts_with('|') && !table) || l.starts_with("- \"") {
                    continue;
                }
                let mut c = BTreeSet::new();
                directives(l, &mut c);
                guides += c.len();
                run(
                    &c,
                    &tags,
                    None,
                    &format!("{file}:{}", base + i),
                    &label(l.trim_matches('|').split('|').next().unwrap_or(l)),
                    "working checkout",
                );
            }
            let c = fenced(&section);
            guides += c.len();
            run(
                &c,
                &tags,
                None,
                &format!("{file}:{base}"),
                &label(title),
                "working checkout",
            );
        }
    }
    report.summary = format!(
        "## Rule coverage\n{total} indexed documents: {derived} have executable example checks; {observed} additional sources matched a built-in check on their contrasting examples or this diff; {} have no identified executable coverage here. Partial patterns do not verify an entire rule. {guides} additional checks from checkout guidance.\nInventory: pre_push_audit(project_id=\"{project}\", coverage_offset=0) lists every ID and coverage status.\n\n",
        total - derived - observed
    );
    if !matched.is_empty() {
        report
            .matches
            .push_str("## Source-derived matches on added lines\n");
        for s in matched.into_values() {
            let _ = writeln!(report.matches, "{s}");
        }
        report.matches.push('\n');
    }
    if let Some(start) = offset {
        let end = start.saturating_add(25).min(total);
        for s in inventory.into_iter().skip(start).take(25) {
            let _ = writeln!(report.inventory, "- {s}");
        }
        if end < total {
            let _ = writeln!(
                report.inventory,
                "Next: pre_push_audit(project_id=\"{project}\", coverage_offset={end})"
            );
        }
    }
    report
}
