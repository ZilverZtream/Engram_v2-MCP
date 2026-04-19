# Dashboard Plan 5 — Settings + Docs + Release Polish

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close out v1 — ship the Settings lens (rules / watches / memory / boundaries / ignore patterns / embedder / rollout / export-import / appearance), write first-run + EQL + WS + security docs, codify the manual smoke checklist, enforce performance budgets in CI, and run the final self-review against the spec. After this plan, the dashboard is ready to cut a v1.0 tag.

**Architecture:** Settings mostly reuses the Registry CRUD already present in `engram_core` — the lens is a typed form layer over existing stores. Docs are generated from OpenAPI (API reference) plus hand-written guides. Smoke checklist is a Markdown document exercised in manual release review.

**Spec sections:** §4.9 Settings, §9.6 Docs, §10.5 Performance budgets.

**Prerequisite:** Plans 1–4 complete.

---

## File map

**Created (backend):**
- `crates/engram_dashboard/src/routes/settings.rs`
- `crates/engram_dashboard/src/routes/export_project.rs`
- `crates/engram_dashboard/tests/settings_api_tests.rs`
- `crates/engram_dashboard/tests/export_import_tests.rs`

**Created (frontend):**
- `web/src/lib/components/settings/RulesEditor.svelte`
- `web/src/lib/components/settings/WatchesEditor.svelte`
- `web/src/lib/components/settings/MemoryBankEditor.svelte`
- `web/src/lib/components/settings/BoundariesEditor.svelte`
- `web/src/lib/components/settings/IgnorePatternsEditor.svelte`
- `web/src/lib/components/settings/EmbedderConfig.svelte`
- `web/src/lib/components/settings/RolloutControl.svelte`
- `web/src/lib/components/settings/ExportImport.svelte`
- `web/src/lib/components/settings/Appearance.svelte`
- `web/src/routes/settings/+page.svelte` (real)

**Created (docs):**
- `docs/dashboard/eql-reference.md`
- `docs/dashboard/ws-events.md`
- `docs/dashboard/security.md`
- `docs/dashboard/api-reference.md` (generated from OpenAPI)
- `docs/dashboard/smoke-checklist.md` (update from Plan 1)
- `crates/engram_dashboard/web/src/routes/docs/+page.svelte` (embedded help viewer — optional)

**Modified:**
- `crates/engram_dashboard/src/server.rs` (merge settings router)
- `crates/engram_dashboard/src/routes/mod.rs`
- `README.md` (add dashboard section)
- `.github/workflows/dashboard.yml` (or equivalent): add perf-budget job

---

## Task 1 — Settings: rules CRUD

**Files:** `routes/settings.rs`, test.

- [ ] **Step 1:** Existing Registry exposes rules (`Registry::list_rules / create_rule / update_rule / delete_rule`, per `MEMORY.md` §Capabilities). Verify method signatures and wire up:

```
GET    /api/v1/settings/rules?project=X
POST   /api/v1/settings/rules
PATCH  /api/v1/settings/rules/:id
DELETE /api/v1/settings/rules/:id
```

All under CSRF. Every write appends to `dashboard_ops` via the same audit layer used by Plan 4.

- [ ] **Step 2:** Failing test + implementation + run. Commit. `feat(dashboard): rules settings CRUD`.

---

## Task 2 — Settings: watches CRUD

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:** Same pattern as rules:

```
GET    /api/v1/settings/watches?project=X
POST   /api/v1/settings/watches          body: { directory, enabled }
PATCH  /api/v1/settings/watches/:id
DELETE /api/v1/settings/watches/:id
```

Creating a watch triggers an `AppEvent::WatchUpdate` through the existing `events_tx` (not `dashboard_events_tx`), so the watcher actor picks it up. Confirm this is still how the system learns about new watches.

- [ ] **Step 2:** Commit. `feat(dashboard): watches settings CRUD`.

---

## Task 3 — Settings: memory bank CRUD

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:**

```
GET    /api/v1/settings/memory?project=X
POST   /api/v1/settings/memory           body: { section, title, content }
PATCH  /api/v1/settings/memory/:id
DELETE /api/v1/settings/memory/:id
```

Operates on memory_bank nodes in the graph (per node-types list in `MEMORY.md`). Use `GraphStore` upsert / delete with `NodeKind::memory_bank_section`.

- [ ] **Step 2:** Commit. `feat(dashboard): memory bank CRUD`.

---

## Task 4 — Settings: boundaries.yaml editor

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:**

```
GET /api/v1/settings/boundaries?project=X  → { yaml: string, parsed: object }
PUT /api/v1/settings/boundaries            body: { project, yaml: string }
```

The PUT handler validates the YAML against the schema `engram_server::services` uses, rejects with 400 on parse/validation failure, writes to the config path, and emits a reload event.

- [ ] **Step 2:** Commit. `feat(dashboard): boundaries.yaml editor endpoint`.

---

## Task 5 — Settings: ignore patterns

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:**

```
GET /api/v1/settings/ignore-patterns?project=X  → { patterns: [string] }
PUT /api/v1/settings/ignore-patterns             body: { project, patterns: [string] }
```

- [ ] **Step 2:** Commit. `feat(dashboard): ignore patterns editor endpoint`.

---

## Task 6 — Settings: embedder config

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:**

```
GET /api/v1/settings/embedder                → { mode: "projection"|"local"|"ollama"|"openai", ... }
PUT /api/v1/settings/embedder                body: new config
POST /api/v1/settings/embedder/test          body: new config → attempts a handshake, returns ok/err
```

The PUT path persists to the runtime config; a restart is required for some modes (return an `advice: "restart required"` field when that's the case).

- [ ] **Step 2:** Commit. `feat(dashboard): embedder config endpoints`.

---

## Task 7 — Settings: rollout / kill-switch

**Files:** `routes/settings.rs` (extend).

- [ ] **Step 1:**

```
GET  /api/v1/settings/rollout                → { adp_kill_switch: bool, effective_from: ts }
POST /api/v1/settings/rollout                body: { action: "kill-switch"|"enable" }
```

Backed by the existing `AppState::adp_kill_switch` atomic and the Registry-persisted flag.

- [ ] **Step 2:** Commit. `feat(dashboard): rollout/kill-switch endpoints`.

---

## Task 8 — Settings: export + import project state

**Files:** `routes/export_project.rs`.

- [ ] **Step 1:**

```
GET  /api/v1/settings/export?project=X      → streams a tarball: nodes + edges + docs + vectors + rules + watches + memory + boundaries
POST /api/v1/settings/import                multipart: tarball + { target_project_id, overwrite: bool }
```

Export: compose per-table EQL queries that yield the full content; tar them into `ndjson` files + a `manifest.json` (version, timestamp, source project id). Gzip.

Import: validate manifest (version matches), run in a single Redb transaction where possible, emit a `job_progress` stream.

- [ ] **Step 2:** Tests: round-trip export → import into a fresh project → assert graph counts match.

- [ ] **Step 3:** Commit. `feat(dashboard): project export + import endpoints`.

---

## Task 9 — Frontend: Settings shell + tabs

**Files:** `web/src/routes/settings/+page.svelte`.

- [ ] **Step 1:** Tabs along the top: Rules · Watches · Memory · Boundaries · Ignore patterns · Embedder · Rollout · Export/Import · Appearance.

- [ ] **Step 2:** Each tab mounts its own component. URL state: `?tab=rules` preserved.

- [ ] **Step 3:** Commit. `feat(dashboard-ui): Settings shell with 9 tabs`.

---

## Task 10 — Frontend: Settings tab components

- [ ] **Step 1:** `RulesEditor.svelte` — list + inline edit + add + delete, wired to `/settings/rules`.
- [ ] **Step 2:** `WatchesEditor.svelte` — similar, with "directory" picker (free-text path + client-side path normalization).
- [ ] **Step 3:** `MemoryBankEditor.svelte` — list + rich-text area (Markdown preview via `marked`).
- [ ] **Step 4:** `BoundariesEditor.svelte` — full-page CodeMirror YAML editor with save button + validation errors surfaced.
- [ ] **Step 5:** `IgnorePatternsEditor.svelte` — textarea (one pattern per line) + preview counter ("would ignore N paths under current project").
- [ ] **Step 6:** `EmbedderConfig.svelte` — select mode + per-mode inputs (host/model/api-key-masked), "Test connection" button.
- [ ] **Step 7:** `RolloutControl.svelte` — big red kill-switch toggle with confirmation modal; current state badge.
- [ ] **Step 8:** `ExportImport.svelte` — "Download project backup" button + "Import" file picker with target-project-id input and overwrite checkbox.
- [ ] **Step 9:** `Appearance.svelte` — theme toggle (dark default; light variant for later — scaffold only), density control (cozy/compact).

One commit per component: `feat(dashboard-ui): <component>`.

---

## Task 11 — Frontend: First-run empty state

**Files:** `web/src/routes/+page.svelte` (augment Plan 2 overview).

- [ ] **Step 1:** On Overview load, if `projects.length === 0`, render a dedicated first-run view:
  - Hero title "Welcome to Engram Dashboard."
  - Three CTAs: "Index a directory" (form to call `analyze_full_project_migration` or the existing indexing tool) · "Open docs" · "Learn more about EQL."

- [ ] **Step 2:** Commit. `feat(dashboard-ui): first-run empty state on Overview`.

---

## Task 12 — Docs: first-run guide

**Files:** `docs/dashboard/first-run.md` (replace stub from Plan 1).

- [ ] **Step 1:** Contents: what it is, how to start (`engram dashboard`, flags, DASHBOARD_AUTOSTART), what each lens does (one sentence each), project switcher basics, dev mode.

- [ ] **Step 2:** Commit. `docs(dashboard): first-run guide`.

---

## Task 13 — Docs: EQL reference

**Files:** `docs/dashboard/eql-reference.md`.

- [ ] **Step 1:** Full reference: grammar, operators, types, per-table schemas (pulled from `schema::all()` — either handwritten or generated by a small build-time script that reads `schema::all()` and renders Markdown), examples per table, common anti-patterns (OR across two indices, full scans without LIMIT).

- [ ] **Step 2:** Commit. `docs(dashboard): EQL reference`.

---

## Task 14 — Docs: WS events reference

**Files:** `docs/dashboard/ws-events.md`.

- [ ] **Step 1:** For each `DashboardEvent` variant: description, schema, rate limits, example frame.

- [ ] **Step 2:** Topic-matching rules, reconnect behavior, back-pressure signals.

- [ ] **Step 3:** Commit. `docs(dashboard): WebSocket events reference`.

---

## Task 15 — Docs: security model

**Files:** `docs/dashboard/security.md`.

- [ ] **Step 1:** Summarize the security section from the spec: loopback default, remote mode with bearer token, CSRF + Origin check, CSP headers, rate limits, what's out of scope.

- [ ] **Step 2:** Include a runnable threat-model checklist with mitigations.

- [ ] **Step 3:** Commit. `docs(dashboard): security notes`.

---

## Task 16 — Docs: API reference (generated)

**Files:** `docs/dashboard/api-reference.md`, a small script.

- [ ] **Step 1:** Add a build-time step: `cargo run --quiet -p engram_dashboard --bin emit_openapi > /tmp/openapi.json && node scripts/openapi-to-md.mjs /tmp/openapi.json > docs/dashboard/api-reference.md`. The Node script walks the spec and emits a Markdown reference per path.

- [ ] **Step 2:** Wire into CI so the doc stays current (failing the job if the generated file differs from committed — forces a rebuild + commit on API changes).

- [ ] **Step 3:** Commit. `docs(dashboard): auto-generated API reference`.

---

## Task 17 — Smoke checklist finalized

**Files:** `docs/dashboard/smoke-checklist.md`.

- [ ] **Step 1:** One checklist item per lens × core flow. Example excerpt:

```
Overview
  [ ] KPI tiles render with non-zero counts on a real indexed project.
  [ ] Hotspots top-10 links resolve to Inspector.
  [ ] Recent calls list updates live when a tool is invoked.
Graph explorer
  [ ] Typing in search finds a known method.
  [ ] Clicking a node opens the detail pane with callers/callees.
  [ ] Neighbors expand without stalling on a 2000-neighbor node.
Inspector
  [ ] Opening a known red-edit-safety method shows all 8 tabs populated.
  [ ] "Prepare implementation context" copies a non-empty dossier.
Tool runner
  [ ] Every group is browsable; a sampled tool from each group runs to completion.
Migration
  [ ] Report renders a Phase 32/33 output unchanged vs direct MCP call.
  [ ] Rollout kill-switch flips state; status reflects in header badge.
Business logic
  [ ] Streaming answer renders chunks incrementally; citations appear on done.
Data browser
  [ ] Typed EQL round-trips through the builder without corruption.
  [ ] CRUD: create → update → delete → undo returns original row.
  [ ] Export CSV yields a file with headers matching columns.
Activity log
  [ ] Stream shows live events; filter by tool name reduces the feed.
Settings
  [ ] Each CRUD tab is usable; destructive actions require confirmation.
```

- [ ] **Step 2:** Commit. `docs(dashboard): finalized smoke checklist`.

---

## Task 18 — Perf budgets enforced in CI

**Files:** CI config.

- [ ] **Step 1:** Add a `perf` job that runs the ignored perf tests (`cargo test -p engram_dashboard -- --ignored perf_ --nocapture`) with a fixture project of 100k nodes built at job setup.

- [ ] **Step 2:** On failure, job fails with the measured latency in the log.

- [ ] **Step 3:** Commit. `ci(dashboard): perf budgets enforced`.

---

## Task 19 — README integration

**Files:** root `README.md`.

- [ ] **Step 1:** Add a short section:

```markdown
## Dashboard (optional)

Engram ships an optional browser workbench. Start it with:

    engram dashboard

Opens http://localhost:<auto> in your browser. See [docs/dashboard/first-run.md](docs/dashboard/first-run.md) for flags and lens reference.
```

- [ ] **Step 2:** Commit. `docs: mention dashboard in README`.

---

## Task 20 — Release checklist

**Files:** `docs/dashboard/release-checklist.md` (internal, not user-facing).

- [ ] **Step 1:** Checklist:

```
[ ] All tests pass on main: cargo test --all && cargo test -p engram_dashboard
[ ] Frontend build clean: pnpm -C crates/engram_dashboard/web build
[ ] Smoke checklist completed against an indexed 20k-node fixture
[ ] Perf budgets pass
[ ] OpenAPI spec committed, api-reference.md regenerated
[ ] CHANGELOG entry under "Dashboard v1" with feature list
[ ] Version bump on engram_dashboard (0.1.0 → 1.0.0)
[ ] Tag v1.0.0-dashboard
```

- [ ] **Step 2:** Commit. `docs(dashboard): release checklist`.

---

## Task 21 — Final self-review across all 5 plans

- [ ] **Step 1:** Read the spec section-by-section. For each section, list the task(s) that implement it. Gaps → open issue or add task.

- [ ] **Step 2:** Run the full CI pipeline locally (release build, frontend build, all tests, smoke, perf). If anything fails, fix before cutting the release.

- [ ] **Step 3:** Confirm `docs/dashboard/smoke-checklist.md` passes on a real project end-to-end.

- [ ] **Step 4:** Tag the release.

**Completion gate:** Dashboard v1 shipped. Developer can start the workbench, use every lens against a real project, edit data with an audit trail, recover from a bad edit via undo, and share a URL link to a specific inspector view with a colleague on the same machine.

---

## Post-v1 (out of scope here, for future planning)

- Playwright E2E suite.
- Durable activity history in Redb (current: 10k in-memory ring).
- `join on edge_kind = ...` in EQL (current: no joins).
- Mobile-responsive design.
- Multi-user auth / role separation.
- Live-reload hot theme switching.
- Native macOS/Windows wrapper (Tauri) — redundant given single-binary model, but could bundle an icon + menu bar integration.
