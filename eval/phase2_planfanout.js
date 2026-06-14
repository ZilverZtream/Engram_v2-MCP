// Planning-fanout harness — does a multi-agent PLANNING phase get the implementing
// agent to developer parity? Hypothesis (user's): the one-shot agent under-scopes
// because it plans in its head; give it a detailed plan built by several specialist
// planners first. Per story: 4 parallel planners (diverse lenses, read the dossier +
// the actual code) -> 1 architect synthesizes one detailed plan -> 1 dev agent
// implements FROM the plan -> 3 strict reviewers judge developer parity.
// Baseline to beat: one-shot 2/15, completeness-force 0/15.
export const meta = {
  name: 'engram-planfanout-parity',
  description: 'Multi-agent planning phase -> implement -> 3-judge parity audit vs the 2/15 one-shot baseline',
  phases: [
    { title: 'Plan', detail: '4 specialist planners per story (scope / mechanism / code-paths / companions)', model: 'opus' },
    { title: 'Synthesize', detail: 'architect merges into one detailed implementation plan', model: 'opus' },
    { title: 'Implement', detail: 'dev agent implements strictly from the plan', model: 'opus' },
    { title: 'Audit', detail: '3 strict reviewers vote merge-equivalent yes/no', model: 'opus' },
  ],
}

let STORIES = null // INJECTED_STORIES
const stories = (STORIES && STORIES.length) ? STORIES : (Array.isArray(args) ? args : (args ? [args] : []))

const PROPOSAL = {
  type: 'object', required: ['approach', 'files'],
  properties: {
    approach: { type: 'string' },
    files: { type: 'array', items: { type: 'object', required: ['path', 'action', 'change'],
      properties: { path: { type: 'string' }, action: { type: 'string', enum: ['add', 'modify'] }, change: { type: 'string' } } } },
  },
}
const VERDICT = {
  type: 'object', required: ['parity', 'confidence', 'missing', 'reason'],
  properties: {
    parity: { type: 'boolean', description: 'true ONLY if a senior reviewer would merge this as functionally equivalent to the real PR. Default false when unsure.' },
    confidence: { type: 'number' }, missing: { type: 'array', items: { type: 'string' } }, reason: { type: 'string' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance:\n${s.acceptance}`
  return t
}
const ctx = (s) => `USER STORY:
${storyText(s)}

Codebase (BEFORE this story), READ-ONLY at:
  ${s.worktree}
Investigate ONLY this checkout (Read/Grep/Glob, and \`git -C ${s.worktree} log/-S/show\` for history). Do NOT edit files.

Engram analyzed this story; its dossier ranks the files to change and ends with a completeness checklist:
  ${s.dossier_path}`

const PLANNERS = [
  ['scope', `You are the SCOPE planner. Output the COMPLETE file/layer list this change must touch and, for EACH, one line of justification. Cover every layer this kind of story spans: .aspx/.ascx markup AND its code-behind; the CLIENT side (every relevant .ts source AND its committed compiled .js bundle — legacy apps ship both); EVERY .resx language file (not just default); SQL migration; setting/config add-or-REMOVE; permission/auth. For each dossier candidate, decide include or exclude with a reason (do not blanket-dismiss resx/SQL as noise — verify by reading the code/history). Flag files you're unsure about.`],
  ['mechanism', `You are the MECHANISM/CONVENTION planner. Determine HOW this team actually implements this kind of change, from git history (\`git -C <wt> log\`, \`git log -S<symbol>\`, \`git show\` of similar past commits) and the code. Decide the real approach: e.g. does enabling a behavior mean ADDING a gate/setting or REMOVING one? How is client behavior wired (event handlers, toggle functions)? Name the concrete mechanism and cite the precedent commit/code you based it on.`],
  ['codepaths', `You are the CODE-PATH planner. Using the dossier and the actual code, identify the EXACT edit locations: which methods/handlers/controls (with file + approximate line range), which functions to add/modify, and the precise call sites. Be concrete enough that the implementer edits the right lines without searching.`],
  ['companions', `You are the COMPANION/RISK planner. List the easy-to-forget pieces a reviewer would block on: both the .ts AND its compiled .js, ALL resx languages, .designer.vb if controls change, mutual-exclusion / field-reset logic, permission checks, and any second parallel surface (e.g. a server page AND a map/client implementation of the same feature). For each, say why it matters here.`],
]

const PLAN_PROMPT = (s, role, instr) => `${instr}\n\n${ctx(s)}\n\nReturn your ${role} analysis as a concise, concrete plan section (markdown). Ground every claim in files/code you actually read.`

const SYNTH = (s, plans) => `You are the ARCHITECT. Merge the four specialist plans below into ONE detailed, deduplicated implementation plan for this story — an ordered, file-by-file plan the implementer can follow without re-deciding scope. Resolve conflicts (e.g. if the mechanism planner says REMOVE a setting but scope says keep it, decide and say why). Be explicit about EVERY file to touch and the specific change in each, including the client .ts + compiled .js and all resx languages where applicable, and OMIT files that don't belong.

${ctx(s)}

SPECIALIST PLANS:
${plans.map((p, i) => `### ${PLANNERS[i][0]} plan\n${(p || '(none)').slice(0, 8000)}`).join('\n\n')}

Return the final implementation plan (markdown): ordered steps, each naming the exact file + the precise change.`

const IMPL = (s, plan) => `You are a senior engineer. Implement the story by following the PLAN below exactly — it was built by specialist planners against the real code. Produce every file you would add/modify with the key code. Do not re-scope; if the plan is wrong about a file, fix it, but otherwise implement the full plan including client .ts + compiled .js and all resx languages it specifies.

${ctx(s)}

IMPLEMENTATION PLAN:
${(plan || '').slice(0, 16000)}

Return the full set of file changes (real paths under ${s.worktree} or new files in the right place) with actual code.`

const JUDGE = (s, lens, prop) => `You are a SENIOR engineer doing a strict merge review. Is the proposal at DEVELOPER PARITY with the real merged PR — would you merge it as functionally equivalent to what shipped?

${storyText(s)}

REAL MERGED files:
${(s.modified_files || []).map(f => '  - ' + f).join('\n')}
Real merged diff — READ it: ${s.merged_diff_path}

PROPOSAL:
${JSON.stringify(prop, null, 2).slice(0, 14000)}

Emphasize: ${lens}. Parity = TRUE only if same behavior, the files that matter, nothing material omitted. Partial/headline-only, wrong mechanism, or missing companions (client TS/JS, resx languages, SQL, settings) = NOT parity. Be strict; default false when unsure.`

phase('Plan')
const results = await pipeline(
  stories,
  // Stage 1: 4 planners in parallel.
  (s) => parallel(PLANNERS.map(([role, instr]) =>
    () => agent(PLAN_PROMPT(s, role, instr), { label: `plan:${role}:pr${s.pr_id}`, phase: 'Plan', model: 'opus', agentType: 'claude' })
  )).then(plans => ({ s, plans })),
  // Stage 2: architect synthesizes one plan.
  ({ s, plans }) => agent(SYNTH(s, plans), { label: `synth:pr${s.pr_id}`, phase: 'Synthesize', model: 'opus', agentType: 'claude' }).then(plan => ({ s, plan })),
  // Stage 3: implement from the plan.
  ({ s, plan }) => agent(IMPL(s, plan), { label: `impl:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL, agentType: 'claude' }).then(prop => ({ s, prop })),
  // Stage 4: 3-judge parity.
  ({ s, prop }) => parallel(['behavioral equivalence', 'completeness (all files/companions)', 'correctness & edge cases'].map((lens, i) =>
    () => agent(JUDGE(s, lens, prop), { label: `parity:pr${s.pr_id}:j${i + 1}`, phase: 'Audit', model: 'opus', schema: VERDICT, agentType: 'claude' })
  )).then(votes => {
    const v = votes.filter(Boolean); const yes = v.filter(x => x.parity).length
    return { pr_id: s.pr_id, title: s.title, votes_yes: yes, parity: yes >= 2, proposal: prop, votes: v }
  }),
)
const ok = results.filter(Boolean)
const pc = ok.filter(r => r.parity).length
log(`PLAN-FANOUT PARITY: ${pc}/${ok.length} (baselines: one-shot 2/15, completeness-force 0/15). Per-PR: ${ok.map(r => r.pr_id + '=' + r.votes_yes + '/3' + (r.parity ? '✓' : '')).join(' ')}`)
return { parity_rate: `${pc}/${ok.length}`, parity_count: pc, total: ok.length, baselines: { oneshot: '2/15', completeness: '0/15' }, results: ok }
