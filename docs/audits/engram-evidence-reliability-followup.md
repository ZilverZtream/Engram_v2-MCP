# Evidence reliability follow-up

2026-09-09. Production changes and fixtures remain generic. External application
review artifacts remain outside this repository.

## Priority order

1. Correct freshness checks before expanding inference. Graph `file_hash` values
   fingerprint raw bytes; chunk `ContentHash` normalizes line endings. Comparing
   these falsely withholds current test axes for CRLF files. Match the indexer's
   raw fingerprint and retain rejection after actual source edits.
2. Make `find_tests_for_method` overloads addressable. Qualified name and file
   cannot identify two same-named declarations in the same file. Add a precise
   selector, reject conflicting selectors, and test that callers of a sibling
   overload are not presented as direct coverage of the selected declaration.
3. Preserve review decisions with provenance. Distinguish open observations,
   accepted exceptions, claimed fixes, and fixes verified against a commit.
   A resolved thread alone must not establish that code was repaired. Keep
   provider-specific ingestion separate from the generic evidence model.
4. Make review coverage explicit. Expose reviewed files, unexamined areas,
   missing providers, and whether checks were static, compiled, or executed.
   A small finding count or successful tool call must not imply full coverage.
5. Evaluate these workflows on held-out repositories and languages, including
   LF/CRLF files, overloads, stale indexes, and changing branch heads. Use task
   success and evidence correctness alongside latency; do not raise scores from
   passing compilation alone.

## First change

The existing test-matrix integration fixture now uses raw-hashed CRLF source.
It failed before the production change because current gate guidance was
withheld. The same fixture exercises a subsequent semantic edit and absent
legacy fingerprints. Production comparison now uses raw BLAKE3 bytes.
After the fix, all 14 `top10_audit_tests` integration tests passed. The changed
files also passed `git diff --check`.

All five work items are now implemented and validated for the bounded contract.
Deployed at 13:56 CEST with live acceptance completed afterwards. See
`engram-hardening-pass10-2026-09-09.md` for the tested binary, external evidence,
scope and remaining limits. Source fingerprinting also passed against the
original reference case on the deployed daemon.
