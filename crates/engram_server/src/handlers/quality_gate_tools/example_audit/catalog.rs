use super::*;
fn label(s: &str) -> String {
    if let Some((_, detail)) = s.split_once(" — ") {
        if let Some((clause, _)) = detail.split_once(" | ") {
            return clause.trim().chars().take(130).collect();
        }
    }
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
            "javascript" | "js" | "es5" | "es6" | "jquery" => Some("js"),
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
fn example_result(
    body: &str,
    checks: &BTreeSet<Check>,
    contracts: &[structural::Contract],
    blocked: bool,
) -> String {
    if blocked {
        return "own example: skipped (inactive source)".into();
    }
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let examples = parsed
        .as_ref()
        .and_then(|v| Some((v["bad_example"].as_str()?, v["good_example"].as_str()?)))
        .or_else(|| {
            let b = BAD.find(body)?;
            let g = GOOD.find_at(body, b.end())?;
            Some((
                body[b.end()..g.start()]
                    .trim()
                    .trim_end_matches('—')
                    .trim_end(),
                &body[g.end()..],
            ))
        });
    let Some((bad, good)) = examples else {
        return "own example: no Bad/Good pair".into();
    };
    let tags = TAGS
        .captures(body)
        .map(|c| c[1].to_lowercase())
        .unwrap_or_default();
    let test = |text: &str| {
        let text = TAGS.replace(text, "");
        let rows = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.to_owned()))
            .collect::<Vec<_>>();
        let mut hits = super::super::diff_checks::builtin_example_hits(body, &text)
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for path in [
            "example.ts",
            "example.js",
            "example.vb",
            "example.sql",
            "example.cs",
            "example.html",
            "example.aspx",
            "example.py",
            "example.go",
            "example.rs",
            "example.java",
        ] {
            if !applies(&tags, path) {
                continue;
            }
            let input = Input::from_lines(path, &rows);
            for c in checks {
                if !input.hits(c).is_empty() {
                    hits.insert(c.describe());
                }
            }
            if !contracts.is_empty() {
                let h =
                    structural::Hunk::new(path, rows.iter().map(|(n, l)| (*n, true, l.clone())));
                for c in contracts {
                    if !h.hits(c).is_empty() {
                        hits.insert(c.description());
                    }
                }
            }
        }
        hits
    };
    let b = test(bad);
    let g = test(good);
    let mut summary = format!(
        "own Bad: {}; own Good: {}",
        if b.is_empty() { "missed" } else { "detected" },
        if g.is_empty() { "quiet" } else { "flagged" }
    );
    if b.is_empty() {
        // Some imported records flatten multiple snippets into a single line
        // comment. Preserve the literal outcome and label the separate replay.
        let fragment = bad
            .lines()
            .filter_map(|l| l.split_once("//"))
            .flat_map(|(_, comment)| comment.split_once(';').map(|(_, tail)| tail.trim()))
            .any(|tail| !test(tail).is_empty());
        if fragment {
            summary.push_str("; embedded comment fragment: detected");
        }
    }
    summary
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
    let mut structural_only = 0;
    let mut guides = 0;
    let mut matched_prose = 0;
    let inputs: Vec<_> = files.iter().map(Input::new).collect();
    let hunks: Vec<_> = files.iter().map(structural::Hunk::file).collect();
    let mut matched = BTreeMap::new();
    let mut run = |checks: &BTreeSet<Check>,
                   contracts: &[structural::Contract],
                   tags: &str,
                   scope: Option<&str>,
                   src: &str,
                   title: &str,
                   status: &str| {
        for (input, hunks) in inputs.iter().zip(&hunks) {
            let path = input.path;
            if !applies(tags, path)
                || scope.is_some_and(|p| !crate::utils::files::pattern_match(path, p))
            {
                continue;
            }
            let lexical = checks
                .iter()
                .flat_map(|c| input.hits(c).into_iter().map(move |n| (n, c.describe())));
            let structural = contracts.iter().flat_map(|c| {
                hunks
                    .iter()
                    .flat_map(move |h| h.hits(c).into_iter().map(move |n| (n, c.description())))
            });
            for (n, pattern) in lexical.chain(structural) {
                let row = format!(
                    "- {path}:{n} [source pattern] {pattern}. {title}. Source: {src} ({status})"
                );
                let key = (path.to_owned(), n, pattern);
                if status == "working checkout" && !src.contains("mirror of ") {
                    matched.insert(key, row);
                } else {
                    matched.entry(key).or_insert(row);
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
        let status = verifier.document_status_at(&path,&id, &content);
        let title = label(body);
        let checks = compile(body);
        let contracts = structural::contracts(body);
        let blocked = status.starts_with("retired:")
            || status.starts_with("stale:")
            || status.starts_with("unavailable:")
            || (content.starts_with("Quality-rule source:") && status.starts_with("unverified:"));
        let class = if blocked {
            "manual: source stale or unavailable"
        } else if !checks.is_empty() || !contracts.is_empty() {
            if checks.is_empty() {
                structural_only += 1;
            } else {
                derived += 1;
            }
            let tags = TAGS
                .captures(body)
                .map(|c| c[1].to_lowercase())
                .unwrap_or_default();
            run(
                &checks,
                &contracts,
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
            if checks.is_empty() {
                "structural policy check"
            } else {
                "example check"
            }
        } else if BAD
            .find(body)
            .and_then(|b| GOOD.find_at(body, b.end()).map(|g| (b, g)))
            .is_some_and(|(b, g)| {
                super::super::diff_checks::example_support(
                    body,
                    &body[b.end()..g.start()],
                    &body[g.end()..],
                )
            })
        {
            observed += 1;
            "built-in check evidenced"
        } else {
            if builtin.iter().any(|f| f.source.contains(&id)) {
                matched_prose += 1;
            }
            "manual: no derived check"
        };
        let test = if offset
            .is_some_and(|start| (start..start.saturating_add(25)).contains(&inventory.len()))
        {
            example_result(body, &checks, &contracts, blocked)
        } else {
            String::new()
        };
        inventory.push(format!("{class} | {id} | {path} | {title} | {test}"));
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
                let contracts = structural::contracts(l);
                guides += c.len() + contracts.len();
                let mut citation = format!("{file}:{}", base + i);
                if let Some(pos) = l.rfind("(qg-") {
                    citation.push_str(" mirror of ");
                    citation.push_str(l[pos + 1..].trim_end_matches(')'));
                }
                run(
                    &c,
                    &contracts,
                    &tags,
                    None,
                    &citation,
                    &label(l.trim_matches('|').split('|').next().unwrap_or(l)),
                    "working checkout",
                );
            }
            let c = fenced(&section);
            guides += c.len();
            run(
                &c,
                &[],
                &tags,
                None,
                &format!("{file}:{base}"),
                &label(title),
                "working checkout",
            );
        }
    }
    for (file,hunks) in files.iter().zip(&hunks){
        let rules=crate::services::written_casing::for_file(root,&file.path);
        if let Some((rule,upper))=crate::services::written_casing::public_rule(&rules){
            let c=structural::Contract::PublicMemberCase(upper);
            for n in hunks.iter().flat_map(|h|h.hits(&c)){
                let desc=c.description();let path=&file.path;
                matched.insert((path.clone(),n,desc.clone()),format!("- {path}:{n} [source pattern] {desc}. {}. Source: {} (working checkout)",rule.text,rule.source));
            }
        }
    }
    report.summary = format!(
        "## Rule coverage\n{total} indexed documents: {derived} have executable example checks; {observed} additional documents reproduce a built-in check on their contrasting examples; {structural_only} additional documents enable structural checks; {} require manual review. Of these, {matched_prose} also supplied evidence for a built-in finding on this diff. Checks cover individual conditions. {guides} additional checks from checkout guidance.\nInventory: pre_push_audit(project_id=\"{project}\", coverage_offset=0) lists every ID with its own Bad/Good replay result or missing-example status.\n\n",
        total - derived - observed - structural_only
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
