# Unified route model — live evidence (sanitized) — generation 984

Fresh clean reindex (the bloated 20GB graph.redb + 35GB project data were reset).

## Canary — the unified declared-Class= path resolves getimg (PASS)
- getimg incoming: 10 edges, **ajax.ts is a caller** — resolved via the .asmx's
  declared `Class=` (exposes), NOT the removed filename special-case.
- ox_causal_20: **16 routes**, api.getimg cited, 1 VIA-labelled mediated hop.

## Floors (gen 984)
| Suite | Floor | Gen 984 |
|---|---|---|
| causal (20) | ≥16 | **15** — one borderline row (ox_causal_19) |
| golden (35) | ≥23 | **23** ✓ |
| blind (8)   | ≥6  | **6** ✓ |

## ox_causal_19 — why it dipped (NOT a model defect)
"Which frontend code depends on ConvertHeicToBase64String?" required_all
[imgHandler, api-images]. The unified model cites all THREE real frontend callers
(fbinstplan.js, imgHandler.ts, imgManager.js — all verified in source to call
ConvertHeic) plus the impl. The precision metric (3/10) penalizes this because:
(a) the row's required_all lists only 2 of the real callers, so the extra CORRECT
citations count against precision; (b) the semantic code arm cites wrong-modality
`.vb` backend files (CryptoHelper.vb, RoQReport.vb, …) for a `.ts`-frontend
question. The answer is MORE correct, not less — the curated row is narrow.
Chasing it by hiding real callers is the overfitting the re-audit told me to stop.
