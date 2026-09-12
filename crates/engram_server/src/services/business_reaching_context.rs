//! Bounded syntactic prerequisites, never call binding or control-flow proof.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

const MAX_CALLS: usize = 32;
const MAX_RUNS: usize = 16;
const MAX_EXPRESSION_BYTES: usize = 384;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReachingContext {
    pub version: String,
    pub coverage_status: String,
    pub outcome_status: String,
    pub scope: String,
    pub language: String,
    pub runs: Vec<Vec<CallStatement>>,
    pub unsupported_boundaries: usize,
    pub omitted_calls: usize,
    #[serde(default)]
    pub unexamined_tail_start_line: Option<u32>,
    #[serde(default)]
    pub unexamined_tail_lines: usize,
    pub omissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallStatement {
    pub line: u32,
    /// Exact supplied call text, not a resolved method identity.
    pub expression: String,
}

#[derive(Debug, Clone)]
pub struct ReachingQualification {
    pub has_prerequisites: bool,
    pub summary: String,
}

fn finish(run: &mut Vec<CallStatement>, context: &mut ReachingContext) {
    if run.len() > 1 {
        if context.runs.len() < MAX_RUNS {
            context.runs.push(std::mem::take(run));
        } else {
            context.omitted_calls += run.len();
        }
    }
    run.clear();
}

fn stop_tail(
    context: &mut ReachingContext,
    start_line: u32,
    offset: usize,
    total: usize,
    reason: &str,
) {
    context.unsupported_boundaries += 1;
    context.unexamined_tail_start_line = Some(start_line + offset as u32);
    context.unexamined_tail_lines = total.saturating_sub(offset);
    context.omissions.push(format!("Analyzed prefix only: {reason} at line {}; this line and {} remaining physical lines are unexamined.", start_line + offset as u32, context.unexamined_tail_lines));
}

pub fn collect(body: &str, language: &str, start_line: u32) -> ReachingContext {
    let mut context = ReachingContext {
        version: "straight-line-reaching-context-v1".into(),
        coverage_status: "incomplete".into(),
        outcome_status: "normal_completion_unverified".into(),
        scope: "incomplete: adjacent synchronous standalone/simple-assignment calls only; recognized statement runs are syntactic, not compiler binding or branch dominance; every later call requires preceding statements in its run to complete normally if that run is reached; earlier exit gates, enclosing predicates, argument/property effects, helper internals and unsupported boundaries are unexamined; no semantic proof".into(),
        language: language.into(), runs: vec![], unsupported_boundaries: 0, omitted_calls: 0,
        unexamined_tail_start_line: None, unexamined_tail_lines: 0,
        omissions: vec![format!("Limits: {MAX_CALLS} recognized calls, {MAX_RUNS} runs, {MAX_EXPRESSION_BYTES} UTF-8 bytes per expression; only complete physical-line statements.")],
    };
    let vb = language == "vb";
    if !vb && !matches!(language, "cs" | "csharp") {
        context
            .omissions
            .push("Unsupported language; reaching coverage unknown.".into());
        return context;
    }
    let masked = super::business_outcome_dependencies::executable_lines(body, vb);
    // Unstructured error handling can resume past a failed call, invalidating
    // even an earlier lexical sequence. Decline this entire member.
    static UNSTRUCTURED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)\bOn\s+Error\b|(?:^|:)\s*Resume\b|\bThen\s+Resume\b")
            .expect("unstructured VB flow")
    });
    if vb && UNSTRUCTURED.is_match(&masked) {
        context.omissions.push("VB On Error/Resume detected; whole member declined because later calls may follow failed earlier calls.".into());
        context.unsupported_boundaries = 1;
        context.unexamined_tail_start_line = Some(start_line);
        context.unexamined_tail_lines = body.lines().count();
        return context;
    }
    // A jump may enter a later statement without earlier normal completion.
    // Do not attempt to reconstruct labels, targets or backward edges.
    static VB_JUMP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bGoTo\b").expect("VB jump"));
    static CS_JUMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bgoto\b").expect("C# jump"));
    if (vb && VB_JUMP.is_match(&masked)) || (!vb && CS_JUMP.is_match(&masked)) {
        context.omissions.push("Unstructured jump detected; whole member declined because label entry and backward flow are not analyzed.".into());
        context.unsupported_boundaries = 1;
        context.unexamined_tail_start_line = Some(start_line);
        context.unexamined_tail_lines = body.lines().count();
        return context;
    }
    static LABEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:[A-Za-z_]\w*|[0-9]+)\s*:(?:[^:=]|$)").expect("label boundary")
    });
    static UNSUPPORTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:await|async|yield|delegate)\b|=>|\b(?:Function|Sub)\s*\(|^\s*(?:For\b|While\b|Do\b|Select\s+Case\b|switch\s*\(|foreach\s*\(|for\s*\(|while\s*\()").expect("unsupported control")
    });
    static CS_CONDITIONAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\s*(?:}\s*)?(?:if|else)\b").expect("C# conditional"));
    static CS_DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:(?:public|private|protected|internal|static|virtual|override|sealed|unsafe|new)\s+)*(?:[A-Za-z_]\w*(?:[.<>,?\[\]A-Za-z_0-9]*)?)\s+[A-Za-z_]\w*\s*\([^;]*\)\s*(?:\{|$)").expect("C# declaration boundary")
    });
    static CALL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:(?:Call\s+)|(?:(?:Dim\s+)?[A-Za-z_]\w*(?:\s+As\s+[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*|\s+[A-Za-z_]\w*)?\s*=\s*))?(?P<call>(?:global::)?[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*\s*\([^()]*\))\s*;?\s*$").expect("simple synchronous call")
    });
    static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:[{}]|(?:Public |Private |Protected |Friend |Shared |Static |Override |Overrides )*(?:Sub|Function)\b.*|End\s+(?:Sub|Function|If|Try)\b|If\b.*\bThen\s*|ElseIf\b.*\bThen\s*|Else|Try|Catch\b.*|Finally|(?:if|else\s+if)\s*\(.*\)\s*\{?|else\s*\{?|try\s*\{?|catch\b.*\{?|finally\s*\{?|(?:return|throw)\b.*)\s*$").expect("run boundary")
    });
    // Do not recognize VB declaration words as C# block boundaries: a C#
    // local function can have a user-defined return type named Function/Sub.
    static CS_BOUNDARY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:[{}]|(?:if|else\s+if)\s*\(.*\)\s*\{?|else\s*\{?|try\s*\{?|catch\b.*\{?|finally\s*\{?|(?:return|throw)\b.*)\s*$").expect("C# run boundary")
    });
    let mut run = Vec::new();
    let mut count = 0;
    let mut continued_depth: i64 = 0;
    let masked_lines: Vec<_> = masked.lines().collect();
    let total_lines = body.lines().count();
    for (offset, (line, raw)) in masked_lines.iter().copied().zip(body.lines()).enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let next = masked_lines[offset + 1..]
            .iter()
            .copied()
            .find(|l| !l.trim().is_empty())
            .map(str::trim);
        let boundary = if vb {
            BOUNDARY.is_match(line)
        } else {
            CS_BOUNDARY.is_match(line)
        };
        let unbraced = !vb
            && CS_CONDITIONAL.is_match(line)
            && !line.trim_end().ends_with('{')
            && next != Some("{");
        let declaration = !vb && CS_DECLARATION.is_match(line);
        let unsupported_block =
            !vb && line.trim_end().ends_with('{') && !boundary && !(declaration && offset == 0);
        if UNSUPPORTED.is_match(line)
            || LABEL.is_match(line)
            || unbraced
            || (declaration && offset > 0)
            || unsupported_block
        {
            finish(&mut run, &mut context);
            stop_tail(
                &mut context,
                start_line,
                offset,
                total_lines,
                "unsupported deferred/local-function/control or label boundary",
            );
            break;
        }
        if declaration && offset == 0 {
            finish(&mut run, &mut context);
            continue;
        }
        let depth_delta = line.bytes().filter(|b| matches!(b, b'(' | b'[')).count() as i64
            - line.bytes().filter(|b| matches!(b, b')' | b']')).count() as i64;
        if continued_depth > 0 || depth_delta != 0 {
            finish(&mut run, &mut context);
            if !vb {
                stop_tail(
                    &mut context,
                    start_line,
                    offset,
                    total_lines,
                    "uncertain multiline C# statement/declaration",
                );
                break;
            }
            context.unsupported_boundaries += 1;
            continued_depth = (continued_depth + depth_delta).max(0);
            continue;
        }
        if boundary {
            finish(&mut run, &mut context);
            continue;
        }
        let capture = CALL
            .captures(line)
            .filter(|_| vb || line.trim_end().ends_with(';'));
        let Some(call) = capture.as_ref().and_then(|c| c.name("call")) else {
            finish(&mut run, &mut context);
            // Unknown syntax may start a generic/multiline local function whose
            // next bare brace would otherwise admit its deferred body as caller
            // flow. Stop conservatively, including ordinary unsupported C#.
            if !vb {
                stop_tail(
                    &mut context,
                    start_line,
                    offset,
                    total_lines,
                    "uncertain C# statement/declaration",
                );
                break;
            }
            context.unsupported_boundaries += 1;
            continue;
        };
        // This scanner deliberately excludes constructors, nested calls and
        // multiline argument evaluation rather than inventing their order.
        let expression = &raw[call.start()..call.end()];
        if expression.len() > MAX_EXPRESSION_BYTES || count >= MAX_CALLS {
            finish(&mut run, &mut context);
            context.omitted_calls += 1;
            continue;
        }
        count += 1;
        run.push(CallStatement {
            line: start_line + offset as u32,
            expression: expression.into(),
        });
    }
    finish(&mut run, &mut context);
    context
}

/// Extraction gets all bounded run statements, unlike the short retrieval
/// summary. Ordered runs encode earlier-call prerequisites without quadratic
/// repetition of every earlier expression for every later statement.
pub fn prompt_context(context: Option<&ReachingContext>) -> String {
    let Some(context) = context else {
        return qualify(None, None).summary;
    };
    let mut text = format!(
        "\nStraight-line reaching evidence: {}; {}. {}\nWithin each ordered run, statement i conditionally requires ALL preceding statements 1..i-1 to complete normally if execution reaches this run. Call expressions are supplied lexical text, not verified binding/invocation/outcome. Unsupported boundaries {}, omitted calls {}, unexamined tail start {:?}, tail physical lines {}.\n",
        context.coverage_status,
        context.outcome_status,
        context.scope,
        context.unsupported_boundaries,
        context.omitted_calls,
        context.unexamined_tail_start_line,
        context.unexamined_tail_lines
    );
    let mut supplied = 0usize;
    let mut omitted = 0usize;
    for (run_index, run) in context.runs.iter().enumerate() {
        for (index, statement) in run.iter().enumerate() {
            if run_index >= MAX_RUNS
                || supplied >= MAX_CALLS
                || statement.expression.len() > MAX_EXPRESSION_BYTES
            {
                omitted += 1;
                continue;
            }
            supplied += 1;
            // JSON quoting preserves the exact expression and distinguishes
            // quotes inside arguments from prompt framing.
            text.push_str(&format!("Run {}, statement {}, line {}: {}; prior_statement_count={}, prior_statement_positions=1..{}; normal_completion_unverified.\n",run_index+1,index+1,statement.line,serde_json::to_string(&statement.expression).expect("expression serialization"),index,index));
        }
    }
    text.push_str(&format!("Prompt run statements supplied {supplied}; additionally omitted {omitted}. Limits {MAX_RUNS} runs, {MAX_CALLS} expressions, {MAX_EXPRESSION_BYTES} raw UTF-8 bytes/expression; JSON escaping can expand representation. No later-tail analysis.\n"));
    text
}

fn lexical_contains(rule: &str, expression: &str, vb: bool) -> bool {
    let rule = if vb {
        rule.to_ascii_lowercase()
    } else {
        rule.into()
    };
    let expression = if vb {
        expression.to_ascii_lowercase()
    } else {
        expression.into()
    };
    rule.match_indices(&expression).any(|(start, _)| {
        let before = rule[..start].chars().next_back();
        let after = rule[start + expression.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'))
            && !after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
    })
}

/// None means document-level retrieval; Some(rule) limits association to its
/// anchor or exact supplied-expression occurrence. Neither proves invocation.
pub fn qualify(context: Option<&ReachingContext>, rule: Option<&str>) -> ReachingQualification {
    let Some(context) = context else {
        return ReachingQualification { has_prerequisites: false, summary: "Reaching context: legacy_coverage_unknown; no recorded straight-line prerequisites. Raw inference is not verified.".into() };
    };
    static ANCHOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[line (\d+)\]").expect("rule anchor"));
    let anchor = rule
        .and_then(|r| ANCHOR.captures(r))
        .and_then(|c| c[1].parse::<u32>().ok());
    let mut matches: usize = 0;
    let mut shown = Vec::new();
    for run in &context.runs {
        for (i, statement) in run.iter().enumerate().skip(1) {
            let association = match rule {
                None => "document_record",
                Some(_) if anchor == Some(statement.line) => "anchor_line_conditional",
                Some(rule)
                    if lexical_contains(rule, &statement.expression, context.language == "vb") =>
                {
                    "exact_expression_lexical_conditional"
                }
                _ => continue,
            };
            matches += 1;
            if shown.len() < 4 {
                let mut end = statement.expression.len().min(160);
                while !statement.expression.is_char_boundary(end) {
                    end -= 1;
                }
                let previous: Vec<_> = run[..i]
                    .iter()
                    .take(4)
                    .map(|s| s.line.to_string())
                    .collect();
                shown.push(format!("line {} {}{} requires normal completion of prior call statements at lines {} ({} earlier statements, {} more line references omitted); association={association}; normal_completion_unverified",statement.line,&statement.expression[..end],if end < statement.expression.len() {" [shortened]"} else {""},previous.join(","),i,i.saturating_sub(4)));
            }
        }
    }
    ReachingQualification {
        has_prerequisites: matches > 0,
        summary: format!(
            "Reaching context: {}; incomplete syntactic coverage, not invocation/binding/branch-dominance proof. Matched {matches}, shown {}, omitted {}; unsupported boundaries {}, omitted calls {}; unexamined tail start {:?}, tail physical lines {}. {} Full structured runs/scope: get_chunk(namespace=\"business_logic\") using this document ID. Earlier exit gates and enclosing conditions remain unexamined.",
            if matches > 0 {
                "prerequisites_require_review"
            } else {
                "no_matched_prerequisites_not_verified"
            },
            shown.len(),
            matches.saturating_sub(shown.len()),
            context.unsupported_boundaries,
            context.omitted_calls,
            context.unexamined_tail_start_line,
            context.unexamined_tail_lines,
            shown.join("; ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_csharp_declarations_keep_only_supported_prefix() {
        for declaration in [
            "void Local<T>()\n {\n HiddenA();\n HiddenB();\n }",
            "void Local(\n int x)\n {\n HiddenA();\n HiddenB();\n }",
            "Function Local<T>()\n {\n HiddenA();\n HiddenB();\n }",
            "(int, int) Local()\n {\n HiddenA();\n HiddenB();\n }",
        ] {
            let source =
                format!("void Outer() {{\n A();\n B();\n {declaration}\n Later();\n Last();\n}}");
            let context = collect(&source, "cs", 10);
            assert_eq!(context.runs.len(), 1, "{source}");
            assert_eq!(
                context.runs[0]
                    .iter()
                    .map(|s| s.expression.as_str())
                    .collect::<Vec<_>>(),
                vec!["A()", "B()"],
                "{source}"
            );
            assert_eq!(context.unexamined_tail_start_line, Some(13), "{source}");
            assert_eq!(context.unexamined_tail_lines, source.lines().count() - 3);
            assert!(!qualify(Some(&context), Some("HiddenB()")).has_prerequisites);
            assert!(!qualify(Some(&context), Some("Last()")).has_prerequisites);
            // No prefix: even a generic/multiline nested body alone is declined.
            let no_prefix = format!("void Outer() {{\n {declaration}\n}}");
            assert!(collect(&no_prefix, "cs", 1).runs.is_empty());
        }
        let source = "void Outer() {\n A();\n B();\n int ordinary = 1;\n C();\n D();\n}";
        let context = collect(source, "cs", 1);
        assert_eq!(context.runs.len(), 1);
        assert_eq!(context.unexamined_tail_start_line, Some(4));
    }

    #[test]
    fn jumps_decline_member_and_labels_cannot_join_runs() {
        for (source, language) in [
            ("A();\nB();\ngoto target;\nC();\ntarget:\nD();\nE();", "cs"),
            ("A()\nB()\nGoTo target\nC()\ntarget:\nD()\nE()", "vb"),
            ("target:\nA();\nB();\ngoto target;", "cs"),
            ("target:\nA()\nB()\nGoTo target", "vb"),
        ] {
            let context = collect(source, language, 7);
            assert!(context.runs.is_empty());
            assert_eq!(context.unexamined_tail_start_line, Some(7));
            assert!(
                context
                    .omissions
                    .iter()
                    .any(|s| s.contains("Unstructured jump"))
            );
        }
        for (source, language) in [
            ("A();\nB();\ntarget:\nC();\nD();", "cs"),
            ("A()\nB()\ntarget: C()\nD()", "vb"),
        ] {
            let context = collect(source, language, 1);
            assert_eq!(context.runs.len(), 1);
            assert_eq!(context.runs[0].len(), 2);
            assert_eq!(context.unexamined_tail_start_line, Some(3));
        }
        // Comment/string mentions and global:: qualification are not jumps/labels.
        for (source, language) in [
            (
                "A(\"goto target\");\n// goto hidden;\nglobal::Helpers.B();",
                "cs",
            ),
            ("A(\"GoTo target\")\n' GoTo hidden\nB()", "vb"),
        ] {
            let context = collect(source, language, 1);
            assert_eq!(context.runs[0].len(), 2);
            assert_eq!(context.unexamined_tail_start_line, None);
        }
    }

    #[test]
    fn unsupported_flow_cannot_borrow_nested_or_conditional_calls() {
        for (source, language) in [
            ("if (flag)\n A();\n B();", "cs"),
            ("if (flag) {\n A();\n}\nelse\n B();\n C();", "cs"),
            ("On Error Resume Next\nA()\nB()", "vb"),
            ("A()\nB()\nOn Error GoTo handler\nResume Next", "vb"),
            ("Dim f = Sub()\n A()\n B()\nEnd Sub", "vb"),
            ("var f = delegate() {\n A();\n B();\n};", "cs"),
            ("void Outer() {\n void Local() {\n A();\n B();\n }\n}", "cs"),
        ] {
            let context = collect(source, language, 1);
            assert!(context.runs.is_empty(), "{source}");
            assert!(context.unexamined_tail_start_line.is_some(), "{source}");
        }
        // Supported braced control remains distinct from the unbraced negative.
        let good = collect("if (flag)\n{\n A();\n B();\n}", "cs", 1);
        assert_eq!(good.runs[0].len(), 2);
    }

    #[test]
    fn validator_and_builder_prefix_survives_later_projection_lambda() {
        // Generic structural equivalent of the reviewed recorded member:
        // early multiline input/permission guards, five validators, two
        // assignment calls, then a projection lambda and a later catch.
        let source = "Function ReadAll(query As Query) As Object\nIf query Is Nothing OrElse\n (query.id < 1 AndAlso\n  (Not query.otherId.HasValue OrElse query.otherId.Value < 1)) Then\n Reject()\n Return Nothing\nEnd If\nIf Not CanRead(query) Then\n Deny()\n Return Nothing\nEnd If\nTry\n ValidateDate(query.first, \"first\")\n ValidateDate(query.second, \"second\")\n ValidateDate(query.third, \"third\")\n ValidateDate(query.fourth, \"fourth\")\n ValidateId(query.user, \"user\")\n Dim filter = Build(query)\n Dim data = Fetch(filter)\n Return data.Select(Function(item) Map(item, filter)).ToList()\nCatch ex As Exception\n Log(ex)\n Respond()\nEnd Try\nEnd Function";
        let context = collect(source, "vb", 1);
        assert_eq!(context.runs.len(), 1);
        assert_eq!(context.runs[0].len(), 7);
        assert_eq!(context.runs[0][0].line, 13);
        assert_eq!(context.runs[0][6].expression, "Fetch(filter)");
        assert_eq!(context.unexamined_tail_start_line, Some(20));
        assert_eq!(context.unexamined_tail_lines, 6);
        let mut evidence = super::super::business_outcome_dependencies::collect(
            source,
            "Reader",
            "vb",
            1,
            "Reader.vb",
            None,
        );
        evidence.reaching_context = Some(context.clone());
        let prompt = evidence.prompt_context();
        assert!(prompt.contains("Run 1, statement 7, line 19: \"Fetch(filter)\"; prior_statement_count=6, prior_statement_positions=1..6"), "{prompt}");
        for statement in &context.runs[0] {
            assert!(
                prompt.contains(&serde_json::to_string(&statement.expression).unwrap()),
                "{prompt}"
            );
        }
        assert!(prompt.contains("Prompt run statements supplied 7; additionally omitted 0"));
        assert!(prompt.contains("unexamined tail start Some(20)"));
        let grouped = qualify(
            Some(&context),
            Some(
                "ValidateDate(query.first, \"first\") and ValidateDate(query.second, \"second\") [line 13]",
            ),
        );
        assert!(grouped.has_prerequisites);
        assert!(
            grouped
                .summary
                .contains("exact_expression_lexical_conditional")
        );
        let fetch = qualify(Some(&context), Some("Fetch(filter) [line 19]"));
        assert!(fetch.has_prerequisites);
        assert!(fetch.summary.contains("6 earlier statements"));
        assert!(!qualify(Some(&context), Some("Respond() [line 23]")).has_prerequisites);
        assert!(
            context
                .runs
                .iter()
                .flatten()
                .all(|s| !s.expression.contains("Map(") && !s.expression.contains("Log("))
        );
    }
    #[test]
    fn ordered_calls_and_assignment_builder_retain_prerequisites() {
        let c = collect(
            "ValidateA()\nValidateB()\nDim filter = Build(query)\nFetch(filter)",
            "vb",
            10,
        );
        assert_eq!(c.runs[0].len(), 4);
        let q = qualify(Some(&c), Some("fetch [line 13]"));
        assert!(q.has_prerequisites);
        assert!(q.summary.contains("10,11,12"));
        assert!(!qualify(Some(&c), Some("first [line 10]")).has_prerequisites);
    }
    #[test]
    fn grouped_first_anchor_matches_only_exact_later_expressions() {
        let c = collect("Validate(a)\nValidate(b)\nSave()", "vb", 20);
        let q = qualify(Some(&c), Some("Validate(a) and Validate(b) [line 20]"));
        assert!(q.has_prerequisites);
        assert!(q.summary.contains("exact_expression_lexical_conditional"));
        assert!(!qualify(Some(&c), Some("OtherValidate(b) [line 20]")).has_prerequisites);
        assert!(!qualify(Some(&c), Some("unrelated [line 80]")).has_prerequisites);
    }
    #[test]
    fn branches_try_and_catch_never_share_runs() {
        let c = collect(
            "Try\nValidate()\nFetch()\nCatch ex As Exception\nLog(ex)\nRespond()\nEnd Try",
            "vb",
            1,
        );
        assert_eq!(c.runs.len(), 2);
        let q = qualify(Some(&c), Some("Respond() [line 6]"));
        assert!(q.summary.contains("lines 5"));
        assert!(!q.summary.contains("lines 2"));
        let c = collect(
            "if (x) {\nFirst();\nSecond();\n}\nelse {\nThird();\nFourth();\n}",
            "cs",
            1,
        );
        assert_eq!(c.runs.len(), 2);
        assert_eq!(c.runs[1][0].expression, "Third()");
    }
    #[test]
    fn literals_nested_calls_and_deferred_execution_do_not_invent_runs() {
        for (s, lang) in [
            (
                "First();\nvar text = \"Fake(); Return();\";\nSecond();",
                "cs",
            ),
            ("First()\nDim text = \"Fake()\"\nSecond()", "vb"),
            ("First();\nOuter(Inner());\nSecond();", "cs"),
            ("Outer(\nFirst(),\nSecond()\n);", "cs"),
            ("First();\nawait Second();\nThird();", "cs"),
            ("Dim f = Function()\nFirst()\nSecond()\nEnd Function", "vb"),
        ] {
            assert!(collect(s, lang, 1).runs.is_empty(), "{s}");
        }
        let c = collect("First();\n/* Fake();\n Return(); */\nSecond();", "cs", 1);
        assert_eq!(c.runs[0].len(), 2);
        assert_eq!(c.runs[0][1].line, 4);
    }
    #[test]
    fn bounds_legacy_and_fingerprints_remain_explicit() {
        let body = (0..50).map(|i| format!("Call{i}()\n")).collect::<String>();
        let c = collect(&body, "vb", 1);
        assert!(c.omitted_calls > 0);
        let q = qualify(Some(&c), None);
        assert!(q.summary.contains("shown 4"));
        assert!(q.summary.contains("omitted 27"));
        assert!(q.summary.len() < 2000);
        assert!(
            qualify(None, None)
                .summary
                .contains("legacy_coverage_unknown")
        );
        let mut a = super::super::business_outcome_dependencies::collect(
            "A()\nB()", "C", "vb", 1, "C.vb", None,
        );
        a.fingerprint("A()\nB()", "test");
        let first = a.analysis_fingerprint.clone();
        a.reaching_context = Some(collect("A()\nB()", "vb", 2));
        a.fingerprint("A()\nB()", "test");
        assert_ne!(first, a.analysis_fingerprint);
    }
}
