# Review evidence contract

Engram's static review results do not certify compilation, test execution, or a
complete application audit. `pre_commit_review.coverage` names the submitted
files, textual diff scope, binary/metadata-only gaps, input digest, before/after
source fingerprints, and commit identities. Provider failures and caps remain in
`gate_status`. Changed or unavailable source snapshots prevent a green clearance.
The supplied diff is not certified to match the working tree. Deleted files have
no current-content verification. Snapshot limits are 8 MiB per file and 64 MiB per
review; unavailable data is explicit.

## Exact test targets

### Source boundaries

`get_full_method_body` distinguishes `retrieval_scope: indexed_method` from
`explicit_source_range`. A unique FQN resolves an indexed method span. `source_verification` distinguishes
a matching stored fingerprint from a legacy entry without one. Legacy boundaries
remain unverified. `source_file_hash` is the BLAKE3 hex digest of the complete
file bytes used for slicing; verification and slicing use that same snapshot.
Later edits are outside that snapshot. An explicit file/line request
returns only those lines; it does not establish method boundaries. Search chunk
bounds can end mid-method. Resolve a unique FQN or use `get_method_edit_context`
with `file_path`, `method_name` and an overload-disambiguating `line` before
reasoning about a complete method. Invalid, reversed or out-of-file ranges fail
instead of silently returning empty or clipped source.

### Test lookup

Use `find_tests_for_method(project_id, method_name, file_path, start_line)` to
select an overloaded declaration. `start_line` is the exact, 1-based indexed
declaration start, not an arbitrary line in its body. It requires `file_path`.
Ambiguous, conflicting, or incomplete candidate lookups fail explicitly.
Responses include the target node ID, file and declaration line. Dependency
edges and name heuristics remain separate; neither proves executed test coverage.

## Decision history

### Reviewing inferred business logic

Review `purpose`, `steps`, `business_rules`, `data_flow`, `error_handling`, and
`side_effects_detail` against the complete source. A decision about numbered rules
does not cover the other fields. Record supported, contradicted and unexamined
claims separately, including the source branch or counterexample for each
disposition. Source-anchor consistency and a successful extraction are not
semantic validation.

Distinguish constructing or returning a deferred query, iterator, delegate or
task from enumerating, invoking or awaiting it. Normal completion of a return
expression does not establish completion of deferred work. Inspect actual
execution/materialization operations and helper bodies before claiming those
operations occur, succeed, or are prerequisites for returning the value.

### Recording review decisions

`record_review_decisions` appends immutable events under a project and review ID.
`get_review_decisions` returns the full event history and latest disposition for
each finding. Use `PR-123` to attach decisions to `find_merged_work` cards.

Each event records `event_id`, `finding_id`, `supersedes`, `kind`, `source_url`,
`author`, `recorded_at`, `rationale`, and optional `verification`. A transition
must supersede the latest event for that finding. Replaying an identical event
is idempotent; editing an existing event is rejected. The whole batch is validated
before writing. Limits: 500 events and 1 MiB per review. Updates are serialized
within the daemon. A project must have one owning daemon, as with its other stores.

Kinds:

- `open`: unresolved observation, or a disposition needing reconciliation.
- `accepted_exception`: explicitly accepted risk or intentionally declined change.
- `claimed_fix`: someone or the provider says it is fixed; no verified check.
- `verified_fix`: an **external attestation** with full commit ID, check description,
  HTTPS evidence URL, artifact SHA-256 and verifier. This means reported verification,
  not that Engram ran the check or authenticated the author or artifact. It is current
  only when the registered repository has that exact HEAD and a clean worktree.
  Otherwise the response says `verification_not_current`; this is not proof of a regression.

No decision automatically suppresses a finding. Agents should cite the source and
rationale when reconciling it. Imported text never becomes an instruction to run
a command. Missing imports, persistence errors, and stale attestations are explicit.
Merged-work cards show at most 20 current decisions, with a count and a full-history
handoff. Point-in-time replay omits these records because imported timestamps are
not independently verified.

## Import existing provider exports

`tools/import_review_decisions.py` accepts saved Azure DevOps `value` thread arrays
or GitHub `reviewThreads` objects with `nodes`. It performs no network fetches or
posting. Run `python tools/import_review_decisions.py --help` for required paths,
project/review identity, binary and source URL.

The adapter retains reviewer comments and source links. Azure `fixed` becomes
`claimed_fix`; `wontFix`/`byDesign` becomes `accepted_exception`. Closed Azure or
resolved GitHub conversations remain `open` for reconciliation, because resolution
alone does not say whether the finding was fixed or declined. Comment text is not
classified with an LLM. Record an explicit superseding decision when a human comment
contains the actual disposition. Long excerpts disclose truncation and link to the
original source. Import manifests retain the input file digest.

This supplements the existing `ingest_code_review_history` pattern-learning pipeline;
it does not reinterpret its historical fix-rate heuristics as verified decisions.

## Repeatable acceptance

`tools/evaluate_evidence_reliability.py --binary PATH --output DIRECTORY --repo REPO`
clones committed local reference snapshots into the output directory and runs MCP
checks with an isolated FTS-only store. Repeat `--repo` for additional repositories.
Use `--live` to exercise the installed daemon instead. Original repositories are
not edited and their application code is not executed.

Results retain binary SHA-256, reference commits, raw responses, schemas, assertions
and per-call timings. Synthetic CRLF/overload/attestation checks are labelled
separately from held-out retrieval checks. These are deterministic acceptance
checks, not accuracy percentages, domain approval, or concurrent-load benchmarks.
