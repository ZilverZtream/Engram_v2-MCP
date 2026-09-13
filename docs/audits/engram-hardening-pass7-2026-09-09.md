# Engram hardening pass 7

Deployed and verified at 2026-09-09 09:47 CEST. The first ten workflows now meet
the provisional 9/10 standard for supported static-evidence behavior. This is
not measured accuracy, domain approval or runtime certification.

## Changes

- SQL uses separate CTE, derived-table and correlated-subquery scopes. It checks
  alias visibility, duplicate outputs, scalar subquery arity, ORDER BY aliases
  and ordinals, and JOIN conditions. Qualified tables require matching stored
  CREATE TABLE evidence; qualifiers are never silently discarded.
- INSERT checks explicit target columns and projection/value counts; UPDATE
  checks assignments and predicates; DELETE checks predicates. Defaults,
  procedure contracts, wildcard expansion, set operations and unsupported DML
  clauses remain explicitly partial. Binding is bounded to depth 16 and 512
  scopes. No SQL is executed. Types, nullability/default contracts, aggregate
  legality, function contracts and application-side injection remain unverified.
- Removed raw SQL-text injection/anti-pattern regexes that mistook legal
  concatenation, comments or quoted contents for application-side hazards.
- Test derivation withholds graph axes when the indexed file hash differs from
  current source. Legacy fingerprints are labelled unverified. Source reads
  are bounded to 8 MiB/file and 64 MiB/query. Empty output does not imply that
  the change is gate-free.
- Review preserves shared-provider errors and lookup caps. Filtering known risk
  findings by severity cannot turn a review green. The temporal gate distinguishes
  absent, partial and complete Git history. Compilation and test execution remain
  explicit separate statuses.

## Verification

1,275 unit tests and 38 integration checks passed. The first integration run
found two issues: qualified-name coverage reporting and a fixture that expected
metadata=None to clear an existing graph fingerprint. Both were corrected and
the final suite passed.

All 40 live checks passed: 18 baseline workflows and 22 extended checks,
including 19 positive/negative SQL fragments. Those SQL checks took 0.001–0.010
seconds. The synthetic review defect produced RED with one critical finding;
compilation and test execution remained not_run. The patch was never applied.
Shared audit-convention truncation currently degrades all gates; narrowing that
notification to dependent gates is a remaining usability improvement.

All 145 schemas are advertised. The reference project reported generation 1079;
that generation change occurred outside this work. Its working tree now has
external edits in two source files. Engram work did not modify application source.

## Deployment

- SHA256: `83A85A61CEC98742D95D98D2AA0B0C0AADD0F816459D1ADA6266F8F417A5DB39`.
- Daemon PID at verification: 25052.
- Backup: `engram_server.retired-20260909-094656.exe`.
- The initial copy-based restart encountered a Windows executable lock. A staged,
  hash-verified rename/swap succeeded, with the retired binary preserved.
- Evidence: `target/engram-readiness-pass7`, including final tests, deployment,
  all live responses and the repeatable verification script.

Next-group baseline evidence is saved separately under `target/engram-next10-audit`.
