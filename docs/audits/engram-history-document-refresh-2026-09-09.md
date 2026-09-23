# Safe history document refresh and business-rule qualifications

`index_git_history` now accepts `mode: "refresh"`. It regenerates existing
`diff:<commit>:<path>` documents from local Git objects without clearing or
incrementing graph relationships, changing normal history watermarks, ingesting
new commits, or rebuilding merged-PR documents. `max_commits` bounds a batch;
repeat while `more: true`. A separate versioned cursor resumes in commit-ID order.
In this mode, `force: true` restarts only that cursor.

Replacement documents are indexed before superseded document IDs are deleted.
Cleanup is scoped to the project and namespace. Cancellation, an unavailable
Git object, or an embedding failure leaves the old evidence available. A failure
between writes and cleanup can leave duplicate evidence until retry; this is
not a cross-store atomic transaction. Already-current text is skipped. This
operation does not backfill independently missing vectors for unchanged text.

Replacement batches flush after reaching 200 documents, 8 MB of accumulated
text, or the end of the requested commit batch. A single large commit can exceed
the text threshold. The cursor advances only after replacement writes and
cleanup succeed, allowing a failed batch to converge on retry.

Background history jobs now acquire the same project update lock as synchronous
history jobs. They propagate the actual indexing failure and preserve successful
result summaries instead of discarding the result and reporting completion.

Business-rule reference checks distinguish complete identifiers from longer
names with matching prefixes or suffixes, while preserving VB case and spacing
behavior. Static-consistency ratings, including High, no longer suppress the
model-inference qualification in rendered analysis. These checks do not establish
semantic correctness or complete rule coverage.

Validation: 19 focused integration regressions and all 1,275 server unit tests
pass. Tests cover bounded resume, repeat stability, exact regenerated diff text,
unchanged graph weights/metadata and history checkpoints, namespace isolation,
retention on missing objects, project-lock serialization, and background error
reporting, plus retention and retry after a multi-commit preparation failure.
The focused store tests use the lexical-only backend; live vector
acceptance is recorded separately.

Build/deployment and MCP acceptance receipts are kept outside the repository at
`C:\ai-projects\audits\Engram\history-document-refresh-20260909`.

The live corpus refresh completed at 20:16 CEST. All 7,005 stored commit groups
were checked; 130,082 replacement upserts completed and 94,445 superseded document
records were removed. Upserts include already-current documents within changed
commits and are not a count of unique defects. Final health passed, source
completeness had no missing paths or mismatches, and search/vector totals agreed
at 187,004. Graph counts remained unchanged. Three live retrieval controls passed.
The aggregate receipt is `complete-receipt.json` in the external evidence root.

Business-rule extraction remains 8/10 pending semantic fidelity and coverage
improvements. Historical exemplars returns to 9/10 for the supported bounded
retrieval workflow; broad semantic recall and review-approval proof remain outside
this evidence.
