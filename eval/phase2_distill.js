// Distill a raw code-review-finding corpus (CodeRabbit/Sonar history) into a
// compact set of GENERIC, deduplicated project rules that apply to ANY change —
// the team's "what to avoid" knowledge as reusable rules, NOT a file/line lookup.
//
// map    : chunk the findings -> each agent GENERALIZES its chunk into candidate
//          rules (collapse same-root-cause findings into one; drop one-off,
//          file/PR-specific noise that carries no reusable lesson).
// reduce : per category, merge near-duplicate candidates into canonical rules
//          (summing evidence_count).
// Returns this batch's canonical ruleset; a final cross-batch critic merges batches.
export const meta = {
  name: 'distill-quality-gates',
  description: 'Distill raw CodeRabbit/Sonar findings into generic deduplicated project rules',
  phases: [
    { title: 'Map', detail: 'generalize each finding-chunk into candidate rules', model: 'opus' },
    { title: 'Reduce', detail: 'merge candidates per category into canonical rules', model: 'opus' },
  ],
}

let FINDINGS = null // INJECTED_FINDINGS
let BATCH = 'all'   // INJECTED_BATCH
const findings = FINDINGS || (Array.isArray(args) ? args : (args && args.findings) || [])

const CATEGORIES = [
  'error-handling', 'data-access-sql', 'security-permissions', 'null-handling',
  'localization-resx', 'typescript-javascript-transpile', 'webforms-ui',
  'performance', 'concurrency-threading', 'logging-audit', 'naming-style',
  'validation-input', 'api-contract', 'other',
]

const RULESET = {
  type: 'object', required: ['rules'],
  properties: {
    rules: {
      type: 'array',
      items: {
        type: 'object',
        required: ['rule', 'why', 'category', 'languages', 'severity', 'evidence_count'],
        properties: {
          rule: { type: 'string', description: 'Imperative, generic project rule a dev follows on ANY change. No file/PR/feature names.' },
          why: { type: 'string', description: 'One line: the failure mode it prevents.' },
          category: { type: 'string', enum: CATEGORIES },
          languages: { type: 'array', items: { type: 'string' } },
          severity: { type: 'string', enum: ['high', 'medium', 'low'] },
          bad_example: { type: 'string' },
          good_example: { type: 'string' },
          evidence_count: { type: 'number', description: 'How many raw findings this rule generalizes.' },
        },
      },
    },
  },
}

function chunk(arr, n) {
  const out = []
  for (let i = 0; i < arr.length; i += n) out.push(arr.slice(i, i + n))
  return out
}
const fmt = (fs) => fs.map((f, i) => {
  const src = f.source === 'sonar' ? `sonar${f.sonar_rule ? ` ${f.sonar_rule}` : ''}` : (f.source || 'cr')
  const rej = (f.resolution === 'wontFix' || f.resolution === 'byDesign') ? ' REJECTED' : ''
  return `${i + 1}. [${f.lang} · ${src}${rej}${f.freq > 1 ? ` ×${f.freq}` : ''}] ${f.text}`
}).join('\n')

const ES5_CONSTRAINT = `CRITICAL PROJECT CONSTRAINT — ES5 / WebGrease: handwritten JavaScript under \`~.js/\` directories is bundled by WebGrease 1.6.0 (an ES5-era parser) and MUST stay ES5-compatible. NEVER emit a rule telling anyone to USE ES2015+ syntax (optional chaining \`?.\`, nullish \`??\`, arrow functions, template literals, \`let\`/\`const\`, classes, destructuring, async/await, \`String.replaceAll\`, etc.) in handwritten \`.js\`. If a finding (ESPECIALLY a SonarQube finding) suggests modernizing handwritten \`.js\`, the team REJECTED it — turn it into the OPPOSITE rule: "handwritten .js under ~.js/ must remain ES5-compatible; do not apply analyzer modern-syntax suggestions there (WebGrease/NOSONAR)". Modern-syntax guidance is valid ONLY for TypeScript SOURCE (.ts), which the compiler downlevels to ES5 — scope any such rule to ".ts source only". Also: when a .ts changes, its committed .js bundle must be regenerated and committed.`

const MAP = (fs) => `You are distilling this team's CODE-REVIEW HISTORY into GENERIC, reusable project rules.

${ES5_CONSTRAINT}

Below are ${fs.length} real review findings from this team's merged PRs — from CodeRabbit AND SonarQube (tagged with source; Sonar findings carry a rule key). Some are tagged REJECTED (the team marked them Won't-fix/By-design). Your job: GENERALIZE.

RESOLUTION HANDLING:
- A REJECTED finding is one the team DELIBERATELY declined. Do NOT turn it into a "do this" rule. If it is an analyzer (Sonar) modern-syntax/style suggestion on handwritten .js (e.g. "use includes()", "use readonly", "reduce cognitive complexity", "use optional chaining"), the rejection is BECAUSE of the ES5/WebGrease constraint or a deliberate convention — fold it into the ES5/exception rule, not a modernization rule.
- Non-rejected findings (fixed/active/closed) are legit — extract them as rules.

- Collapse EVERY finding that shares a root cause into ONE imperative rule (e.g. ten "wrap SqlConnection in Using" findings -> one rule "Always wrap ADO.NET disposables in a Using block").
- Phrase each rule so it applies to ANY future change — NO file names, NO feature names, NO PR-specific specifics. A developer should be able to follow it blind.
- DROP findings that are purely one-off and carry no reusable lesson (a typo in one comment, a single bespoke logic bug). Keep only what generalizes into a convention.
- Set evidence_count = how many of these findings the rule covers (a proxy for how often the team trips on it).
- Prefer fewer, sharper rules over many overlapping ones.

FINDINGS:
${fmt(fs)}

Return {rules:[...]} — the generic rules these findings distill to.`

const REDUCE = (cat, rules) => `You are canonicalizing candidate project rules in the category "${cat}", produced independently from different slices of the same review history. They contain duplicates and near-duplicates.

- Merge near-identical rules into ONE canonical rule (sum their evidence_count).
- Keep ONLY generic, reusable rules; delete any that still reference a specific file/feature/PR or that don't generalize.
- Make each rule a crisp imperative with a one-line "why" and a tiny bad->good example where it helps.

CANDIDATE RULES (category=${cat}):
${JSON.stringify(rules, null, 1).slice(0, 28000)}

Return {rules:[...]} — the deduplicated canonical rules for this category, sorted by evidence_count desc.`

phase('Map')
const chunks = chunk(findings, 80)
log(`distill batch=${BATCH}: ${findings.length} findings -> ${chunks.length} map chunks`)
const mapped = await parallel(chunks.map((c, i) =>
  () => agent(MAP(c), { label: `map:${BATCH}:${i + 1}/${chunks.length}`, phase: 'Map', model: 'opus', schema: RULESET, agentType: 'claude' })))
const candidates = mapped.filter(Boolean).flatMap(r => r.rules || [])
log(`mapped -> ${candidates.length} candidate rules`)

// group candidates by category, reduce each
phase('Reduce')
const byCat = {}
for (const r of candidates) (byCat[r.category] || (byCat[r.category] = [])).push(r)
const reduced = await parallel(Object.entries(byCat).map(([cat, rules]) =>
  () => agent(REDUCE(cat, rules), { label: `reduce:${BATCH}:${cat}`, phase: 'Reduce', model: 'opus', schema: RULESET, agentType: 'claude' })
    .then(r => (r && r.rules ? r.rules.map(x => ({ ...x, category: x.category || cat })) : []))))
const ruleset = reduced.filter(Boolean).flat()
ruleset.sort((a, b) => (b.evidence_count || 0) - (a.evidence_count || 0))
log(`batch=${BATCH} canonical rules: ${ruleset.length}`)
return { batch: BATCH, candidate_count: candidates.length, rule_count: ruleset.length, rules: ruleset }
