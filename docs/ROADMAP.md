# Roadmap (status-aligned)

Status labels: `implemented`, `partial`, `experimental`, `planned`.

## Current execution checkpoint (2026-11-13)

The original OciusX improvement backlog remains active. Use
[the delivery goal](../eval/engram-delivery-goal-20261110.json), its current
execution focus and [TODO](../TODO.md) for active priorities and retained gaps.
CodeMap and Blast Radius are closed for improvement except an explicitly requested
change or a concrete regression caused by current work. Kernel/SSD investigation
and deletion of preexisting folders remain held.

Recent delivery records distinguish installed behavior from partial acceptance:
[default search disclosure](../eval/ociusx-defaults-delivery-20261113.json),
[public MCP contracts](../eval/ociusx-mcp-contract-delivery-20261113.json),
[source/schema request mapping](../eval/ociusx-request-dbml-mapping-delivery-20261112.json)
and [unfinished feature-plan semantic acceptance](../eval/ociusx-feature-plan-installed-partial-20261113.json).
Hosted CI remains unverified while repository Actions are disabled. These records
do not establish complete backlog delivery or an overall productivity improvement.

Dated observations and next-step proposals below are historical evidence. They
preserve unfinished findings but do not override the current selection or holds.

## Historical checkpoint (2026-11-04)

Audit status (2026-11-04): **OPEN**. The earlier goal was marked complete
prematurely. Deployed fixes and the seven synthetic scenario runs are completed work;
partial, unresolved and deferred findings below remain unfinished audit work.
Deferral does not mean completion or an approved reduction in scope.

Prioritize improvements to the existing planning, retrieval, and review tools.
Another tool adds little value when applicable guidance is missed or routine
indexing consumes the machine.

Local resource evidence updated 2026-11-04: the optimized `72a2d3d7` candidate
used 48.94% fewer process logical writes during the 40 matched edit/update operations
than the saved installed control, with sampled peak private memory falling
from 105 to 72 MiB. A separate 500-edit workload, including ten explicit
collections and restart/query checks, peaked at 107.93 MiB and ended at
76.25 MiB; post-collection growth between edits 250 and 500 was 4.375 MiB.
Post-workload five-second idle windows recorded no logical writes and
0–0.015625 CPU seconds.
These are local workload observations, not physical SSD-write measurements,
a whole-machine leak attribution, or proof of real-project productivity.
The first `f8239c8c` runtime and parser-v4 package were installed on 2026-11-04
from source published as `d0a6112` on `origin/master`. OciusX was the live
deployment priority: conversion recovered 199,845 retained vectors, then a
non-wiping full repair published generation 3117 with 2,302 files and 31,915
source chunks. Installed normal-runtime lexical/native retrieval and all 94
OciusX repository rules were verified. The other 14 projects are preserved
and pending conversion; their source-tool availability is not claimed.

The real full repair peaked at 1,249,169,408 private bytes, exceeding its
768 MiB diagnostic threshold. Cold vector queries peaked at 842,620,928 bytes,
exceeding their 512 MiB threshold, then settled to 243,388,416 bytes. These
alerts remain open optimization work. A separate 60-second quiet window in
the installed normal daemon recorded zero process logical writes and 0.15625
CPU seconds, ending at 661,082,112 private bytes. This short observation is
not a sustained leak test, a physical SSD measurement, or a matched benchmark.
The old 67 GB canonical native store and complete old installation remain
retained; preservation is not cleanup authority or old-runtime compatibility.
The earlier measurements below are historical stages of this work.

The current installed `79c320d8` package is published as source `b515a5e`.
Startup selection now skips unproven projects before opening their optional
indexes or consuming warm-up slots. Native vector queries use fragment
read-ahead of one while retaining the SDK's I/O budget, exact filters and
cosine scoring. Twelve focused behavior checks passed; no full suite was
repeated for this batch.

One controlled OciusX four-query pair returned identical responses before
and after this change, preserving all 255 selected raw Registry records,
known catalogs/routes and the old canonical store's metadata. Primary CPU
fell from 20.953125 to 12.890625 seconds. Sampled peak private memory fell
from 894,033,920 to 798,871,552 bytes; after ten seconds of settling it fell
from 554,307,584 to 94,425,088 bytes. Both peaks still exceed the unchanged
512 MiB diagnostic threshold. Repeated queries slowed from 1.32–1.41 to
2.30–2.31 seconds; process logical writes remained 10,661 bytes in each run.
This records an accepted CPU/memory versus latency tradeoff, not a universal
performance gain or a hard memory cap.

The installed normal runtime then passed current OciusX lexical/native
retrieval and exposed all 94 repository rules. Its separate 60-second
post-query window recorded 96,136 process logical write bytes and 0.484375
CPU seconds, ending at 381,403,136 private bytes (about 364 MiB). This is a
short normal-runtime observation, separate from the controlled actorless
pair. The original configuration is unchanged; both previous installation
pairs remain retained. Peak-memory optimization, other before-allocation
coverage and the broader audit remain open.

| Priority | Work | Acceptance and limits |
|---|---|---|
| 1 | Resource stability (`partial`) | Immutable source reuse, cache bounds, watcher filtering and explicit vector maintenance are deployed. OciusX conversion and installed normal retrieval passed; the short quiet window above recorded no logical writes. Real full-repair/cold-query memory alerts, broader before-allocation coverage and sustained real-project observation remain open. Other 14 conversions are pending and do not block OciusX. Kernel-pool attribution remains unresolved and on hold at the user's direction. |
| 2 | Scoped guidance in planning and review (`implemented`) | Applicable authored instructions reach `get_change_set`, `prepare_implementation_context`, source `get_chunk`, and `pre_commit_review`; exceptions remain intact, omissions are explicit and recoverable, and historical requests exclude live policy. Retrieval does not certify prose compliance. |
| 3 | Preserve scope when ingesting quality rules (`implemented` for receipt-owned imports) | Explicit JSON patterns, relative source identities and source-owned removal/reordering are deployed. Existing matching semantics remain; legacy records require ownership review before migration. |
| 4 | Instruction freshness and maintenance (`partial`) | Scoped dependencies, historical/current evidence and hash-bound update/area-creation proposals have 45 public cases passing in each build and are included in the deployed package. Real reporting no-change, present KML code/policy conflict and historical marker API change examples passed proposal validation and independent review. The historical change reproduces the accepted guide exactly. Broader relevance and comparative usefulness remain unfinished. |
| 5 | Task-level outcome evaluation (`partial`) | Seven synthetic scenarios and eight fresh paired real-source planning/diagnosis runs are recorded. Independent blinded judging found 20/20 criteria in each condition; automatic context reduced evidence IDs from 10 to 6 but increased delivered bytes by 23.1%. This establishes no quality or implementation/QA productivity uplift. Regular verification and broader workflow evidence remain open. |

The historical installed baseline and the current package have different storage
behavior. The current package selects immutable file revisions; native desired-
behavior tests cover inherited lexical/vector results, held readers and collection.
The undeployed `05a58288` candidate completed 250 distinct-file edits and restart
within the protective workload limits. Against the historical installed control's
same first 240 edit/search groups, it used about 6% less CPU and 59% fewer process
logical writes, with 45% lower wall time. The fresh 40-edit comparison improved CPU
by 48%; that small-run result must not be substituted for the sustained result.
Memory still grows and search cost rises with revision history. A subsequent
PageRank retention fix has passed native and no-vector behavior tests. Its `fc5a13ff`
250-edit run reduced peak private memory by 43% and the fresh store footprint by
81% versus `05a58288`, with essentially unchanged CPU and logical write volume.
Memory still grows; the remaining retention and query costs need attribution.
Page-cache reservations are per store and do not establish a process RSS ceiling.
Process logical I/O counters do not measure physical SSD wear.

The later undeployed `ed13dbb1` 250-edit run reduced peak private memory by
8.4%, settled private memory by 13.8%, and edit/query CPU by 8.7% versus `fc5a13ff`;
logical write volume was effectively unchanged. Peak/settled private memory was
215.4/184.2 MiB. Late memory still rose by about 34 MiB per 100 edits. Distinct
edits also increased the current selected revision groups, so this establishes
neither a plateau nor a leak attribution. Four fresh native processes confirmed
version-dependent cache retention and effective weighted eviction; lowering a
diagnostic cache budget did not remove most append-case private growth. Production
cache budgets were left unchanged.

The existing `incremental_indexing_gc(compact_vectors=true)` option previously
reported compaction without executing it. The working tree now runs explicit
native compaction and reports completed manifest-fragment counts. Native checks
reduced eight fragments to one while preserving exact rows, old readers,
cancellation fencing, reopen and the next edit; the public handler reduced three
to one without changing its source head. The no-vector build reports a skip.
Old native versions remain available, so fragment counts are not reclaimed disk
bytes.

A subsequent native batch verifies separate history pruning after explicit
compaction. It requires exclusive maintenance admission, no physical readers or
unresolved backend work, and bounded validation of the owned flat table's entire
manifest/reference inventory. Tags, unknown files, unsupported metadata, and
inspection limits preserve history. Fresh tables disable Lance's scheduled
cleanup; existing managed tables migrate its exact known defaults under retained
write authority. Thirty focused native checks pass, including actual version-20
migration, aged unknown-file and reader preservation, cancellation, history
removal, reopen, and the next edit. Actual no-vector, broader release and
sustained process-benefit checks remain separate gates. Further inventory and
decoder accounting work is in progress; this paragraph establishes no hard RSS
bound or physical SSD write reduction.

In one subsequent control-first pair, both binaries completed 200 edits cycling
50 files, four explicit collection checkpoints, all-file source/vector checks
and a process restart. The `530b90ab` candidate performed real compaction
(53 fragments to one, then 51 to one at each remaining checkpoint), with
10.4% lower peak and 18.3% lower settled private memory than `ed13dbb1`.
Late private memory still grew 14.8 MiB per 100 edits. Collection consumed more
CPU and logical writes; store size and file count did not fall. The small total
logical-write difference establishes no meaningful write reduction. This
synthetic pair does not establish a plateau, physical SSD writes, or a general
production-workload improvement. Both owned fixtures were removed.

A later native debug observation requested 500 edits cycling the same 50 paths,
with an unchanged sibling and explicit collection every 50 edits. Its unchanged
900-second watchdog stopped it after 327 edits. All six collection checkpoints
verified all 51 current lexical/native declarations and absence of obsolete ones;
the restart and remaining checkpoints were not reached. After-verification
private memory rose from 82.7 MiB at edit 50 to 126.2 MiB at edit 300, while
selected source groups stayed at 51. Approximate native session items also grew;
that correlation does not identify allocation ownership or prove a leak.
The owned process and helper threads closed and its fixture was removed.
This debug diagnostic establishes neither a plateau nor optimized throughput.

That run recorded 662 native delete commits among 994 commits. Locked library
inspection showed that a delete matching no rows still creates a native version,
transaction and manifest. Engine-owned cleanup now reopens the latest table under
the write lock and performs a bounded existence check before deleting. Two native
behavior cases verify unchanged versions and metadata-file hashes for empty
staging and collection, plus real abandoned-row cleanup, queued writers, held
readers, reopening and the next edit. The public raw-table API retains its existing
delete semantics. These behavior cases alone establish no process-level memory,
CPU or write-volume benefit; resource growth remains unresolved.

The subsequent native debug process run confirmed the transaction suppression:
100 edits produced 100 updates and 100 real retention deletes, with no empty
staging or zero-cutoff deletes. Both collection checkpoints and all 51 sources
survived a complete restart check. A longer run completed all 500 edits and ten
collection/source checks, but its unchanged 900-second limit interrupted restart
verification. Private memory still grew 30.9 MiB between edits 250 and 500, while
selected source groups stayed at 51. No plateau is established. All owned
processes, helper threads, handles and fixtures were cleaned up.

Native manifests reached version 1024 in that run. The locked library resolves
the latest local manifest by enumerating the complete versions directory. Safe
version retention is the next resource target; it must preserve active readers,
retained source revisions and native tags. It has not been implemented or measured.

Ready database opens and empty orphan-job cleanup now avoid unnecessary write
transactions. Fifteen behavior cases pass in each feature configuration,
including payload preservation, partial schemas, wrong table types and atomic
orphan cleanup. In the bounded empty-project MCP startup comparison, all four
closed database hashes and lengths remain unchanged; restart growth fell from
2,121,728 bytes to zero. Process logical writes fell from 6,448,059 to 2,150,971
bytes, including shutdown. Independent inspection found no material defect.
This is a no-op startup result, not a sustained-edit or physical SSD measurement.
All owned processes were reaped and their fixtures removed.

Concept-footprint and implementation-pattern requests now retain source leases
through response construction and give blocking source workers their own lease.
Six publication/collection and cancellation cases pass in each build; a cancelled
RPC cannot release source rows while its worker still reads them. Pure lexical
search workers also retain the lease after cancellation. Graph search and the
DOC resolution fallback retain their source view through enrichment; two actual
handler cases pass in each build across publication and collection. Remaining
reader/count/enrichment coverage and graph consistency are still open.

Four further native cases cover trace, centrality search, retrieval benchmark
loops and project health retaining their selected source through publication and
collection. A source lease preserves chunks, not a historical graph or disk
snapshot. Freshness ownership and refusal to certify newer live graph records
against an older source view now pass a paused public request case. Indexed test
axes also refuse newer graph metadata even when current disk bytes match. Support
KB feature queries and live ADP benchmark loops retain their selected view; wrong
project, generation and engine guards are refused. These new cases pass in the
native build. Remaining feature checks are batched at the release milestone.

Instruction discovery now retains applicability/hash descriptors instead of
every applicable document body before applying the output budget. Selected
whole files are reread with hash verification; changed or missing files become
unresolved omissions. This eliminates potential retention of 512 bounded
512 KiB source bodies without raising guidance caps or changing source priority.
It does not establish a bound on all parser/metadata allocations or measured
process memory. Complete-policy and public workflow checks pass in both builds.

Explicit full repair with `wipe_and_reindex=false` can now recover a corrupt or
missing required source head. It observes bounded physical/catalog generations,
reserves a fresh generation, rebuilds and seals source, then publishes only if
the captured authority still matches. Native 56 and no-vector 55 related checks
pass, including held readers, abandoned and malformed attempts, relocated source,
unrelated history preservation, stale recovery rejection, restart and another
edit. Collection preserves invalid unreachable metadata and unvalidated filename
aliases. Missing identity/allocator authority, incompatible vector storage and
exceeded inspection limits still refuse safely. Final deployment and remaining
storage gates are open.

## Audit follow-up ledger

| Finding | Disposition | Evidence / next gate |
|---|---|---|
| EG-01: Applicable authored guidance missed by planning/retrieval/review | Fixed in `0a82ea8` | Scoped guidance acceptance tests and isolated MCP verification; prose compliance is still agent-reviewed. |
| EG-02: Large change sets lose actionable context | Fixed in `0a82ea8` | Bounded manifests retain identities and provide exact hash-bound recovery. |
| EG-03: Watcher noise and unchanged refreshes rebuild source snapshots | Fixed and deployed in `ebbc03b` | Watcher and generation behavior tests; short live idle sample. Active-edit production measurements remain needed. |
| EG-04: Repeated copy searches/writer shutdowns and oversized database cache allowances | Earlier fixes deployed; new retention fix under verification | Streaming and store-cache improvements are deployed. A later audit found full PageRank maps retained for every generation in both memory and Redb, plus a duplicate derived-cache write. The working tree keeps one result per runtime and one current disk record, fences late completion and bounds search background workers. Native and no-vector behavior cases pass. The undeployed fc5a native 250-edit run reduces peak private memory 43% and fresh store footprint 81% versus 05a; CPU/write volume remains essentially unchanged and memory still grows. Remaining retention/resource/release gates stay open. |
| EG-05: Graph test stores retain temporary files | Fixed in `b7f0312` | 80 graph tests passed; contained store run left zero temporary entries. |
| EG-06: Explicit JSON scope discarded during guideline promotion; source basename collisions | Reviewed and deployed in `603a79a` | Reproduced cross-area enforcement; checks cover independent same-named sources, narrowing, invalid-import preservation and ambiguous legacy ownership. Existing matching semantics and legacy-migration limits are documented. |
| EG-07: Authored policy can enter a prompt that generalizes away qualifications | Reviewed and deployed in `603a79a` | Authored-policy distillation is rejected before model execution; direct to context-preserving ingestion. Markdown badges and the raw model-output parsing contract remain supported. |
| EG-08: Imported guidance can outlive its supporting source | Reviewed and deployed in `603a79a` | 42 focused workflow/registry checks pass. Source-owned reconciliation, atomic rule/receipt publication, bounded freshness checks and degraded coverage. Manual edits preserved; legacy records require migration review. Managed policies excluded from generalizing root-guidance generation. |
| EG-09: Tool tests alone do not establish task-level usefulness | Seven-case acceptance evaluation completed | All seven guided contracts passed; unguided candidates passed four, missed two domain requirements and withheld one implementation pending evidence. Historical uncertainty was respected by both arms. Coordinator verified five recovery pages and BLAKE3 before supplying guidance. [Raw proposals, outcomes and limits](../eval/instruction-outcomes-20261031.json). No statistical coding-quality claim. |
| EG-10: Installed VB sidecar missing, causing fallback | Reviewed, deployed and verified | Self-contained companion/runtime with recoverable asset inventory. Seventeen real companion checks, interrupted-copy recovery and a target path containing spaces pass. Installed Rust launched the packaged companion from an unrelated working directory; all 215 installed assets are manifest-bound. |
| EG-11: Filesystem-related kernel-pool retention | Open; ownership and lifetime unresolved | Selected tags rebounded by 2,237,469,120 bytes between 06:12 and 16:58 on November 2, then declined by 746,675,168 bytes by 19:13 on the same boot. FMfn paged allocations were about 2.07 GB at the latest sample. These observations show some release but identify neither an application nor the original freeze trigger. Material allocation ownership/lifetime or verified mitigation remains required. |
| EG-12: Deferred response snapshots fail under deep Windows data paths | Reviewed and deployed in `603a79a` | Actual MCP evaluation exposed native MAX_PATH handling in atomic persistence. A regression failed before the fix; eight recovery checks and actual 94,961-byte/five-page MCP recovery now pass. Canonical extended paths preserve containment, atomicity and exact hash-bound recovery. |
| EG-13: Actual source edits still copy unchanged chunks between generations | Partial local replacement; undeployed | Native candidate `05a58288` completes the fresh 50-file/40-edit pair and 251-file/250-edit run with checked source results, restart and fixture teardown. The fresh small pair reduces edit/query CPU 48% and total logical writes 26%. The comparable historical 240-edit prefix reduces CPU 6%, wall time 45% and logical writes 59%. Memory still grows. The previous f036 IO denial did not recur, but its owner/cause remains unknown. The later fc5a PageRank run reduces peak private memory 43% and fresh store footprint 81% versus 05a; CPU/write volume is essentially unchanged and residual memory growth remains. Physical payload, all-writer, recovery and remaining storage gates stay open. |
| EG-14: Area-specific instruction drafting | Implemented locally; acceptance/release incomplete | Generic maintenance requests can declare absent area Markdown and separate JSON provenance outputs, scoped observed/authored/proposed claims, citations and explicit evidence gaps. The 31-case native maintenance suite covers stale evidence, historical corruption and human-file/case-alias preservation. Representative workflow usefulness, final review and deployment remain open; no OX-specific integration was performed. |
| EG-15: Generated instruction-file ownership safety | Implemented and locally verified; final review/deployment pending | Receipt-bound generators preserve human/unreceipted/edited output and report recoverable proposals, conflicts and partial-write outcomes. Dated behavior evidence is recorded in closure_state; filesystem publication TOCTOU and final deployed validation remain. |
| EG-16: Generator overflow accounting | Locally verified overflow accounting; final review/deployment pending | Root overflow and original rule lineage remain recoverable through scoped companions or explicit omission/delivery accounting, including write failures. Final review/deployed validation and EG-24/25 task usefulness remain. |
| EG-17: Scope of manual/legacy rules in generated instructions | Locally verified scope preservation; consumer/release gates pending | Manual exceptions, distinct legacy scopes, zero-match warnings and authority decisions preserve scope. Claude paths frontmatter contract is checked; actual consumer loading, final review and deployment remain. |
| EG-18: Core tool preset dependency closure | Implemented and MCP-verified in WIP; not deployed | The 32-tool preset now includes project discovery, exact citation readers and job status/cancellation. A real client verified search-to-citation recovery, memory reads and owned-job lifecycle; full remains default with legacy/OciusX schemas. Startup instructions and advanced catalog disclose profile boundaries. Final review/deployment remain pending. |
| EG-19: Review-decision annotations in review | Partial local integration | Default review supplies bounded prior text and decision provenance in JSON and Markdown; current findings survive context failure/omissions. Imported wontFix/LLM dismissal is unverified workflow evidence, and workflow matches no longer dampen antipattern severity. Native/no-vector handler cases pass. Historical eligibility, authority/applicability, complete discussion recovery and decision-only discovery remain open. |
| EG-20: Automatic history routing without vectors | Locally verified; release pending | Eight public history cases pass with native vectors and without vector support. Long natural-language routing uses explicit lexical fallback while preserving requested first-parent cutoffs and filters. Final review and deployed-runtime checks remain required. |
| EG-21: Caller-controlled history reranking latency | Locally verified; release pending | The 0..90000 ms budget supports lazy skip and cancellation of a pending provider; invalid/provider-error/timeout outcomes retain the complete useful baseline with accurate ranking-tier disclosure. Public native/no-vector cases pass; final review/deployment remain pending. |
| EG-22: Persistent change-card metadata cache | Locally verified; release pending | Measured reconstruction and durable embedding reuse before adding bounded metadata tables to the existing Registry. Fixed OciusX history of 1,756 commits keeps the same metadata hash: baseline 8.79–10.21 seconds, cached 0.266 seconds and reopened Registry 0.260 seconds, with zero hit writes or repeated descriptions. Independent process-restart/profile/vector oracles and public history cases pass in the applicable builds. Final checks pass 71 executions across both features, including the real read-only probe. This is metadata-stage improvement, not whole-app latency/RSS or a claim of repeated API charges. Global release/deployment gates remain open. |
| EG-23: Change-card cache embedding identity | Locally verified; release pending | Actual same-repository Local-provider A-to-B-to-A and concurrent-profile cases verify document-provider identity isolation. Cache retention is bounded by partitions and estimated bytes while request snapshots survive eviction. This is not a hard process-memory limit; final review/deployment remain pending. |
| EG-24: Held-out task evaluation in regular verification | Partial; frozen pilot completed | Eight fresh paired scoped-policy, companion, history and diagnosis runs passed frozen-input/delivery/canary verification and independent opaque-label judging. Both conditions met 20/20 criteria, with evidence IDs 10→6 and delivered bytes +23.1%. Four tasks and saved-run verification do not establish regular CI, file-discovery savings, model token cost or real implementation/QA productivity. |
| EG-25: Matched OX implementation/QA benefit | Open evidence requirement | Obtain matched workflow evidence for useful decisions, prevented defects or reduced effort. Prior intake and sealed-QA measurements do not establish comparative productivity. |
| EG-26: Documentation and tool-contract drift | Partial reconciliation | README/request schemas expose 19 review gates; the roadmap distinguishes installed behavior, verified local work and open release/evaluation gates. Vector-maintenance descriptions now match real work and skip behavior, and memory diagnostics state their accounting limits. Complete tool/schema/capability reconciliation remains open. Capability tables are not measured readiness. |
| EG-27: Instruction evidence dependencies, freshness and reference drift | Local observation behavior verified; relevance/usefulness open | CommonMark and inline-code paths supply bounded version-local evidence, exact locations and explicit ambiguity/omissions. Symbol candidates retain captured hashes/excerpts and per-instruction observations; VB body coverage and proof of absence remain unavailable. Forty-five public cases pass in each build. Actual OX map/reporting pilots complete in 1.65/0.63 seconds with independent selected-source and UTF-8/CRLF/hash checks, while naming size/dependency limits. Lexical matches are noisy observations, not confirmed intent, stale-policy rulings or write authority. Broader usefulness, semantic policy correctness and release/deployment remain open. |
| EG-28: Legacy quality-rule ownership migration | Partial local implementation | Bounded raw ownership inventory and explicit source-bound atomic adoption preserve human rows and fence ordinary/derived publishers, deletions and receipt changes. Local behavior verification is recorded in the inventory. Alias lifecycle, exact live legacy evidence, actual human ownership decisions and release validation remain open. |
| EG-29: Owned packaging verification fixture teardown | Open; automatic cleanup rejected | The recorded fixture `<shared target>/package space verification-20261031` remains. Preserve the exact local receipt and rejection reason; close only after permitted cleanup or an explicit resolution. Preexisting copy inventories remain read-only under their separate approval requirement. |
| EG-30: Machine-wide filesystem stalls during audit work | Open; NTFS contention confirmed, initiating cause unresolved | The dump contains 222 associated NTFS waiters, ample available RAM and no NTFS blackbox history. Teams is not established as the culprit. Later SSD requests completed promptly, which does not clear the original interval. Recorder writes and active C: shadow-copy protection complicate physical-write attribution. The validation recorder produced a 464 MiB ETL and roughly three-minute save phase; it is unsuitable as a default lightweight monitor. Authorized bounded tests/builds have resumed. No new heavy capture or system/firmware change was performed; the original initiating cause remains unresolved. Closure requires a supported causal chain and verified correction, or an explicit disposition of the unrecoverable historical trigger. Evidence: `<shared target>/diagnostics/freeze-20261101/incident-summary.txt`. |
| EG-31: Evidence-driven instruction maintenance after code changes | Partial; three real-source dispositions verified | Reporting preserves existing guidance; the KML example flags a present code/policy conflict without weakening policy. A minimal historical PR2077 marker API replay produces the exact accepted optional-name guide update while preserving every other byte and exception. Actual source citations, independent raw Git/BLAKE3 checks, stale-evidence refusal and independent proposal review support these bounded examples. Current marker guidance already contains the update; replay omissions do not establish historical absence or compliance. Proposals write no authored files. The replay fixture is preserved after automatic cleanup rejection; wider usefulness and deployment remain open. |

The complete reconciled inventory, original-report identities and detailed acceptance
gates are recorded in [the audit inventory](../eval/audit-inventory-20261101.json).
EG-31 restores an original OX workflow requirement omitted from the earlier ledger;
it is not a new expansion. Historical completion receipts remain claims to verify
against their actual scope, rather than proof that every related outcome is closed.

Storage replacement release gates from independent static review remain open:

- Missing metadata for a managed snapshot must fail explicitly, rather than silently fall back to an incomplete physical generation. Pre-migration generations need positive identification.
- Full index, incremental index, repair, interrupted-attempt recovery and rollback materialization must share the snapshot publication protocol. A pending or sealed abandoned manifest must not collide with a full reindex.
- A stale GC head must be rejected even while an old reader remains pinned; head capture and collection must share the publication lock. Every multi-step source RPC must hold its reader lease until response construction ends.
- Reserve memory before reading/parsing/cloning snapshot metadata; bound cumulative ancestry bytes and selector construction. Recursive full-map cloning is not an acceptable resource bound.
- Establish writer/process exclusion and a verified materialization path before downgrade. Replacing the binary alone is not a valid rollback after shared file revisions are published.
- Run desired-behavior recovery/concurrency cases with and without vectors, then compare bounded sustained-edit workloads. The new storage remains undeployed until the storage release gates are met. Authorized bounded verification has resumed; no new heavy diagnostic capture or firmware/OS change has been performed.

The matrix below is generated from capability flags and records advertised implementation status. It does not establish behavioral readiness, resource performance, independent review, or deployment.

<!-- CAPABILITIES_MATRIX:START -->
| Tool / Feature | Status |
| :--- | :--- |
| `find_missing_indexes` | implemented |
| `detect_n_plus_one` | implemented |
| `query_schema` | implemented |
| `batch_read` | implemented |
| `index_project` | implemented |
| `update_project` | implemented |
| `list_projects` | implemented |
| `project_info` | implemented |
| `project_health` | implemented |
| `repair_project` | implemented |
| `delete_project` | implemented |
| `watch_project` | implemented |
| `unwatch_project` | implemented |
| `get_index_freshness` | implemented |
| `resolve_id` | implemented |
| `get_concept_footprint` | implemented |
| `find_similar_changes` | implemented |
| `find_implementation_pattern` | implemented |
| `map_guards_and_settings` | implemented |
| `plan_user_story` | implemented |
| `one_shot_feature_plan` | implemented |
| `get_change_set` | implemented |
| `validate_feature_contract` | implemented |
| `ingest_quality_gates` | implemented |
| `distill_quality_gates` | implemented |
| `pre_push_audit` | implemented |
| `get_gis_inventory` | implemented |
| `resolve_graph_edges` | implemented |
| `find_connection_path` | implemented |
| `find_dependency_cycles` | implemented |
| `detect_incomplete_changes` | implemented |
| `ask_codebase` | implemented |
| `trace_user_request` | implemented |
| `begin_edit_session` | implemented |
| `complete_edit_session` | implemented |
| `generate_agent_integration` | implemented |
| `ingest_review_findings` | implemented |
| `search_memory` | implemented |
| `get_chunk` | implemented |
| `update_memory_bank` | implemented |
| `list_memory_bank` | implemented |
| `read_memory_bank` | implemented |
| `delete_memory_bank` | implemented |
| `import_memory_bank` | implemented |
| `produce_support_kb` | implemented |
| `add_repo_rule` | implemented |
| `list_repo_rules` | implemented |
| `delete_repo_rule` | implemented |
| `query_graph_nodes` | implemented |
| `find_references` | implemented |
| `graph_search` | implemented |
| `traverse_graph` | implemented |
| `index_git_history` | implemented |
| `ingest_zip_history` | implemented |
| `search_history` | implemented |
| `analyze_temporal_couplings` | implemented |
| `analyze_reverts` | implemented |
| `impact_analysis` | implemented |
| `get_table_schema` | implemented |
| `trace_state_usage` | implemented |
| `trace_ui_event` | implemented |
| `trace_ui_action` | implemented |
| `export_capture_pack` | implemented |
| `get_ui_blueprint` | implemented |
| `get_codebase_overview` | implemented |
| `produce_claude_md` | implemented |
| `find_symbol_references` | implemented |
| `analyze_error_stack` | implemented |
| `dream_project` | implemented |
| `trigger_rem_cycle` | implemented |
| `analyze_file_coding_style` | implemented |
| `list_jobs` | implemented |
| `cancel_job` | implemented |
| `get_job_status` | implemented |
| `immune_check` | implemented |
| `anti_pattern_guard` | implemented |
| `pre_commit_review` | implemented |
| `ingest_code_review_history` | implemented |
| `explain_change` | implemented |
| `get_instrumentation_pack` | implemented |
| `suggest_migration_boundaries` | implemented |
| `ingest_instrumentation_logs` | implemented |
| `ingest_runtime_artifacts` | implemented |
| `generate_migration_blueprint` | implemented |
| `ast_dependency_graph` | implemented |
| `vector_search` | implemented |
| `incremental_indexing_gc` | implemented |
| `dedicated_antipattern_index` | implemented |
| `get_metrics` | implemented |
| `check_integrity` | implemented |
| `evaluate_safety` | implemented |
| `generate_migration_plan` | implemented |
| `benchmark_retrieval` | implemented |
| `get_extraction_confidence` | implemented |
| `get_checkpoint_status` | implemented |
| `get_memory_budget` | implemented |
| `compute_blast_radius` | implemented |
| `detect_design_patterns` | implemented |
| `autonomous_decision_gate` | implemented |
| `graph_centrality_rerank` | implemented |
| `generate_migration_scaffold` | implemented |
| `generate_instrumentation_code` | implemented |
| `reconcile_runtime_evidence` | implemented |
| `suggest_state_migration` | implemented |
| `generate_characterization_tests` | implemented |
| `generate_strangler_fig_config` | implemented |
| `map_validation_controls` | implemented |
| `map_auth_config` | implemented |
| `map_page_lifecycle` | implemented |
| `analyze_viewstate_deps` | implemented |
| `map_ajax_regions` | implemented |
| `trace_data_flow` | implemented |
| `get_migration_dossier` | implemented |
| `check_migration_coverage` | implemented |
| `update_migration_status` | implemented |
| `get_migration_progress` | implemented |
| `suggest_migration_order` | implemented |
| `analyze_full_project_migration` | implemented |
| `analyze_business_logic` | implemented |
| `query_business_logic` | implemented |
| `analyze_database_intelligence` | implemented |
| `get_sp_details` | implemented |
| `list_triggers` | implemented |
| `analyze_sync_hazards` | implemented |
| `get_jquery_inventory` | implemented |
| `get_session_workflows` | implemented |
| `get_vb_translation_traps` | implemented |
| `get_csharp_diagnostics` | implemented |
| `get_c_diagnostics` | implemented |
| `get_cpp_diagnostics` | implemented |
| `get_rust_diagnostics` | implemented |
| `get_method_info` | implemented |
| `get_full_method_body` | implemented |
| `get_method_edit_context` | implemented |
| `get_page_context` | implemented |
| `prepare_implementation_context` | implemented |
| `validate_generated_code` | implemented |
| `validate_sql_fragment` | implemented |
| `find_tests_for_method` | implemented |
| `find_dead_methods` | implemented |
| `check_edit_safety` | implemented |
<!-- CAPABILITIES_MATRIX:END -->

## Focus areas

- **Historical Phase 28 capability promotions.** Registry labels record code
  status; they do not close later quality, workflow or delivery requirements.
  The following implementation notes are retained:
  - `analyze_file_coding_style`: multi-language AST support (C#, TypeScript, Java, Go + existing Rust, Python), improved confidence calibration, edge-case hardening (parse error detection), line length + async pattern detection.
  - `graph_centrality_rerank`: multi-algorithm centrality (PageRank + degree + betweenness approximation), 3 modes (search+rerank, node scoring, top-N), configurable algorithm weights, Brandes k-pivot betweenness.

## Changelog

### Phase 37: Intelligence Amplification (6 tickets)
- **Async LLM Enhancement Pass**: `use_llm: true` flag on `analyze_full_project_migration` upgrades deterministic business logic summaries with LLM-powered step-by-step analysis as an async post-processing step, validated against static analysis with High/Medium/Low confidence scoring
- **LLM Validation Gate** (`business_logic_service.rs`): Cross-validates LLM output against deterministic static analysis across 9 effect categories (SQL, Session, Redirect, ViewState, Cache, Application, Cookie, Email, File I/O); detects hallucinated table references; assigns confidence scores
- **Stored Procedure Deep Analysis** (`database_intelligence_service.rs`): Deterministic SP summaries (purpose, parameters, tables read/written, side effects), SP→SP call chain detection with cycle detection, maximal-only chain emission, trigger detection (AFTER/INSTEAD OF/FOR with DML event types)
- **Database Schema Ingestion** (`database_intelligence_service.rs`): Balanced-parenthesis SQL parser for CREATE TABLE/VIEW, column extraction (type, nullable, default, computed), PK/FK constraints with ON DELETE/UPDATE actions, CHECK constraints, schema cross-referencing against code-level table references with full column inventory
- **Session Workflow Reconstruction** (`session_workflow_service.rs`): Synthesizes WritesState/ReadsState graph edges into per-key workflow narratives with scope detection (Session/Application/Cache/ViewState/Cookie), flow pattern classification (7 patterns), cross-page chain counting, operation deduplication
- **Confidence Dashboard** (`full_project_migration_service.rs`): 6-dimension migration intelligence matrix (Code Structure, Business Logic, Database, Session Workflows, Data Access, External Integrations) with coverage metrics, confidence badges, and rendered markdown section in the full report

### Phase 36: LLM-Powered Business Logic Comprehension (2 tools, 1 service, 17 tests)
- **New tool**: `analyze_business_logic` — LLM-powered method-level analysis via local Ollama (Qwen 2.5 Coder 14B) producing structured summaries: PURPOSE, STEPS, RULES, DATA, ERRORS, EFFECTS. Deterministic fallback when no LLM available. Content-hash caching prevents re-analysis of unchanged methods. Supports method, file, or project scope
- **New tool**: `query_business_logic` — semantic search over previously analyzed business logic summaries stored in DocStore `business_logic` namespace
- **New service**: `business_logic_service.rs` — `analyze_method_logic`, `analyze_file_logic`, `analyze_project_logic`, `deterministic_method_summary`, `parse_llm_response`, `render_compact_markdown`, `render_method_as_doc`
- **New namespace**: `business_logic` (GlobalMutable, KeepForever) in `namespaces.rs`
- **Report integration**: Business logic summaries embedded in `analyze_full_project_migration` report with per-file class-level purpose and method-level detail tables

### Phase 35: Last-Mile Accuracy (4 bug fixes + 4 new features, 58+ new tests)
- **VB Translation Traps**: Module-level detection, ByRef semantics, implicit conversions, Nothing vs null, On Error Resume Next, optional parameters, late binding, With blocks, ReDim Preserve
- **jQuery Inventory**: Comprehensive jQuery/jQuery UI widget detection, AJAX call tracing, event handler extraction, plugin identification, selector pattern analysis
- **Inherited Effects**: Base class method analysis, inheritance chain traversal, virtual/override pattern detection, shared code-behind analysis
- **Cross-Layer AJAX Tracing**: End-to-end tracing from jQuery `$.ajax()` through WebMethod/PageMethod/ASMX endpoints to SQL operations with data contract extraction

### Phase 31: Migration Workflow Engine (12 tools, 11 services)
- **12 new tools**: `map_validation_controls`, `map_auth_config`, `map_page_lifecycle`, `analyze_viewstate_deps`, `map_ajax_regions`, `trace_data_flow`, `get_migration_dossier`, `check_migration_coverage`, `update_migration_status`, `get_migration_progress`, `suggest_migration_order`, `analyze_full_project_migration`
- **Validation mapping** (`validation_mapping_service.rs`): WebForms validator → DataAnnotation/FluentValidation mapping with custom validator detection and validation group analysis
- **Auth config analysis** (`auth_config_service.rs`): web.config auth mode detection (Forms/Windows/None), location rules, membership/role provider config, code-level auth pattern scanning
- **Page lifecycle mapping** (`lifecycle_service.rs`): Page lifecycle event mapping (Page_Init → OnInitialized), IsPostBack detection, implicit WebForms behaviors
- **ViewState analysis** (`viewstate_service.rs`): Explicit ViewState["key"] extraction, implicit control-level ViewState, modern state type recommendations
- **AJAX region mapping** (`ajax_region_service.rs`): UpdatePanel/ScriptManager inventory, partial postback trigger mapping, component decomposition suggestions
- **Data flow tracing** (`data_flow_service.rs`): Entry-point → sink data flow tracing, state read/write tracking, SQL table access, cross-file dependencies
- **Migration dossier** (`dossier_service.rs`): Per-file migration dossier orchestrating lifecycle/viewstate/ajax/validation/auth/blast-radius/scaffold
- **Coverage checking** (`coverage_service.rs`): Migration coverage gap analysis comparing original page vs modern code across 6 categories
- **Progress tracking** (`migration_progress_service.rs`): Standalone Redb-backed migration status tracker (not_started/in_progress/done/blocked per file)
- **Migration ordering** (`migration_order_service.rs`): Kahn's algorithm topological sort producing parallelizable waves with cycle detection and bottleneck identification
- **Full project analysis** (`full_project_migration_service.rs`): Single-call orchestrator that reads all markup + code-behind files concurrently, runs migration_order + state_migration + auth_config + db_strategy + per-file dossiers, produces FullProjectMigrationReport with CrossCuttingSummary and comprehensive markdown report
- **Infrastructure**: MigrationProgressStore in AppState (standalone Redb), helper functions find_codebehind_path/find_aspx_for_codebehind, futures dependency for concurrent file I/O

### Phase 30: End-to-End Migration Engine (8 gaps) + Phase 30a Hardening (7 fixes)
- **6 new tools**: `generate_migration_scaffold`, `generate_instrumentation_code`, `reconcile_runtime_evidence`, `suggest_state_migration`, `generate_characterization_tests`, `generate_strangler_fig_config`
- **Control mapping catalog** (`engram_index::control_mapping`): 50-entry WebForms → Blazor/React/Angular control mappings with accessibility, data binding, and event equivalents
- **Scaffold service** (`scaffold_service.rs`): Blazor/React/Angular component generator with real business logic from graph edges (SQL→repository, state→get/set, navigation→routing), async/await conversion guidance, repository interfaces, DTOs, test scaffolds
- **DB strategy service** (`db_strategy_service.rs`): Data access pattern classifier (8 patterns), repository interface generator, SQL injection risk scorer
- **Instrumentation service** (`instrumentation_service.rs`): C#/VB.NET HttpModule generator with route/session/SQL/postback/error tracing, InstrumentedSessionStateWrapper (IHttpSessionState impl), InstrumentedDbCommand (DbCommand subclass with timing), plus static-vs-runtime reconciliation
- **State migration service** (`state_migration_service.rs`): Per-key migration recommendations with access pattern analysis, ViewState lifecycle classification, and affinity grouping
- **Characterization test service** (`characterization_test_service.rs`): NUnit/xUnit/MSTest test generator producing executable test bodies with TestPageFactory, MockHttpSession, TestDbFactory, MockResponseRecorder helper infrastructure
- **Strangler fig service** (`strangler_fig_service.rs`): YARP reverse proxy config, Microsoft.FeatureManagement feature flags, routing middleware with percentage-based rollout and sticky sessions, health check endpoint, Program.cs registration with Polly circuit breaker/retry, CorrelationId middleware
- **VB.NET deep extraction**: Nested With block stack, On Error GoTo label resolution (two-pass), CreateObject return value propagation and alias tracking, late-bound method call detection, My. namespace, ReDim Preserve
- **GIS deep extraction**: 30+ Google Maps classes (Places, StreetView, Heatmap, KML, Directions, DistanceMatrix, Elevation, Geometry), 80+ Esri/ArcGIS ES module classes (widgets, tasks, renderers, geometry, portal, auth, 3D), migration complexity assessment
- **Classic ASP extractor** (`asp_classic_extractor.rs`): 7 detection categories with 31 tests
- **Report extractor** (`report_extractor.rs`): SSRS and Crystal Reports detection with 16 tests
- **Windows Service detection**: ServiceBase/TopShelf/BackgroundService recognition in pattern detection service
- **Config fields**: `scaffold_default_target_stack`, `scaffold_include_tests`, `enable_classic_asp_extraction`, `enable_report_extraction`, `characterization_test_framework`
- **138+ tests** across all Phase 30 modules (121 original + 17 strangler fig)

### Phase 27: Gold Standard Hardening (10 tickets)
- **Benchmark schemas** (`engram_core::benchmark`): `BenchmarkPack`, `AdpCorpus`, `TraceScenarioLibrary`, `DriftReport` with versioned schemas, per-class thresholds, and drift detection
- **ADP replay**: Deterministic `replay_from_scenario()` and batch `run_corpus()` with `AdpConfusionMatrix` for false-allow/false-deny calibration reporting
- **JSON audit reports**: `AdpDecisionReport` with `build_decision_report()` — immutable per-verdict report with gate-by-gate evidence, config snapshots, and input replay data
- **Rollout policy engine**: `RolloutPhase` (shadow/advisory/guarded/autonomous) with `apply_rollout_policy()` — kill-switch forces all decisions to deny
- **Runtime evidence schemas** (`engram_core::runtime_evidence`): `RuntimeEvent`, `RuntimeEvidenceBatch`, `ReconciliationResult` with `validate_batch()` schema validator
- **Trace provenance**: `trace_ui_event` enhanced with structured provenance block, unresolved candidates, per-hop evidence, follow-up probes
- **Safety calibration**: 7-scenario labeled corpus with false-allow rate ≤ 1% assertion
- **WebForms mutation tests**: 12 mutation tests for extraction robustness
- **Integrity canary tests**: 9 canary tests with synthetic drift injection
- **Benchmark CI**: `.github/workflows/benchmark-ci.yml` with artifact upload
- **Config fields**: `adp_rollout_phase`, `adp_kill_switch`
- **Bugfix**: `saturating_sub` overflow in ADP evidence sufficiency gate

### Phase 26: Autonomous Decision Protocol (ADP v1)
- **New tool**: `autonomous_decision_gate` — mandatory 8-gate verification pipeline for autonomous code changes (allow/deny/abstain verdicts)
- **Gate pipeline**: extraction confidence → trace certainty → safety policy → retrieval quality → blast radius → anti-pattern → runtime evidence → evidence sufficiency
- **Trace ambiguity fix**: `trace_ui_event` now emits fallback candidate metadata and confidence penalties instead of silently resolving to first match
- **Config fields**: `adp_enabled`, `adp_min_extraction_confidence`, `adp_max_blast_radius`
- **Service**: `autonomous_decision_service.rs` with 11 unit tests covering all acceptance criteria
- **Bugfix**: Fixed pre-existing `cs: Query` vs `Option<Query>` type mismatch in `engram_index/src/parsing.rs`
