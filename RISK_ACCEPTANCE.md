# Engram-MCP v2 — Risk Acceptance Register

Date: 2026-03-27
Prepared by: Engineering (automated audit remediation)
Audit session: 2026-03-27 INV findings

## Status: All Highs Fixed — No Open Risk Acceptances Required

All High findings from the 2026-03-27 audit have been resolved by code fix:

| Finding | Severity | Status | Fix commit | Closed date |
|---------|----------|--------|-----------|-------------|
| AUD-2026-INV-0001 | High | Fixed | afd0a2e | 2026-03-27 |
| AUD-2026-INV-0002 | High | Fixed | afd0a2e | 2026-03-27 |
| AUD-2026-INV-0003 | High | Fixed | afd0a2e | 2026-03-27 |
| AUD-2026-INV-0004 | High | Fixed | afd0a2e | 2026-03-27 |
| AUD-2026-INV-0005 | Medium | Fixed | afd0a2e | 2026-03-27 |
| AUD-2026-INV-0006 | Medium | Fixed | afd0a2e | 2026-03-27 |

## Residual risks noted (Low — no formal acceptance required)
- Watcher event drops under extreme saturation are logged but not retried (intentional by design — backpressure policy)
- Runtime evidence derivation falls back to false on project-not-loaded (acceptable — conservative default)
- CWD default for allowed_roots requires explicit opt-in (by design — fail-closed is the correct default)

## Next audit cycle
All gate requirements through 4.0 have been addressed. Gate 4.5+ requires one full clean retest cycle (pass `cargo test --lib --all` with 0 failures).
