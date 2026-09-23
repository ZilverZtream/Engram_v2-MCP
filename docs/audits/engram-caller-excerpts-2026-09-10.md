# Caller evidence in pre-edit context

The historical replay exposed a gap between identifying a caller and showing
how that caller invokes the method. Different export consumers can pass different
arguments or rely on defaults, while returning only caller identities makes that
distinction expensive to discover.

`get_method_edit_context` now includes `caller_excerpts` in JSON and a caller
source excerpt section in Markdown. Full caller bodies remain opt-in. The
existing caller display cap also limits this section; the default is three.

Each excerpt comes from the indexed caller span in source whose raw BLAKE3
fingerprint matches the indexed file fingerprint. A missing fingerprint, changed
file, invalid span, unavailable source or exhausted read budget withholds the
excerpt and explains why. The bytes that are hashed are also the bytes inspected.
These optional failures do not prevent the other method context from returning.

The implementation is generic. It matches the target's final name followed by
an opening parenthesis, case-insensitively, within each caller span. It preserves
original line numbers, limits selection to three lexical matches and four lines
per window, deduplicates overlapping lines and limits each line to 240 Unicode
characters. Reads are bounded to 8 MiB per caller and 64 MiB per request.

## Evidence

- Three focused unit tests pass: argument and line preservation, bounded Unicode
  excerpts and source-read budget enforcement.
- All 14 method-context integration tests pass. The new handler test confirms
  default versus explicit rendering arguments, omission of full caller bodies,
  withholding without fingerprints and withholding after a caller edit.
- An isolated compiled debug MCP probe passes real indexing, tool advertisement,
  and JSON/Markdown retrieval for two consumers with different arguments.
- The optimized release passed the same isolated MCP probe and was deployed
  at 04:45 UTC on September 10. Fresh live acceptance passed: 147 tools,
  unchanged unrelated schemas and configuration, and project health OK.
  Binary SHA256: `f54d069e2e81e584fe523c399c6d86fe94fa75b9028abf503f293faa777a3507`.
  Verification and rollback receipts are maintained outside this repository at
  `C:/ai-projects/audits/Engram/caller-excerpts-20260910/`.

## Limits and assessment

Excerpts are lexical leads, not argument binding, overload resolution or execution
proof. Comments, strings, declarations and other receivers may match. Generic
invocations with type arguments may not match. A window may end mid-call; the
response explicitly directs agents to inspect full callers for defaults and
output handling. Only indexed callers are considered, with existing dangling
edge and provider coverage qualifications retained.

Method edit context remains a provisional **9/10** engineering assessment.
Passing retrieval tests does not establish that agents use the evidence correctly
or improve implementation outcomes. A fresh blinded replay is still needed.
Automatic historical regression discovery remains separate pending work; this
change does not claim to implement it.
