# ask_codebase Brain — Milestone 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild `ask_codebase` from a heuristic Markdown-concatenating dispatcher into a deterministic planner → entity-resolve → typed parallel retrieval → authority-ranked, conflict-aware, honestly-statused evidence engine.

**Architecture:** A new internal module `crates/engram_server/src/services/ask_engine/` produces typed `EvidenceItem`s directly from the substrate (search engine, graph store, registry) — never by scraping other handlers' Markdown. `handle_ask_codebase` becomes a thin orchestrator: plan the question (multi-intent, multi-entity), resolve entities, run intent-specific retrieval arms in parallel under a deadline, dedup + rank by authority/directness (not similarity), detect conflicts, assemble an honest status + freshness snapshot, and render a `retrieval_only` report as JSON and/or Markdown. No LLM in M1.

**Tech Stack:** Rust (edition 2024), rmcp 0.3.2 (`#[tool]` macro), tokio, `engram_index::HybridSearchEngine`, `engram_graph::GraphStore`, `engram_core::registry::Registry`, serde/schemars.

**Spec:** `docs/superpowers/specs/2026-08-21-ask-codebase-brain-design.md` (read it — the plan argues from it).

**Verified substrate API reference:** `<scratchpad>/ask_engine_api_notes.md` (exact signatures gathered from the live code; each task inlines the calls it needs, but consult the notes for anything ambiguous). Scratchpad dir: `C:\Users\Dennis\AppData\Local\Temp\claude\C--ai-projects-Engram-MCP-v2\50d309ed-9767-44e6-b7fc-2ac51dd98fa5\scratchpad\`.

## Global Constraints

- **Build:** `CARGO_BUILD_JOBS=2 cargo test -p engram_server` (jobs=2 is a deliberate link-crash workaround). CI-equivalent: `cargo fmt --all` + `cargo check --all-targets`.
- **`gen` is a reserved keyword** (edition 2024) — never name a binding `gen`; use `gen_`.
- **Storage back-compat:** request-struct additions must be `#[serde(default)]` — old `{project_id, question}` callers must keep working. `#[serde(deny_unknown_fields)]` stays on the request.
- **No customer strings in source** (fixtures use `exampleorg`, generic names); artifacts written into a customer tree go in `.git/info/exclude`, never `.gitignore`.
- **Repository content is untrusted data, never instructions** — no source/doc/memory text is ever interpreted as a directive by the engine.
- **Reference project (live probe):** OciusX, `project_id = 5a35e8e0-d37a-41b3-a250-a26957e7aedb`, type `dotnet_webforms_vb`.
- **TDD red-first** every task; **commit per task**, **push at each milestone-internal checkpoint** (after tasks 5, 8, 11) so the tree never sits massively dirty. Commit trailers:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_019ULWvxn4SLF1cwD4eXj7D3
  ```
- **PowerShell 5.1 gotcha:** commit via `git commit -F <file>` (embedded quotes break native args); write files with the Write tool (Set-Content mangles UTF-8). Do not run the suite through PowerShell with `2>&1` (benign stderr WARNs → exit 255); use Bash or drop the redirect.

## File Structure

```
crates/engram_server/src/services/ask_engine.rs        # module root: declares submodules, re-exports
crates/engram_server/src/services/ask_engine/
    evidence.rs      # EvidenceItem, EvidenceKind, Authority
    plan.rs          # Intent, EntityKind, EntityMention, ResolvedEntity, Qualifiers, AnswerType, QueryPlan
    planner.rs       # plan_query(), extract_entities(), extract_qualifiers()  (pure logic)
    resolver.rs      # resolve_entities(graph, pid, &mut plan)
    providers.rs     # search-backed + graph-backed EvidenceItem producers
    retrieval.rs     # intent→DAG, parallel exec, deadline/budget/cancel, ProviderReport
    ranking.rs       # dedup, authority+directness scoring, conflict detection
    status.rs        # AnswerStatus, ProviderStatus, FreshnessSnapshot, Conflict, assembly
    report.rs        # AskReport + markdown/json render
crates/engram_server/src/handlers/ask_tools.rs          # MODIFY: handle_ask_codebase becomes orchestrator
crates/engram_server/src/models/requests.rs             # MODIFY: extend AskCodebaseRequest + AsOf/Audience
crates/engram_server/src/services/mod.rs                # MODIFY: add `pub mod ask_engine;`
crates/engram_server/src/tools.rs                       # MODIFY: ask_codebase description
crates/engram_server/src/capabilities.rs                # (unchanged; flag already present)
crates/engram_server/tests/ask_engine_tests.rs          # NEW: unit + integration tests
eval/ask_engine_golden.py                               # NEW: seed golden eval runner
eval/ask_engine_golden.jsonl                            # NEW: seed Q&A corpus
```

Module root pattern (matches repo, e.g. `full_project_migration_service.rs` + dir): `ask_engine.rs` declares the submodules and re-exports the public surface.

---

### Task 1: Module scaffold + typed evidence/plan/status model

**Files:**
- Create: `crates/engram_server/src/services/ask_engine.rs`
- Create: `crates/engram_server/src/services/ask_engine/evidence.rs`
- Create: `crates/engram_server/src/services/ask_engine/plan.rs`
- Create: `crates/engram_server/src/services/ask_engine/status.rs`
- Create: `crates/engram_server/src/services/ask_engine/report.rs`
- Modify: `crates/engram_server/src/services/mod.rs` (add `pub mod ask_engine;` alphabetically, before `benchmark_service`)
- Test: `crates/engram_server/tests/ask_engine_tests.rs`

**Interfaces:**
- Produces: the shared types every later task consumes. Exact definitions below (drafted in `<scratchpad>/ask_engine_types_draft.rs`; `FreshnessSnapshot` corrected to match trackable sources).

- [ ] **Step 1: Write the failing test**

```rust
// crates/engram_server/tests/ask_engine_tests.rs
#![allow(clippy::unwrap_used)]
use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};

#[test]
fn authority_orders_strongest_first_and_weight_is_monotonic() {
    // Declaration order = Ord order: RuntimeEvidence is the smallest (strongest).
    assert!(Authority::RuntimeEvidence < Authority::CurrentCode);
    assert!(Authority::CurrentCode < Authority::SemanticSimilarity);
    // weight() is the inverse: strongest authority => highest weight.
    assert!(Authority::CurrentCode.weight() > Authority::SemanticSimilarity.weight());
    assert!(Authority::RuntimeEvidence.weight() >= Authority::CurrentCode.weight());
}

#[test]
fn evidence_item_serializes_with_snake_case_kind() {
    let ev = EvidenceItem {
        evidence_id: "ev_1".into(), kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode, path: Some("a.vb".into()),
        lines: Some((10, 20)), symbol_id: None, title: None,
        content: "x".into(), generation: Some(3), commit: None, timestamp: None,
        confidence: 0.9, relevance: 0.8, extraction_method: "fts".into(),
        warnings: vec![], provider: "code".into(), score: None, directness: None,
    };
    let j = serde_json::to_value(&ev).unwrap();
    assert_eq!(j["kind"], "source_code");
    assert_eq!(j["authority"], "current_code");
    assert_eq!(j["evidence_id"], "ev_1");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_BUILD_JOBS=2 cargo test -p engram_server --test ask_engine_tests`
Expected: FAIL — `unresolved import engram_server::services::ask_engine`.

- [ ] **Step 3: Create `evidence.rs`** — paste the `EvidenceKind`, `Authority` (with `weight()`), and `EvidenceItem` definitions verbatim from `<scratchpad>/ask_engine_types_draft.rs` (the `// ── evidence.rs ──` section). `EvidenceItem` derives `#[derive(Debug, Clone, Serialize)]`; enums derive `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]` and `Authority` additionally `PartialOrd, Ord`; both use `#[serde(rename_all = "snake_case")]`. `use serde::Serialize;` at the top.

- [ ] **Step 4: Create `plan.rs`, `status.rs`, `report.rs`** — paste the `plan.rs`, `status.rs`, and `report.rs` sections from the types draft, with these corrections to `status.rs`'s `FreshnessSnapshot` (the draft's `memory_generation`/`business_logic_generation` are NOT separately trackable):

```rust
#[derive(Debug, Clone, Default, Serialize)]
pub struct FreshnessSnapshot {
    pub project_generation: Option<u64>,
    pub git_commit: Option<String>,     // registry meta "last_git_oid"
    pub git_branch: Option<String>,     // from as_of.branch only (not stored in index)
    pub history_watermark: Option<u64>, // meta "pr_ingest_watermark" (else "total_commits")
    pub last_index_ms: Option<u64>,     // meta "last_index_completed_ms"
    pub semantic_tier: String,          // "semantic" | "degraded_trigram" | "off"
    pub reindex_required: bool,         // ProjectRecord.reindex_required_since_ms.is_some()
    pub incompatible: bool,             // an evidence item's generation != active gen
}
```

- [ ] **Step 5: Create `ask_engine.rs` module root**

```rust
//! ask_engine — the deterministic evidence engine behind ask_codebase.
//! Produces typed EvidenceItems directly from the substrate; no Markdown parsing.
pub mod evidence;
pub mod plan;
pub mod planner;
pub mod resolver;
pub mod providers;
pub mod retrieval;
pub mod ranking;
pub mod status;
pub mod report;
```

Comment out `planner`/`resolver`/`providers`/`retrieval`/`ranking` until their tasks create them, OR create empty stub files now with `// filled in Task N`. (Prefer stubs so the module compiles at each task.) Add `pub mod ask_engine;` to `services/mod.rs`. Confirm `services` is `pub` enough for the test path `engram_server::services::ask_engine::...` — check `crates/engram_server/src/lib.rs` re-exports `pub mod services;` (it does for other integration tests; if not, add it).

- [ ] **Step 6: Run tests to verify they pass**

Run: `CARGO_BUILD_JOBS=2 cargo test -p engram_server --test ask_engine_tests`
Expected: PASS (2 tests).

- [ ] **Step 7: Commit**

```bash
git add crates/engram_server/src/services/ask_engine.rs crates/engram_server/src/services/ask_engine/ crates/engram_server/src/services/mod.rs crates/engram_server/tests/ask_engine_tests.rs
git commit -F <msgfile>   # "feat(ask_engine): typed evidence/plan/status model (M1 task 1)"
```

---

### Task 2: Deterministic multi-intent planner

**Files:**
- Create/replace stub: `crates/engram_server/src/services/ask_engine/planner.rs`
- Test: `crates/engram_server/tests/ask_engine_tests.rs` (append)

**Interfaces:**
- Consumes: `plan::{QueryPlan, Intent, EntityMention, EntityKind, Qualifiers, AnswerType}`, `evidence::EvidenceKind`.
- Produces: `pub fn plan_query(question: &str) -> QueryPlan`, `pub fn extract_entities(q: &str) -> Vec<EntityMention>`, `pub fn extract_qualifiers(q: &str, lower: &str) -> Qualifiers`.

- [ ] **Step 1: Write the failing tests**

```rust
use engram_server::services::ask_engine::planner::{plan_query, extract_entities};
use engram_server::services::ask_engine::plan::{Intent, EntityKind};

#[test]
fn compound_question_yields_multiple_intents() {
    let p = plan_query("How does authentication work, and what would break if we changed it?");
    let intents: Vec<Intent> = p.intents.iter().map(|(i, _)| *i).collect();
    assert!(intents.contains(&Intent::Explain), "{intents:?}");
    assert!(intents.contains(&Intent::Impact), "{intents:?}");
}

#[test]
fn extracts_multiple_entities_and_kinds() {
    let ents = extract_entities(r#"What breaks if we change marker serialization in ImportService.vb from XML to JSON?"#);
    assert!(ents.iter().any(|e| e.text == "ImportService.vb" && e.guessed_kind == EntityKind::File));
    // change verb captured as a qualifier, not an entity — checked separately.
}

#[test]
fn why_is_rationale_not_history() {
    let p = plan_query("Why is customer status enforced on the server?");
    assert_eq!(p.intents.first().unwrap().0, Intent::Rationale);
}

#[test]
fn bare_topic_defaults_to_explain() {
    let p = plan_query("marker clustering");
    assert_eq!(p.intents.first().unwrap().0, Intent::Explain);
}

#[test]
fn change_verb_qualifier_from_x_to_y() {
    let p = plan_query("change serialization from XML to JSON");
    assert_eq!(p.qualifiers.change, Some(("XML".into(), "JSON".into())));
}
```

- [ ] **Step 2: Run to verify fail** — `--test ask_engine_tests`; Expected: FAIL (unresolved `planner`).

- [ ] **Step 3: Implement `planner.rs`** — paste the full contents of `<scratchpad>/ask_engine_planner_draft.rs` (functions `plan_query`, `extract_entities`, `extract_qualifiers`, `primary_answer_type`, `needed_evidence_for`). Fix the `use super::...` paths to `use crate::services::ask_engine::plan::*;` / `use crate::services::ask_engine::evidence::EvidenceKind;`. Ensure the closure-mutation pattern (`add`, `push`, `want`) compiles under the borrow checker — the draft passes the accumulator as an explicit `&mut` arg to avoid closure-capture conflicts; keep that shape.

- [ ] **Step 4: Run to verify pass** — Expected: PASS (5 new tests).

- [ ] **Step 5: Commit** — `"feat(ask_engine): deterministic multi-intent/entity planner (M1 task 2)"`.

---

### Task 3: Entity resolver

**Files:**
- Create/replace stub: `crates/engram_server/src/services/ask_engine/resolver.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: `engram_graph::GraphStore`, `engram_graph::ResolveResult`, `plan::{QueryPlan, EntityMention, ResolvedEntity, EntityKind}`.
- Produces: `pub fn resolve_entities(graph: &GraphStore, project_id: &str, plan: &mut QueryPlan)` — fills each `EntityMention.resolved`. `pub fn node_to_entity_kind(node_type: &str) -> EntityKind` (map graph node_type → EntityKind).

- [ ] **Step 1: Write the failing test** (seed graph nodes directly — no indexing needed)

```rust
use engram_server::services::ask_engine::resolver::resolve_entities;
use engram_server::services::ask_engine::planner::plan_query;
// Uses the direct-graph-seeding harness (see explain_change_tests.rs:20-58 pattern):
// build AppState, put_project + set_meta active_generation=1, insert nodes via
// state.graph. Helper `seed_project(...)` defined in the test file.

#[tokio::test]
async fn resolves_unique_symbol_and_marks_ambiguous() {
    let (state, pid) = seed_project(&[
        ("sym:SaveMarker@a.vb", "function", "SaveMarker", "a.vb"),
        ("sym:SaveMarker@b.vb", "function", "SaveMarker", "b.vb"),
        ("sym:ImportService.Run", "function", "Run", "ImportService.vb"),
    ]).await;
    let mut plan = plan_query("where is SaveMarker used and how does Run work?");
    resolve_entities(&state.graph, &pid, &mut plan);
    let sm = plan.entities.iter().find(|e| e.text == "SaveMarker").unwrap();
    assert_eq!(sm.resolved.len(), 2, "SaveMarker is ambiguous across two files");
    let run = plan.entities.iter().find(|e| e.text.contains("Run")).unwrap();
    assert!(run.resolved.iter().any(|r| r.node_id.is_some()));
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement `resolver.rs`** — for each `EntityMention`, call `graph.resolve_symbol(project_id, &mention.text, None, None)` and map:
  - `ResolveResult::Unique(n)` → one `ResolvedEntity { kind: node_to_entity_kind(&n.node_type), canonical: n.name.clone() (or n.node_id), node_id: Some(n.node_id), confidence: 0.9 }`.
  - `ResolveResult::Ambiguous(v)` → up to `MAX_BRANCHES = 4` `ResolvedEntity`s, `confidence: 0.5`.
  - `ResolveResult::NotFound` → leave `resolved` empty (a search-only entity).
  `node_to_entity_kind`: `"file"→File, "function"|"class"|"interface"→Symbol, "db_table"→Table, "db_column"→Column, "global_state"→Setting, "page"|"control"→UiControl, "route_handler"|"http_handler"→Route, "insight"→Concept, _→Unknown`. Run resolution inside the async fn directly (GraphStore reads are sync redb; wrap the whole loop in `tokio::task::spawn_blocking` only when called from the async orchestrator — here the function itself is sync and the test calls it directly).
  - Write `resolve_entities` as a **sync** fn `pub fn resolve_entities(graph: &GraphStore, project_id: &str, plan: &mut QueryPlan)`; the orchestrator (Task 10) wraps it in `spawn_blocking`. Adjust the test to call it synchronously after cloning the graph handle.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit** — `"feat(ask_engine): graph-backed entity resolver with ambiguity branches (M1 task 3)"`.

---

### Task 4: Search-backed evidence providers

**Files:**
- Create/replace stub: `crates/engram_server/src/services/ask_engine/providers.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: `engram_index::{HybridSearchEngine, HybridQuery, HybridHit}`, `engram_core::registry::Registry` + `MemorySection`, `evidence::{EvidenceItem, EvidenceKind, Authority}`.
- Produces (all take `&mut usize` id counter for `ev_<n>` ids; all async where they call `search`):
  - `pub async fn code_evidence(search, pid, gen_, query, top_k, cancel, id: &mut usize) -> (Vec<EvidenceItem>, ProviderOutcome)`
  - `pub async fn knowledge_evidence(search, pid, gen_, namespace, query, top_k, cancel, id) -> (Vec<EvidenceItem>, ProviderOutcome)` (covers doc/insight/business_logic/history by namespace + authority arg)
  - `pub fn memory_evidence(registry, pid, query, top_k, id) -> (Vec<EvidenceItem>, ProviderOutcome)` (registry scan, kind-aware authority)
  - `ProviderOutcome { status: ProviderStatus, note: Option<String> }`.

- [ ] **Step 1: Write the failing test** (fts_only fixture)

```rust
#[tokio::test]
async fn code_evidence_returns_typed_items_with_lines() {
    let (state, engram, pid) = index_fixture(&[
        ("Auth.vb", "Public Function Authenticate(user As String) As Boolean\n  Return True\nEnd Function\n"),
    ]).await;
    let ps = engram.ensure_project_runtime(&pid).await.unwrap();
    let gen_ = engram.get_active_generation(&pid).await.unwrap();
    let mut id = 0usize;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (items, outcome) = engram_server::services::ask_engine::providers::code_evidence(
        &ps.search, &pid, gen_, "Authenticate", 5, &cancel, &mut id).await;
    assert!(!items.is_empty());
    assert_eq!(items[0].kind, EvidenceKind::SourceCode);
    assert_eq!(items[0].authority, Authority::CurrentCode);
    assert!(items[0].path.as_deref() == Some("Auth.vb"));
    assert!(items[0].lines.is_some());
    assert_eq!(outcome.status, engram_server::services::ask_engine::status::ProviderStatus::Hit);
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement `providers.rs`.** Core mapping from a `HybridHit` (fetch content via `search.get_doc_by_pk(&h.pk)`):

```rust
fn hit_to_evidence(
    h: &engram_index::HybridHit, kind: EvidenceKind, authority: Authority,
    provider: &str, extraction: &str, gen_: u64, content: String, id: &mut usize,
) -> EvidenceItem {
    *id += 1;
    EvidenceItem {
        evidence_id: format!("ev_{id}"),
        kind, authority,
        path: Some(h.path.as_str().replace('\\', "/")),
        lines: (h.start_line > 0).then_some((h.start_line, h.end_line)),
        symbol_id: None,                 // search hits carry no node id
        title: None,
        content: content.chars().take(1200).collect(),
        generation: Some(gen_),
        commit: None,
        timestamp: h.timestamp,
        confidence: 0.85,
        relevance: h.score.clamp(0.0, 1.0),
        extraction_method: extraction.into(),
        warnings: vec![],
        provider: provider.into(),
        score: None, directness: None,
    }
}
```
`code_evidence`: build `HybridQuery { project_id, namespace: NAMESPACE_MEMORY, generation: gen_, text: query, top_k, fts_mode: "loose".into(), use_mmr: true, ..zeroed_optionals }` (all `Option` fields `None`), `search.search(&q, None, cancel).await`. On each hit, `search.get_doc_by_pk(&h.pk)` → `.2` content; map with `CurrentCode`/`SourceCode`/`fts`. Distinguish outcome: `Err` → `ProviderStatus::Failed` + note; `Ok(empty)` → `Empty`; else `Hit`. **Never** `unwrap_or_default` — a search error must surface as `Failed`, not `Empty`.
`knowledge_evidence(namespace, authority)`: same but namespace/authority parameterized — used for `NAMESPACE_MEMORY_BANK` (DocSection/CurrentDocs), `NAMESPACE_INSIGHTS` (Insight/DreamerInsight), `NAMESPACE_BUSINESS_LOGIC` (BusinessRule/DerivedBusinessLogic), `NAMESPACE_HISTORY` (HistoryCommit/MergedHistory).
`memory_evidence`: `registry.list_memory_sections(pid)`; lexical-filter sections whose `title`/`content` contains any query token (case-insensitive); map each to `EvidenceItem { kind: MemoryNote, authority: authority_for_kind(sec.kind.as_deref()), path: None, title: Some(sec.title), content: sec.content truncated, generation: None, timestamp: (sec.updated_at_ms>0).then_some(sec.updated_at_ms), extraction_method: "memory", ... }`. `authority_for_kind`: `Some("decision")|Some("reference")→ApprovedRequirement; _→AgentMemory`.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit** — `"feat(ask_engine): search + memory typed evidence providers (M1 task 4)"`.

---

### Task 5: Graph-backed evidence providers + push

**Files:**
- Modify: `crates/engram_server/src/services/ask_engine/providers.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: `engram_graph::{GraphStore, Node, Edge, EdgeKind}`, coupling helpers, `evidence::*`.
- Produces (sync — graph reads are sync redb; orchestrator wraps in `spawn_blocking`):
  - `pub fn symbol_ref_evidence(graph, pid, symbol_name, file_scope: Option<&str>, max: usize, id) -> (Vec<EvidenceItem>, ProviderOutcome)`
  - `pub fn impact_evidence(graph, pid, target_node_id, limit, id) -> (Vec<EvidenceItem>, ProviderOutcome)`
  - `pub fn concept_evidence(graph, pid, concept, cap, id) -> (Vec<EvidenceItem>, ProviderOutcome)`
  - `pub fn companion_evidence(graph, pid, file_node_id, id) -> (Vec<EvidenceItem>, ProviderOutcome)`

- [ ] **Step 1: Write the failing test** (seed graph: two functions with a Calls edge)

```rust
#[tokio::test]
async fn impact_evidence_surfaces_incoming_callers_as_graph_relations() {
    let (state, pid) = seed_project_with_edges(
        &[("sym:Save@a.vb","function","Save","a.vb"), ("sym:Caller@b.vb","function","Caller","b.vb")],
        &[("sym:Caller@b.vb","sym:Save@a.vb", engram_graph::EdgeKind::Calls, 3)],
    ).await;
    let mut id = 0usize;
    let (items, outcome) = engram_server::services::ask_engine::providers::impact_evidence(
        &state.graph, &pid, "sym:Save@a.vb", 50, &mut id);
    assert_eq!(outcome.status, ProviderStatus::Hit);
    assert!(items.iter().any(|e| e.kind == EvidenceKind::GraphRelation
        && e.symbol_id.as_deref() == Some("sym:Caller@b.vb")
        && e.authority == Authority::CurrentCode));
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement the four providers** using the verified handler logic (see `<scratchpad>/ask_engine_api_notes.md` §"Handler retrieval logic"):
  - `impact_evidence`: `graph.find_incoming_edges_with_kind(pid, None, target_node_id, limit.clamp(1,1000))` → for each `(src_id, kind, weight)`, `graph.get_node(pid, &src_id)?` → `EvidenceItem { kind: GraphRelation, authority: CurrentCode, path: node.file_path.as_str(), lines: (node.start_line,node.end_line), symbol_id: Some(src_id), title: Some(node.name), content: format!("{} {} {} (weight {weight})", node.name, kind.as_str(), "→target"), extraction_method: "graph", directness: Some(0.9), relevance: (weight as f32).min(10.0)/10.0, ... }`. Outcome `Empty` if no incoming, `Hit` otherwise, `Failed` on a graph error.
  - `symbol_ref_evidence`: `graph.query_nodes_by_symbol_name(pid, symbol_name, file_scope, 50)`; per node collect incoming (`find_incoming_edges_with_kind(pid,None,node_id, max)`) + outgoing (`neighbors` per `EdgeKind::ALL`, capped) as `GraphRelation` items.
  - `concept_evidence`: `graph.query_nodes(pid, None, None, None, 200_000)` filtered by a simple token match on `node.name` (reuse a local `matches_concept`), bucket-cap `cap` per node_type, map to `EvidenceItem { kind: ConceptGroup, authority: CurrentCode, ... }`. (Full-scan is acceptable — mirror `handle_get_concept_footprint`; note `scan_truncated` in a warning if 200_000 hit.)
  - `companion_evidence`: `engram_graph::algorithms::coupling::file_temporal_couplings(graph, pid, file_node_id, 2, 20)` → each neighbor as `EvidenceItem { kind: GraphRelation, authority: MergedHistory, extraction_method: "git", content: "co-changes with <file> (n times)", directness: Some(0.5) }`.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit + PUSH** — `"feat(ask_engine): graph-backed typed evidence providers (M1 task 5)"`; then `git push -u origin ask-codebase-brain`.

---

### Task 6: Intent-specific retrieval DAGs (parallel, deadline, budget, cancellation)

**Files:**
- Create/replace stub: `crates/engram_server/src/services/ask_engine/retrieval.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: providers (Task 4/5), `plan::QueryPlan`, `status::{ProviderReport, ProviderStatus}`, `ProjectState`, `GraphStore`.
- Produces: `pub async fn gather_evidence(engram_ctx: &RetrievalCtx<'_>, plan: &QueryPlan, depth: Depth, deadline: Duration, cancel: CancellationToken) -> (Vec<EvidenceItem>, Vec<ProviderReport>)`. `RetrievalCtx { search: Arc<HybridSearchEngine>, graph: Arc<GraphStore>, registry: Arc<Registry>, project_id: String, generation: u64 }`. `Depth { Quick, Standard, Deep }` with per-arm `top_k`/budget.

- [ ] **Step 1: Write the failing tests**
```rust
#[tokio::test]
async fn gather_runs_arms_for_each_intent_and_reports_per_provider() {
    // plan with Explain + Impact over an indexed+graph-seeded fixture.
    // Assert: providers vec contains code + memory (Explain) AND graph impact (Impact);
    // a namespace with nothing → ProviderStatus::Empty (not missing, not Failed).
}
#[tokio::test]
async fn a_failing_arm_becomes_failed_not_a_panic() {
    // inject an entity that resolves but whose provider errors (e.g. bogus node id);
    // gather_evidence returns, ProviderReport for that arm = Failed.
}
```
(Write both with concrete fixtures mirroring Task 4/5 harnesses.)

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement `gather_evidence`.** For each `(intent, _weight)` in `plan.intents`, select the arm set per the spec's DAGs (Explain→code+doc+graph_relations+business_logic+memory; Impact→impact(per resolved entity)+tests(code query "test "+entity)+setting/business rules+companions; Usage→symbol_refs+concept; History→history+code; Rationale→memory+history+code; Feature→memory+concept+code+companions; BugDiagnosis→code(symptom strings)+impact+business_logic (causal — no similarity-only anchor); Requirements→memory+doc; Compare→code+history; Test→code("test")+symbol_refs; Unknowns→memory+doc+code). Deduplicate the arm set across intents (run each provider-arm once). Wrap each arm as a future; run concurrently with `futures::future::join_all` (or `FuturesUnordered`) under `tokio::time::timeout(deadline, ...)`. Graph (sync) providers run via `tokio::task::spawn_blocking` with cloned `Arc<GraphStore>`. On per-arm timeout → `ProviderReport{status: TimedOut}`. Respect `cancel`. Per-arm `top_k`/result cap from `Depth` (Quick=3, Standard=6, Deep=10). Collect all `EvidenceItem`s + one `ProviderReport` per arm run.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit** — `"feat(ask_engine): parallel intent-DAG retrieval with deadline/budget (M1 task 6)"`.

---

### Task 7: Dedup + authority/directness ranking + conflict detection (anti-anchoring core)

**Files:**
- Create/replace stub: `crates/engram_server/src/services/ask_engine/ranking.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: `evidence::{EvidenceItem, Authority}`, `status::Conflict`.
- Produces: `pub fn rank_and_select(items: Vec<EvidenceItem>, cap: usize) -> Vec<EvidenceItem>` (dedup, score, MMR-after-authority, truncate to `cap`; sets each item's `score`/`directness`). `pub fn detect_conflicts(items: &[EvidenceItem], active_generation: u64) -> Vec<Conflict>`.

- [ ] **Step 1: Write the failing tests**
```rust
#[test]
fn one_direct_code_relation_outranks_ten_weak_semantic_hits() {
    let mut items = vec![direct_graph_item("ev_x")]; // GraphRelation, CurrentCode, directness 0.9
    for i in 0..10 { items.push(weak_semantic_item(&format!("ev_{i}"))); } // SemanticSimilarity, relevance 0.6
    let ranked = rank_and_select(items, 3);
    assert_eq!(ranked[0].evidence_id, "ev_x");
}
#[test]
fn dedup_collapses_same_path_and_lines() {
    let ranked = rank_and_select(vec![code_item("a.vb",10,20,"ev_1"), code_item("a.vb",10,20,"ev_2")], 5);
    assert_eq!(ranked.len(), 1);
}
#[test]
fn requirement_contradicting_code_is_flagged() {
    let items = vec![requirement_item("ev_req","must reject admins"), code_item_content("ev_code","admins are allowed")];
    // detect_conflicts uses an authority-disagreement heuristic on overlapping entities;
    let conflicts = detect_conflicts(&items, 3);
    assert!(!conflicts.is_empty());
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement `ranking.rs`.**
  - **Dedup:** key = `symbol_id` if set, else `(path, lines)`, else `evidence_id`. Keep the higher-authority (lower enum) / higher-relevance instance; merge providers into a note if desired.
  - **directness default** (if provider didn't set): `GraphRelation`/exact-symbol → 0.9; `SourceCode` with symbol match → 0.7; `MemoryNote`/`DocSection` → 0.5; `SemanticSimilarity` authority → 0.2.
  - **score** = `0.40*authority.weight() + 0.30*directness + 0.15*relevance + 0.10*corroboration + 0.05*recency` where `corroboration` = min(1.0, count_of_items_sharing_entity/3), `recency` from timestamp via a 30-day halflife (reuse the `recency_bonus` formula, normalized to 0..1). **Authority+directness dominate (0.70 weight); similarity/relevance is ≤0.15.**
  - **Selection:** sort by score desc; then apply MMR *only among items already past an authority floor* — greedily add the next-highest-scoring item unless it is near-duplicate (same file within 5 lines, or same title) of one already chosen; stop at `cap`. This yields a small, source-diverse, high-signal set.
  - **`detect_conflicts`:** (a) snapshot mismatch — any item whose `generation` is `Some(g)` with `g != active_generation` pairs with a current item on the same entity → `kind:"snapshot_mismatch"`; (b) authority disagreement — a high-authority item (`ApprovedRequirement`/`RuntimeEvidence`) and a `CurrentCode` item that share an entity token but whose contents carry opposing polarity markers (a light heuristic: one contains a negation/│reject│deny and the other allow/permit for the same subject). Keep the heuristic conservative (favor false-negatives); conflicts are *surfaced*, never used to drop evidence.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit** — `"feat(ask_engine): authority/directness ranking + conflict detection (M1 task 7)"`.

---

### Task 8: Freshness snapshot + honest status assembly + push

**Files:**
- Modify: `crates/engram_server/src/services/ask_engine/status.rs`
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: `ProjectState`, `Registry`, `ProjectRecord`, `GraphStore`, `status::*`, `evidence::EvidenceItem`, `plan::QueryPlan`, `ProviderReport`.
- Produces: `pub async fn build_snapshot(ctx: &RetrievalCtx<'_>, rec: &ProjectRecord, as_of_branch: Option<&str>) -> FreshnessSnapshot`. `pub fn assess_status(plan: &QueryPlan, evidence: &[EvidenceItem], providers: &[ProviderReport], snapshot: &FreshnessSnapshot) -> AnswerStatus`.

- [ ] **Step 1: Write the failing tests**
```rust
#[test]
fn empty_everything_is_unsupported_not_answered() {
    let s = assess_status(&plan_query("how does frobnicate work"), &[], &[report_empty("code"), report_empty("doc")], &snap_default());
    assert_eq!(s, AnswerStatus::Unsupported);
}
#[test]
fn all_failed_providers_is_failed() {
    let s = assess_status(&plan_query("x"), &[], &[report_failed("code")], &snap_default());
    assert_eq!(s, AnswerStatus::Failed);
}
#[test]
fn ambiguous_entity_yields_ambiguous_status() {
    let mut p = plan_query("where is SaveMarker used");
    // inject 2 resolved branches on the entity
    p.entities[0].resolved = vec![re("a"), re("b")];
    let s = assess_status(&p, &[some_item()], &[report_hit("graph")], &snap_default());
    assert_eq!(s, AnswerStatus::Ambiguous);
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement.** `build_snapshot`: `project_generation = Some(ctx.generation)`; `git_commit = registry.get_meta(pid,"last_git_oid")?`; `git_branch = as_of_branch.map(str::to_string)`; `history_watermark = get_meta("pr_ingest_watermark").or(get_meta("total_commits")).and_then(|s| s.parse().ok())`; `last_index_ms = get_meta("last_index_completed_ms").parse`; `reindex_required = rec.reindex_required_since_ms.is_some()`; `semantic_tier = match ctx.search.semantic_quality() { Semantic=>"semantic", DegradedTrigram=>"degraded_trigram", Off=>"off" }`; `incompatible` set later by ranking (any snapshot_mismatch conflict). `assess_status` precedence: if all providers `Failed`/`TimedOut` and no evidence → `Failed`; else if any entity has >1 resolved branch AND that ambiguity spans the top evidence → `Ambiguous`; else if no evidence at/above `AgentMemory` authority → `Unsupported`; else if `reindex_required` or top evidence generation < active → `Stale`; else if coverage gaps exist (a `needed_evidence` kind produced nothing) → `Partial`; else `Answered`.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit + PUSH** — `"feat(ask_engine): freshness snapshot + calibrated status (M1 task 8)"`; push.

---

### Task 9: Request envelope extension

**Files:**
- Modify: `crates/engram_server/src/models/requests.rs` (`AskCodebaseRequest` at ~:696; add `AsOf`, `Audience`, `Depth` types + default fns)
- Test: append to `ask_engine_tests.rs`

**Interfaces:**
- Produces: extended `AskCodebaseRequest` (all new fields `#[serde(default)]`), `AsOf { branch: Option<String>, commit: Option<String> }`, `Audience { role: Option<String>, permissions: Vec<String> }`. `depth`/`freshness_policy`/`output_format` as `String` with default fns (`default_depth()->"standard"`, `default_freshness()->"best_effort"`, `default_output_format()->"markdown"`).

- [ ] **Step 1: Write the failing tests**
```rust
#[test]
fn legacy_request_still_deserializes() {
    let r: engram_server::AskCodebaseRequest =
        serde_json::from_str(r#"{"project_id":"p","question":"q"}"#).unwrap();
    assert_eq!(r.depth, "standard");
    assert_eq!(r.output_format, "markdown");
    assert!(r.as_of.is_none());
}
#[test]
fn full_envelope_deserializes() {
    let r: engram_server::AskCodebaseRequest = serde_json::from_str(r#"{
      "project_id":"p","question":"q","session_id":"s","task_context":"t",
      "as_of":{"branch":"main"},"audience":{"role":"developer","permissions":[]},
      "depth":"deep","freshness_policy":"require_current","output_format":"both","deadline_ms":15000
    }"#).unwrap();
    assert_eq!(r.depth, "deep");
    assert_eq!(r.as_of.unwrap().branch.as_deref(), Some("main"));
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement** — extend the struct (keep `#[serde(deny_unknown_fields)]`):
```rust
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskCodebaseRequest {
    pub project_id: String,
    pub question: String,
    #[serde(default)] pub session_id: Option<String>,
    #[serde(default)] pub task_context: Option<String>,
    #[serde(default)] pub as_of: Option<AsOf>,
    #[serde(default)] pub audience: Option<Audience>,
    #[serde(default = "default_depth")] pub depth: String,
    #[serde(default = "default_freshness")] pub freshness_policy: String,
    #[serde(default = "default_output_format")] pub output_format: String,
    #[serde(default)] pub deadline_ms: Option<u64>,
}
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AsOf { #[serde(default)] pub branch: Option<String>, #[serde(default)] pub commit: Option<String> }
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Audience { #[serde(default)] pub role: Option<String>, #[serde(default)] pub permissions: Vec<String> }
fn default_depth() -> String { "standard".into() }
fn default_freshness() -> String { "best_effort".into() }
fn default_output_format() -> String { "markdown".into() }
```
Add doc-comments per field (they become the JSON-schema descriptions; escape any inner quotes). Validate `depth`/`output_format` against allowed sets in the handler (Task 10), not serde, so an unknown value degrades gracefully rather than hard-rejecting.

- [ ] **Step 4: Run to verify pass.**

- [ ] **Step 5: Commit** — `"feat(ask_codebase): rich request envelope (session/as_of/depth/output) (M1 task 9)"`.

---

### Task 10: Orchestrator — rewrite `handle_ask_codebase`; JSON+Markdown render

**Files:**
- Modify: `crates/engram_server/src/handlers/ask_tools.rs` (replace body of `handle_ask_codebase`; delete `classify()`/`Intent` or keep privately unused-removed)
- Modify: `crates/engram_server/src/services/ask_engine/report.rs` (render fns)
- Modify: `crates/engram_server/src/tools.rs` (update `ask_codebase` description)
- Test: append integration tests to `ask_engine_tests.rs`

**Interfaces:**
- Consumes: everything above. Produces: the rebuilt handler + `report::render_markdown(&AskReport) -> String` and `report::to_json(&AskReport) -> serde_json::Value`.

- [ ] **Step 1: Write the failing integration tests**
```rust
#[tokio::test]
async fn ask_returns_typed_report_with_citations_on_indexed_project() {
    let (_state, engram, pid) = index_fixture(&[
        ("Auth.vb","Public Function Authenticate() As Boolean\n Return True\nEnd Function\n"),
    ]).await;
    let res = engram.handle_ask_codebase(engram_server::AskCodebaseRequest{
        project_id: pid, question: "how does Authenticate work?".into(),
        session_id: None, task_context: None, as_of: None, audience: None,
        depth: "standard".into(), freshness_policy: "best_effort".into(),
        output_format: "both".into(), deadline_ms: None,
    }).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("retrieval_only"));
    assert!(text.contains("Auth.vb"));       // citation present
    assert!(text.contains("status:"));        // honest status line
}
#[tokio::test]
async fn ask_abstains_when_knowledge_is_absent() {
    let (_s, engram, pid) = index_fixture(&[("a.vb","Public Sub Noop()\nEnd Sub\n")]).await;
    let res = engram.handle_ask_codebase(req(pid, "what is the flux capacitor calibration policy?")).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("unsupported") || text.contains("Unsupported"));
    assert!(!text.contains("#1"));  // NOT the old concat format
}
```

- [ ] **Step 2: Run to verify fail.**

- [ ] **Step 3: Implement the orchestrator.**
```rust
pub async fn handle_ask_codebase(&self, req: AskCodebaseRequest) -> Result<CallToolResult, McpError> {
    crate::handlers::validate_project_id(&req.project_id)?;
    if req.question.trim().is_empty() { return Err(McpError::invalid_params("question must not be empty".into(), None)); }
    let rec = self.ensure_project_record(&req.project_id).await?;
    let ps = self.ensure_project_runtime(&req.project_id).await?;
    let gen_ = self.get_active_generation(&req.project_id).await?;
    let depth = parse_depth(&req.depth);                 // unknown → Standard
    let deadline = Duration::from_millis(req.deadline_ms.unwrap_or(15_000).clamp(1_000, 60_000));
    let cancel = tokio_util::sync::CancellationToken::new();

    let mut plan = planner::plan_query(&req.question);
    { let g = self.state.graph.clone(); let pid = req.project_id.clone();
      let mut p2 = plan.clone();
      plan = tokio::task::spawn_blocking(move || { resolver::resolve_entities(&g, &pid, &mut p2); p2 })
             .await.map_err(|e| McpError::internal_error(e.to_string(), None))?; }

    let ctx = RetrievalCtx { search: ps.search.clone(), graph: self.state.graph.clone(),
        registry: self.state.registry.clone(), project_id: req.project_id.clone(), generation: gen_ };
    let (raw, providers) = retrieval::gather_evidence(&ctx, &plan, depth, deadline, cancel).await;
    let mut evidence = ranking::rank_and_select(raw, depth.evidence_cap());
    let conflicts = ranking::detect_conflicts(&evidence, gen_);
    let mut snapshot = status::build_snapshot(&ctx, &rec, req.as_of.as_ref().and_then(|a| a.branch.as_deref())).await;
    snapshot.incompatible = conflicts.iter().any(|c| c.kind == "snapshot_mismatch");
    let st = status::assess_status(&plan, &evidence, &providers, &snapshot);
    let unknowns = report::coverage_gaps(&plan, &providers);
    let next_best = report::next_best(&plan, &evidence, st);
    let report = AskReport { question: req.question.clone(), plan, status: st, mode: "retrieval_only".into(),
        evidence: std::mem::take(&mut evidence), conflicts, unknowns, next_best, snapshot, providers };

    let body = match req.output_format.as_str() {
        "json" => serde_json::to_string_pretty(&report::to_json(&report)).unwrap_or_default(),
        "both" => format!("{}\n\n```json\n{}\n```", report::render_markdown(&report),
                          serde_json::to_string_pretty(&report::to_json(&report)).unwrap_or_default()),
        _ => report::render_markdown(&report),
    };
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
```
`render_markdown`: leads with `# ask_codebase — retrieval_only report`, then `understanding:` (intents + resolved entities), `status: <status>` + one-line rationale, `snapshot:` (generation/commit/tier/reindex flag), `## Key evidence` (each: `authority · path:lines · provider` + a short snippet — **cite, don't dump**; cap to the selected set), `## Conflicts` (if any), `## Unknowns / coverage gaps`, `## Next best investigation`. Never emit the old `#1`/`doc_id` markers. `coverage_gaps`: `plan.needed_evidence` kinds with zero produced items. `next_best`: e.g. "resolve_id(\"X\") — entity ambiguous", "index git history — history arm empty".

- [ ] **Step 4: Update the tool description** in `tools.rs` (ask_codebase): describe the real contract — "Deterministic, typed, authority-ranked evidence report (retrieval_only): multi-intent planning, entity resolution, parallel retrieval, conflict + freshness aware, honest status (answered/partial/ambiguous/stale/unsupported/failed). Optional envelope: depth, as_of, output_format(markdown|json|both), deadline_ms. START HERE." Keep it accurate — no synthesis claim (that's M2).

- [ ] **Step 5: Run to verify pass** — plus run the pre-existing `ask_tools` classifier tests; delete or update them (the old `classify()` tests in `ask_tools.rs` will fail to compile once `classify` is removed — port the intent expectations into `planner` tests or delete).

- [ ] **Step 6: Full crate check** — `CARGO_BUILD_JOBS=2 cargo test -p engram_server` (whole crate, catch breakage in the integration_test that touches ask_codebase). Fix any callers.

- [ ] **Step 7: Commit** — `"feat(ask_codebase): rewrite over the deterministic evidence engine (M1 task 10)"`.

---

### Task 11: Seed golden eval + live probe on OciusX + push

**Files:**
- Create: `eval/ask_engine_golden.jsonl` (seed Q&A)
- Create: `eval/ask_engine_golden.py` (runner)
- Test: (manual/live — not a unit test)

**Interfaces:**
- Produces: a runnable baseline-vs-new comparison + documented result.

- [ ] **Step 1: Write the seed corpus** `eval/ask_engine_golden.jsonl` — ~12 OciusX questions across categories, each: `{"q": "...", "category": "exact_fact|multi_hop|impact|rationale|ambiguous|compound|missing_knowledge", "expect_status": "answered|partial|ambiguous|unsupported", "must_cite_any": ["path substring", ...], "must_abstain": false}`. For `missing_knowledge` rows, `expect_status: "unsupported"`, `must_abstain: true`. Use generic/anonymized phrasings; no customer strings.

- [ ] **Step 2: Write the runner** `eval/ask_engine_golden.py` — drives the daemon via `tools/engram_drive.py tool ask_codebase '{...}'` (per the deploy notes; stdout UTF-8), parses the JSON block (`output_format:"json"`), and scores: citation-coverage (fraction of `must_cite_any` satisfied), correct-abstention (missing_knowledge rows returning unsupported/abstain), status-match, latency. Print a table. (Runs against a deployed build — STOP the daemon, copy the release binary per the deploy runbook, restart, then run.)

- [ ] **Step 3: Establish the baseline** — check out the pre-rebuild `ask_codebase` (or record its outputs before Task 10 lands) and run the corpus; save `eval/ask_engine_baseline.md`. If the baseline was not captured pre-rebuild, note that and compare against the S1_ask eval framing instead.

- [ ] **Step 4: Run the new engine** on the corpus; save `eval/ask_engine_m1.md`. **Promotion gate:** new engine must (a) abstain correctly on 100% of missing_knowledge rows, (b) match `expect_status` on ≥80% of rows, (c) never emit an unsupported claim (trivially true in M1 — retrieval_only), and (d) not regress citation-coverage vs baseline on the answerable rows.

- [ ] **Step 5: Commit + PUSH + STOP for review** — `"feat(ask_engine): seed golden eval + OciusX live probe (M1 task 11)"`; push. **This is the Milestone-1 review checkpoint** — do not start M2 (LLM synthesis/verifier) until the user reviews the M1 result.

---

## Self-Review

**Spec coverage:** Each spec §(1–10) maps to a task — evidence model→T1; providers→T4/T5; multi-intent/multi-entity planner→T2; entity resolution→T3; retrieval DAGs→T6; rank/conflict/anti-anchoring→T7; status+snapshot→T8; request envelope→T9; JSON+Markdown output→T10; seed golden eval→T11. Security posture (untrusted repo content) is a global constraint honored by never interpreting content as instructions. Deferred M2/M3 items are explicitly out of scope.

**Placeholder scan:** Provider/retrieval steps reference verified signatures (api_notes) rather than "implement appropriately"; conflict-detection heuristic is defined conservatively; no "TBD"/"similar to Task N".

**Type consistency:** `EvidenceItem` fields, `Authority`/`EvidenceKind` variants, `ProviderStatus`/`AnswerStatus`, `RetrievalCtx`, `Depth`, and the request fields are named identically across T1→T10. `gather_evidence`/`rank_and_select`/`assess_status`/`build_snapshot` signatures match their call sites in T10.
