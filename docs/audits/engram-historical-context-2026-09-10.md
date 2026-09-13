# Historical changes in method context

Agents can miss a relevant older regression when they retrieve method context
without separately searching merged work. `get_method_edit_context` now includes
up to two historical changes whose shipped file cohorts match the method file.
The section is enabled by default, with `include_history=false` to omit it.
`history_query` supplies task wording; otherwise retrieval uses the filename.
`merged_before` restricts historical changes to dates strictly before the cutoff.

The shared `find_merged_work` retriever accepts up to ten relative `file_paths`.
Shipped-cohort eligibility is applied before candidate caps and title ranking;
mentions in a PR body cannot qualify an unrelated file. A filename retrieval arm
keeps older file history discoverable when task vocabulary differs from the old
title. Query arms each retain at most 100 candidates, with the aggregate upper
bound reported in the response. Existing title ranking and cutoff checks remain.

Multi-segment trailing path matches accommodate possible root/path changes but
are explicitly identified as leads, not proven rename equivalence. Basename-only
matches do not qualify different directories. Results carry Git provenance,
shipped cohorts and full-document pointers. They do not establish reviewer
approval, method-level applicability or that a historical solution should be
copied. Existing task-planning exemplar wording now says indexed merged work
rather than approved work.

History lookup failures are explicit and do not prevent the remaining method
context from returning. Invalid dates do not silently disable the cutoff. The
embedded history section is limited to 24,000 bytes with an explicit truncation
notice and direct retrieval instructions. Historical source/index isolation is
still required for replay; a history cutoff does not make current source safe
for point-in-time evaluation.

## Verification

- 15 method-context integration tests and six merged-work ranking tests passed.
- A flooded candidate test proves unrelated body mentions cannot crowd out the
  matching shipped-file history and rejects unrelated same-basename files.
- A handler test proves new task wording retrieves an older shared-file
  regression and its related consumer file, excludes future history, and obeys
  opt-out.
- Debug and optimized release MCP probes passed synthetic Git ingestion and
  JSON/Markdown method context. The release probe additionally verifies invalid
  calendar dates withhold history.
- Deployed at 05:23 UTC on September 10: 147 tools, healthy live project,
  unchanged unrelated schemas and configuration. Release SHA256:
  `34ad12aed7d31824ad60eacd9babd9d9239c686d35aa0b07a6d004dbf965e5e5`.

Receipts, synthetic fixtures and rollback metadata are outside this repository:
`C:/ai-projects/audits/Engram/historical-context-20260910/`.

## Outstanding outcome evidence

Method context and merged-work retrieval retain provisional 9/10 engineering
assessments. These checks demonstrate supported retrieval behavior, not improved
agent correctness or complete historical recall. A fresh paired historical
implementation replay is underway; its behavior, tool-use attribution and any
resulting generic repairs are required before completing the active cycle.
