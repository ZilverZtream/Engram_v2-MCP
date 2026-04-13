// Validation tests derived from public API contracts only.
// All assertions reference observable public types and functions.

use engram_index::chunking::{chunk_lines, semantic_chunk_lines};
use engram_index::control_mapping::lookup;
use engram_index::ingest::is_binary;
use engram_index::solution_parser::{
    ProjectType, classify_project_type, parse_solution, parse_solution_configs,
};
use engram_index::sql_parser::{SqlOp, analyze_sql, generate_method_name};
use engram_index::sync_hazard_detector::{HazardSeverity, detect_sync_hazards};
use engram_index::webforms::{candidate_codebehind_paths, is_webforms_markup};
use engram_index::{
    ConfidenceBand, SymbolExtractor, chunk_id_from_hash, escape_tantivy_literal,
    score_control_binding, score_event_wiring, score_sql_trace,
};
use std::io::Write;
use std::path::Path;

// ── ConfidenceBand ─────────────────────────────────────────────────────────────

#[test]
fn confidence_band_from_score_high_at_exactly_08() {
    assert_eq!(ConfidenceBand::from_score(0.8), ConfidenceBand::High);
}

#[test]
fn confidence_band_from_score_high_at_10() {
    assert_eq!(ConfidenceBand::from_score(1.0), ConfidenceBand::High);
}

#[test]
fn confidence_band_from_score_medium_below_08() {
    assert_eq!(ConfidenceBand::from_score(0.79), ConfidenceBand::Medium);
}

#[test]
fn confidence_band_from_score_medium_at_exactly_05() {
    assert_eq!(ConfidenceBand::from_score(0.5), ConfidenceBand::Medium);
}

#[test]
fn confidence_band_from_score_low_below_05() {
    assert_eq!(ConfidenceBand::from_score(0.49), ConfidenceBand::Low);
}

#[test]
fn confidence_band_from_score_low_at_zero() {
    assert_eq!(ConfidenceBand::from_score(0.0), ConfidenceBand::Low);
}

#[test]
fn confidence_band_display_strings() {
    assert_eq!(ConfidenceBand::High.to_string(), "high");
    assert_eq!(ConfidenceBand::Medium.to_string(), "medium");
    assert_eq!(ConfidenceBand::Low.to_string(), "low");
}

// ── score_event_wiring ─────────────────────────────────────────────────────────

#[test]
fn event_wiring_all_true_is_high() {
    let c = score_event_wiring(true, true, true, true, true);
    assert!(c.score >= 0.8, "expected high score, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::High);
}

#[test]
fn event_wiring_all_false_is_low() {
    let c = score_event_wiring(false, false, false, false, false);
    assert!(c.score < 0.5, "expected low score, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::Low);
}

#[test]
fn event_wiring_score_in_unit_range() {
    let cases = [
        (false, false, false, false, false),
        (true, false, false, false, false),
        (true, true, false, false, false),
        (true, true, true, false, false),
        (true, true, true, true, false),
        (true, true, true, true, true),
    ];
    for (a, b, c, d, e) in cases {
        let r = score_event_wiring(a, b, c, d, e);
        assert!(
            r.score >= 0.0 && r.score <= 1.0,
            "score {} out of [0,1]",
            r.score
        );
    }
}

#[test]
fn event_wiring_is_deterministic() {
    let c1 = score_event_wiring(true, false, true, false, true);
    let c2 = score_event_wiring(true, false, true, false, true);
    assert_eq!(c1.score, c2.score);
    assert_eq!(c1.band, c2.band);
}

#[test]
fn event_wiring_score_monotone_with_evidence() {
    let none = score_event_wiring(false, false, false, false, false);
    let some = score_event_wiring(true, true, false, false, false);
    let all = score_event_wiring(true, true, true, true, true);
    assert!(none.score < some.score);
    assert!(some.score < all.score);
}

#[test]
fn event_wiring_has_five_signals() {
    let c = score_event_wiring(true, true, true, true, true);
    assert_eq!(c.signals.len(), 5);
}

#[test]
fn event_wiring_signals_have_non_empty_names_and_evidence() {
    let c = score_event_wiring(true, false, true, false, false);
    for s in &c.signals {
        assert!(!s.name.is_empty(), "signal name empty");
        assert!(!s.evidence.is_empty(), "signal evidence empty");
        assert!(s.weight > 0.0, "signal weight zero or negative");
        assert!(
            s.score >= 0.0 && s.score <= 1.0,
            "signal score out of range"
        );
    }
}

#[test]
fn event_wiring_rationale_non_empty() {
    assert!(
        !score_event_wiring(true, true, true, true, true)
            .rationale
            .is_empty()
    );
    assert!(
        !score_event_wiring(false, false, false, false, false)
            .rationale
            .is_empty()
    );
}

// ── score_sql_trace ────────────────────────────────────────────────────────────

#[test]
fn sql_trace_all_true_is_high() {
    let c = score_sql_trace(true, true, true, true, true);
    assert!(c.score >= 0.8, "expected high, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::High);
}

#[test]
fn sql_trace_all_false_is_low() {
    let c = score_sql_trace(false, false, false, false, false);
    assert!(c.score < 0.5, "expected low, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::Low);
}

#[test]
fn sql_trace_score_in_unit_range() {
    let cases = [
        (false, false, false, false, false),
        (true, false, false, false, false),
        (true, true, true, false, false),
        (true, true, true, true, true),
    ];
    for (a, b, c, d, e) in cases {
        let r = score_sql_trace(a, b, c, d, e);
        assert!(r.score >= 0.0 && r.score <= 1.0);
    }
}

#[test]
fn sql_trace_is_deterministic() {
    let c1 = score_sql_trace(true, false, true, true, false);
    let c2 = score_sql_trace(true, false, true, true, false);
    assert_eq!(c1.score, c2.score);
}

#[test]
fn sql_trace_score_monotone_with_evidence() {
    let none = score_sql_trace(false, false, false, false, false);
    let some = score_sql_trace(true, true, false, false, false);
    let all = score_sql_trace(true, true, true, true, true);
    assert!(none.score < some.score);
    assert!(some.score < all.score);
}

#[test]
fn sql_trace_has_five_signals() {
    let c = score_sql_trace(true, true, true, true, true);
    assert_eq!(c.signals.len(), 5);
}

// ── score_control_binding ──────────────────────────────────────────────────────

#[test]
fn control_binding_all_true_is_high() {
    let c = score_control_binding(true, true, true, true);
    assert!(c.score >= 0.8, "expected high, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::High);
}

#[test]
fn control_binding_all_false_is_low() {
    let c = score_control_binding(false, false, false, false);
    assert!(c.score < 0.5, "expected low, got {}", c.score);
    assert_eq!(c.band, ConfidenceBand::Low);
}

#[test]
fn control_binding_score_in_unit_range() {
    let cases = [
        (false, false, false, false),
        (true, false, false, false),
        (true, true, false, false),
        (true, true, true, false),
        (true, true, true, true),
    ];
    for (a, b, c, d) in cases {
        let r = score_control_binding(a, b, c, d);
        assert!(r.score >= 0.0 && r.score <= 1.0);
    }
}

#[test]
fn control_binding_is_deterministic() {
    let c1 = score_control_binding(true, false, true, false);
    let c2 = score_control_binding(true, false, true, false);
    assert_eq!(c1.score, c2.score);
}

#[test]
fn control_binding_score_monotone_with_evidence() {
    let none = score_control_binding(false, false, false, false);
    let some = score_control_binding(true, true, false, false);
    let all = score_control_binding(true, true, true, true);
    assert!(none.score < some.score);
    assert!(some.score < all.score);
}

#[test]
fn control_binding_has_four_signals() {
    let c = score_control_binding(true, true, true, true);
    assert_eq!(c.signals.len(), 4);
}

// ── escape_tantivy_literal ─────────────────────────────────────────────────────

#[test]
fn escape_tantivy_literal_escapes_special_chars() {
    let input = r#"unsafe { *const char: &str }"#;
    let out = escape_tantivy_literal(input);
    assert!(out.contains(r"\{"));
    assert!(out.contains(r"\}"));
    assert!(out.contains(r"\*"));
    assert!(out.contains(r"\:"));
}

#[test]
fn escape_tantivy_literal_plain_text_unchanged_structure() {
    let input = "hello world";
    let out = escape_tantivy_literal(input);
    // Plain alphanumeric text should not gain backslash prefixes
    assert!(!out.contains('\\'));
}

#[test]
fn escape_tantivy_literal_empty_string() {
    assert_eq!(escape_tantivy_literal(""), "");
}

#[test]
fn escape_tantivy_literal_is_deterministic() {
    let input = "foo(bar) + baz";
    assert_eq!(escape_tantivy_literal(input), escape_tantivy_literal(input));
}

// ── chunk_id_from_hash ─────────────────────────────────────────────────────────

#[test]
fn chunk_id_from_hash_is_deterministic() {
    let hash = [0xabu8; 32];
    assert_eq!(chunk_id_from_hash(hash), chunk_id_from_hash(hash));
}

#[test]
fn chunk_id_from_hash_different_inputs_produce_different_ids() {
    let h1 = [0x01u8; 32];
    let h2 = [0x02u8; 32];
    assert_ne!(chunk_id_from_hash(h1), chunk_id_from_hash(h2));
}

#[test]
fn chunk_id_from_hash_all_zeros() {
    // Must not panic on all-zero hash
    let _ = chunk_id_from_hash([0u8; 32]);
}

// ── chunk_lines ────────────────────────────────────────────────────────────────

#[test]
fn chunk_lines_empty_input_returns_empty() {
    let chunks = chunk_lines("", 500);
    assert!(chunks.is_empty());
}

#[test]
fn chunk_lines_short_text_is_single_chunk() {
    let text = "line one\nline two\nline three\n";
    let chunks = chunk_lines(text, 10_000);
    assert_eq!(chunks.len(), 1);
}

#[test]
fn chunk_lines_chunk_start_line_is_one_based() {
    let text = "a\nb\nc\n";
    let chunks = chunk_lines(text, 10_000);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start_line, 1);
}

#[test]
fn chunk_lines_end_line_gte_start_line() {
    let text = "a\nb\nc\n";
    let chunks = chunk_lines(text, 10_000);
    for c in &chunks {
        assert!(c.end_line >= c.start_line);
    }
}

#[test]
fn chunk_lines_content_hash_non_empty() {
    let text = "fn foo() {}\n";
    let chunks = chunk_lines(text, 10_000);
    assert!(!chunks.is_empty());
    assert!(!chunks[0].content_hash.as_str().is_empty());
}

#[test]
fn chunk_lines_large_text_splits_into_multiple_chunks() {
    // 200 lines * ~20 chars each = ~4000 chars; limit = 500
    let line = "x".repeat(19) + "\n";
    let text = line.repeat(200);
    let chunks = chunk_lines(&text, 500);
    assert!(
        chunks.len() > 1,
        "expected multiple chunks, got {}",
        chunks.len()
    );
}

#[test]
fn chunk_lines_set_doc_id_populates_doc_id() {
    let text = "fn bar() {}\n";
    let mut chunks = chunk_lines(text, 10_000);
    assert!(!chunks.is_empty());
    chunks[0].set_doc_id("src/lib.rs");
    assert!(!chunks[0].doc_id.as_str().is_empty());
}

#[test]
fn chunk_lines_doc_id_deterministic() {
    let text = "fn baz() {}\n";
    let mut c1 = chunk_lines(text, 10_000);
    let mut c2 = chunk_lines(text, 10_000);
    c1[0].set_doc_id("src/lib.rs");
    c2[0].set_doc_id("src/lib.rs");
    assert_eq!(c1[0].doc_id.as_str(), c2[0].doc_id.as_str());
}

// ── semantic_chunk_lines ───────────────────────────────────────────────────────

#[test]
fn semantic_chunk_lines_no_symbols_same_as_chunk_lines() {
    let text = "fn a() {}\nfn b() {}\n";
    let plain = chunk_lines(text, 10_000);
    let sem = semantic_chunk_lines(text, 10_000, &[]);
    assert_eq!(plain.len(), sem.len());
    assert_eq!(plain[0].start_line, sem[0].start_line);
    assert_eq!(plain[0].end_line, sem[0].end_line);
}

// ── SymbolExtractor ────────────────────────────────────────────────────────────

#[test]
fn symbol_extractor_new_does_not_panic() {
    let _ = SymbolExtractor::new();
}

#[test]
fn symbol_extractor_default_equals_new() {
    let _a = SymbolExtractor::new();
    let _b = SymbolExtractor::default();
    // Both should extract identical results for the same input
    let code = "fn hello() {}";
    let path = Path::new("foo.rs");
    let (s1, e1) = SymbolExtractor::new().extract(path, code);
    let (s2, e2) = SymbolExtractor::default().extract(path, code);
    assert_eq!(s1.len(), s2.len());
    assert_eq!(e1.len(), e2.len());
}

#[test]
fn symbol_extractor_rust_finds_function() {
    let code = "pub fn greet(name: &str) -> String { format!(\"hi {}\", name) }\n";
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("greet.rs"), code);
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "greet" && s.kind == "function"),
        "expected 'greet' function; found: {:?}",
        symbols
    );
}

#[test]
fn symbol_extractor_rust_finds_struct() {
    let code = "pub struct Foo { pub x: i32 }\n";
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("foo.rs"), code);
    assert!(
        symbols.iter().any(|s| s.name == "Foo" && s.kind == "class"),
        "expected 'Foo' struct; found: {:?}",
        symbols
    );
}

#[test]
fn symbol_extractor_unknown_extension_returns_empty() {
    let code = "some random bytes that are not source code";
    let (symbols, edges) = SymbolExtractor::new().extract(Path::new("blob.xyz"), code);
    assert!(symbols.is_empty(), "expected no symbols for unknown ext");
    assert!(edges.is_empty(), "expected no edges for unknown ext");
}

#[test]
fn symbol_extractor_symbol_start_line_is_positive() {
    let code = "fn foo() {}\nfn bar() {}\n";
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("x.rs"), code);
    for s in &symbols {
        assert!(s.start_line > 0, "start_line must be 1-based positive");
    }
}

#[test]
fn symbol_extractor_symbol_end_line_gte_start_line() {
    let code = "fn foo() { let x = 1; let y = 2; x + y }\n";
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("x.rs"), code);
    for s in &symbols {
        assert!(s.end_line >= s.start_line);
    }
}

#[test]
fn symbol_extractor_cpp_finds_class_and_method() {
    let code = r#"
class Calc {
public:
    int add(int a, int b) { return a + b; }
};
"#;
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("calc.cpp"), code);
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "Calc" && s.kind == "class"),
        "expected Calc class"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "add" && s.kind == "function"),
        "expected add method"
    );
}

#[test]
fn symbol_extractor_python_finds_function() {
    let code = "def hello():\n    pass\n";
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("hello.py"), code);
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "hello" && s.kind == "function"),
        "expected 'hello' python function; found: {:?}",
        symbols
    );
}

// ── solution_parser ────────────────────────────────────────────────────────────

#[test]
fn classify_project_type_csharp_guid() {
    // FAE04EC0-301F-11D3-BF4B-00C04F79EFBC is the C# project type GUID
    let pt = classify_project_type("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}");
    assert_eq!(pt, ProjectType::CSharp);
}

#[test]
fn classify_project_type_vbnet_guid() {
    // F184B08F-C81C-45F6-A57F-5ABD9991F28F is the VB.NET project type GUID
    let pt = classify_project_type("{F184B08F-C81C-45F6-A57F-5ABD9991F28F}");
    assert_eq!(pt, ProjectType::VbNet);
}

#[test]
fn classify_project_type_unknown_guid() {
    let pt = classify_project_type("{00000000-0000-0000-0000-000000000000}");
    assert_eq!(pt, ProjectType::Unknown);
}

#[test]
fn classify_project_type_display_strings() {
    assert_eq!(ProjectType::CSharp.to_string(), "csharp");
    assert_eq!(ProjectType::VbNet.to_string(), "vbnet");
    assert_eq!(ProjectType::Unknown.to_string(), "unknown");
    assert_eq!(ProjectType::WebApplication.to_string(), "web_application");
    assert_eq!(ProjectType::ClassLibrary.to_string(), "class_library");
}

#[test]
fn parse_solution_empty_returns_no_projects() {
    let projects = parse_solution("");
    assert!(projects.is_empty());
}

#[test]
fn parse_solution_single_csharp_project() {
    let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "MyApp", "MyApp\MyApp.csproj", "{11111111-1111-1111-1111-111111111111}"
EndProject
"#;
    let projects = parse_solution(sln);
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "MyApp");
    assert_eq!(projects[0].project_type, ProjectType::CSharp);
}

#[test]
fn parse_solution_project_guid_populated() {
    let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Lib", "Lib\Lib.csproj", "{AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA}"
EndProject
"#;
    let projects = parse_solution(sln);
    assert_eq!(projects.len(), 1);
    assert!(!projects[0].project_guid.is_empty());
}

#[test]
fn parse_solution_relative_path_populated() {
    let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Core", "src\Core\Core.csproj", "{BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB}"
EndProject
"#;
    let projects = parse_solution(sln);
    assert_eq!(projects.len(), 1);
    assert!(!projects[0].relative_path.is_empty());
}

#[test]
fn parse_solution_configs_empty_sln_returns_empty() {
    let configs = parse_solution_configs("");
    assert!(configs.is_empty());
}

#[test]
fn parse_solution_configs_standard_configs() {
    let sln = r#"
GlobalSection(SolutionConfigurationPlatforms) = preSolution
    Debug|Any CPU = Debug|Any CPU
    Release|Any CPU = Release|Any CPU
EndGlobalSection
"#;
    let configs = parse_solution_configs(sln);
    assert!(
        configs.iter().any(|c| c.contains("Debug")),
        "expected Debug config; got: {:?}",
        configs
    );
    assert!(
        configs.iter().any(|c| c.contains("Release")),
        "expected Release config; got: {:?}",
        configs
    );
}

// ── sql_parser::analyze_sql ────────────────────────────────────────────────────

#[test]
fn analyze_sql_select_sets_operation() {
    let a = analyze_sql("SELECT id, name FROM Users WHERE id = @id");
    assert_eq!(a.operation, SqlOp::Select);
}

#[test]
fn analyze_sql_insert_sets_operation() {
    let a = analyze_sql("INSERT INTO Orders (col1) VALUES (@val)");
    assert_eq!(a.operation, SqlOp::Insert);
}

#[test]
fn analyze_sql_update_sets_operation() {
    let a = analyze_sql("UPDATE Products SET price = @price WHERE id = @id");
    assert_eq!(a.operation, SqlOp::Update);
}

#[test]
fn analyze_sql_delete_sets_operation() {
    let a = analyze_sql("DELETE FROM Sessions WHERE expired = 1");
    assert_eq!(a.operation, SqlOp::Delete);
}

#[test]
fn analyze_sql_preserves_raw_sql() {
    let sql = "SELECT 1 FROM Dual";
    let a = analyze_sql(sql);
    assert_eq!(a.raw_sql, sql);
}

#[test]
fn analyze_sql_detects_parameter() {
    let a = analyze_sql("SELECT * FROM Users WHERE id = @userId");
    assert!(
        a.parameters.iter().any(|p| p.name.contains("userId")),
        "expected @userId parameter; found: {:?}",
        a.parameters
    );
}

#[test]
fn analyze_sql_empty_string_does_not_panic() {
    let _ = analyze_sql("");
}

// ── sql_parser::generate_method_name ──────────────────────────────────────────

#[test]
fn generate_method_name_select_produces_non_empty_string() {
    let a = analyze_sql("SELECT * FROM Customers WHERE id = @id");
    let name = generate_method_name(&a);
    assert!(!name.is_empty());
}

#[test]
fn generate_method_name_is_deterministic() {
    let a = analyze_sql("SELECT id FROM Orders WHERE status = @status");
    assert_eq!(generate_method_name(&a), generate_method_name(&a));
}

// ── sync_hazard_detector::detect_sync_hazards ──────────────────────────────────

#[test]
fn detect_sync_hazards_clean_code_returns_no_hazards() {
    let src = "public string GetName() { return \"hello\"; }\n";
    let r = detect_sync_hazards(src, false);
    assert!(r.hazards.is_empty());
    assert_eq!(r.critical_count, 0);
}

#[test]
fn detect_sync_hazards_dot_result_is_critical() {
    let src = "var x = GetDataAsync().Result;\n";
    let r = detect_sync_hazards(src, false);
    assert!(
        r.critical_count > 0,
        "expected critical hazard for .Result; got {:?}",
        r.hazards
    );
}

#[test]
fn detect_sync_hazards_dot_wait_detected() {
    let src = "task.Wait();\n";
    let r = detect_sync_hazards(src, false);
    assert!(!r.hazards.is_empty(), "expected hazard for .Wait()");
}

#[test]
fn detect_sync_hazards_async_readiness_in_unit_range() {
    let src = "var x = op.Result; task.Wait(); task2.Wait();\n";
    let r = detect_sync_hazards(src, false);
    assert!(r.async_readiness >= 0.0 && r.async_readiness <= 1.0);
}

#[test]
fn detect_sync_hazards_severity_ordering() {
    // HazardSeverity implements Ord: Medium < High < Critical
    assert!(HazardSeverity::Medium < HazardSeverity::High);
    assert!(HazardSeverity::High < HazardSeverity::Critical);
}

// ── ingest::is_binary ──────────────────────────────────────────────────────────

#[test]
fn is_binary_nonexistent_path_returns_true() {
    // Fail-closed contract: unreadable files treated as binary.
    assert!(is_binary(std::path::Path::new(
        "/nonexistent/path/file.bin"
    )));
}

#[test]
fn is_binary_text_file_returns_false() {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(b"fn hello() {}\n").expect("write");
    assert!(!is_binary(f.path()));
}

#[test]
fn is_binary_null_byte_file_returns_true() {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(b"some\x00binary\x00data").expect("write");
    assert!(is_binary(f.path()));
}

// ── webforms::is_webforms_markup ───────────────────────────────────────────────

#[test]
fn is_webforms_markup_aspx_extension_true() {
    assert!(is_webforms_markup(std::path::Path::new("Default.aspx")));
}

#[test]
fn is_webforms_markup_ascx_extension_true() {
    assert!(is_webforms_markup(std::path::Path::new("Header.ascx")));
}

#[test]
fn is_webforms_markup_rs_extension_false() {
    assert!(!is_webforms_markup(std::path::Path::new("lib.rs")));
}

// ── webforms::candidate_codebehind_paths ──────────────────────────────────────

#[test]
fn candidate_codebehind_paths_returns_non_empty_for_aspx() {
    let paths = candidate_codebehind_paths(std::path::Path::new("Default.aspx"));
    assert!(!paths.is_empty());
}

#[test]
fn candidate_codebehind_paths_includes_cs_variant() {
    let paths = candidate_codebehind_paths(std::path::Path::new("Default.aspx"));
    assert!(
        paths.iter().any(|p| p.to_string_lossy().contains(".cs")),
        "expected a .cs codebehind candidate; got: {:?}",
        paths
    );
}

// ── control_mapping::lookup ────────────────────────────────────────────────────

#[test]
fn lookup_gridview_returns_some() {
    assert!(lookup("GridView").is_some());
}

#[test]
fn lookup_textbox_returns_some() {
    assert!(lookup("TextBox").is_some());
}

#[test]
fn lookup_unknown_control_returns_none() {
    assert!(lookup("NonExistentControlXYZ123").is_none());
}

#[test]
fn lookup_gridview_legacy_control_matches() {
    let m = lookup("GridView").expect("GridView should be in catalog");
    assert_eq!(m.legacy_control, "GridView");
}

#[test]
fn lookup_gridview_migration_complexity_positive() {
    let m = lookup("GridView").expect("GridView should be in catalog");
    assert!(m.migration_complexity > 0);
}
