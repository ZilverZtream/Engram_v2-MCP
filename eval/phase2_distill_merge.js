// Cross-batch canonicalizer: merge the per-language batch rulesets (vb/web/client)
// into ONE final generic project ruleset. Dedups cross-cutting rules (e.g. the
// .ts->committed-.js transpile rule appears in both client and web), drops any
// rule still too specific, and ranks by impact (evidence_count × severity).
export const meta = {
  name: 'distill-merge',
  description: 'Merge per-batch distilled rulesets into one canonical generic ruleset',
  phases: [{ title: 'Canonicalize' }],
}

let BATCHES = null // INJECTED_BATCHES
const batches = BATCHES || (args && args.batches) || []
const all = batches.flatMap(b => (b.rules || []).map(r => ({ ...r, _batch: b.batch })))

const RULESET = {
  type: 'object', required: ['rules'],
  properties: {
    rules: {
      type: 'array',
      items: {
        type: 'object',
        required: ['rule', 'why', 'category', 'languages', 'severity', 'evidence_count'],
        properties: {
          rule: { type: 'string' }, why: { type: 'string' }, category: { type: 'string' },
          languages: { type: 'array', items: { type: 'string' } },
          severity: { type: 'string', enum: ['high', 'medium', 'low'] },
          bad_example: { type: 'string' }, good_example: { type: 'string' },
          evidence_count: { type: 'number' },
        },
      },
    },
  },
}

phase('Canonicalize')
// Map each rule to a coarse category bucket so cross-cutting duplicates (e.g.
// XSS/encoding appears in vb, web AND client) land together and get merged once.
function bucket(r) {
  const c = (r.category || '').toLowerCase()
  if (/secur|xss|encod|inject|auth|permission|csrf|sanit/.test(c)) return 'security-permissions'
  if (/null/.test(c)) return 'null-handling'
  if (/error|exception|catch/.test(c)) return 'error-handling'
  if (/valid|input/.test(c)) return 'validation-input'
  if (/sql|data-access|linq|ado|persist|query/.test(c)) return 'data-access-sql'
  if (/resx|local|i18n|translat/.test(c)) return 'localization-resx'
  if (/transpile|typescript|javascript|bundle|client/.test(c)) return 'client-ts-js'
  if (/webform|ui|aspx|control|accessib|markup/.test(c)) return 'webforms-ui'
  if (/concurr|thread|race|async/.test(c)) return 'concurrency-async'
  if (/perf|memory|leak|dispose/.test(c)) return 'performance-resources'
  if (/log|audit/.test(c)) return 'logging-audit'
  if (/nam|style|format|doc/.test(c)) return 'naming-style-docs'
  return 'other'
}
const byBucket = {}
for (const r of all) (byBucket[bucket(r)] || (byBucket[bucket(r)] = [])).push(r)
log(`merging ${all.length} rules from ${batches.length} batches across ${Object.keys(byBucket).length} buckets`)

const MERGE = (buck, rules) => `You are finalizing this team's GENERIC code-review ruleset, distilled from the team's historical CodeRabbit + SonarQube findings across their merged PRs. Below are ${rules.length} candidate rules in the "${buck}" area, produced from separate per-language batches — many overlap or restate each other.

CRITICAL ES5/WebGrease CONSTRAINT: handwritten .js under ~.js/ must stay ES5 (WebGrease 1.6.0). Any candidate rule that recommends ES2015+ syntax (optional chaining, ??, arrow fns, template literals, let/const, classes, destructuring, async/await, replaceAll) for handwritten .js is WRONG — rewrite it to scope modern syntax to ".ts source only" or flip it to "keep handwritten .js ES5-compatible". Keep/strengthen the explicit ES5 rule and the ".ts change => regenerate committed .js" rule.

Produce the FINAL canonical rules for this area:
- Merge duplicate / cross-cutting rules into one (SUM their evidence_count; union the languages).
- KEEP ONLY rules that are GENERIC and reusable on ANY change — delete anything naming a specific file/feature/PR or that doesn't generalize into a standing convention.
- Each rule: a crisp imperative, one-line "why", category, languages, severity, tiny bad->good example where useful.
- Lose NO real signal — if two rules are genuinely distinct, keep both.
This is a VB.NET-primary ASP.NET WebForms shop with TypeScript(->committed ES5 JS bundles), GIS/Maps, jQuery/Ajax.

CANDIDATE RULES (${buck}):
${JSON.stringify(rules, null, 1).slice(0, 200000)}

Return {rules:[...]} — the deduplicated canonical rules for this area, sorted by evidence_count desc.`

const merged = await parallel(Object.entries(byBucket).map(([buck, rules]) =>
  () => agent(MERGE(buck, rules), { label: `merge:${buck}`, phase: 'Canonicalize', model: 'opus', schema: RULESET, agentType: 'claude' })
    .then(r => (r && r.rules) ? r.rules.map(x => ({ ...x, category: x.category || buck })) : [])))
const rules = merged.filter(Boolean).flat()
rules.sort((a, b) => (b.evidence_count || 0) - (a.evidence_count || 0))
log(`FINAL generic ruleset: ${rules.length} rules`)
return { rule_count: rules.length, rules }
