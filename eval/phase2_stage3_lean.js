// LEAN Stage-3 A/B — cheap + Sonnet. Question: does injecting the team's distilled
// generic ruleset (the context a user story omits) make the implementation more
// parity-faithful? Both arms get the SAME story + Engram dossier; only Stage-3 also
// gets team_knowledge (copilot + .coderabbit.yaml + board + 258 distilled rules).
//
// Per PR (3 agents): impl_control + impl_stage3 (parallel) -> 1 paired judge that
// scores each arm binary-parity AND graded 0-5 (graded is sensitive to lift even
// under a floor). 7 PRs x 3 = 21 Sonnet agents (~1/30th the prior 119-Opus run).
export const meta = {
  name: 'stage3-lean-ab',
  description: 'Lean Stage-3 A/B (Sonnet): control vs team-knowledge, paired judge, 21 agents',
  phases: [
    { title: 'Implement', detail: 'control + stage3 impl per PR (Sonnet)', model: 'sonnet' },
    { title: 'Judge', detail: 'one paired judge per PR: parity + 0-5 per arm', model: 'sonnet' },
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
  type: 'object',
  required: ['control_parity', 'stage3_parity', 'control_score', 'stage3_score', 'closer', 'reason'],
  properties: {
    control_parity: { type: 'boolean', description: 'control proposal is merge-equivalent to the real PR' },
    stage3_parity: { type: 'boolean', description: 'stage3 proposal is merge-equivalent to the real PR' },
    control_score: { type: 'number', description: '0-5: how close control is to the real merged PR' },
    stage3_score: { type: 'number', description: '0-5: how close stage3 is to the real merged PR' },
    closer: { type: 'string', enum: ['control', 'stage3', 'tie'], description: 'which is closer to the real PR' },
    reason: { type: 'string' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance:\n${s.acceptance}`
  return t
}
const base = (s) => `USER STORY:\n${storyText(s)}\n\nCodebase (BEFORE this story), READ-ONLY at:\n  ${s.worktree}\nUse Read/Grep/Glob (and \`git -C ${s.worktree} ...\`) to VERIFY against the real code before implementing. Do NOT edit files.\n\nEngram change-set dossier (ranked files to touch + completeness checklist):\n  ${s.dossier_path}`

const IMPL_CONTROL = (s) => `You are a senior engineer implementing this story on a VB.NET ASP.NET WebForms + TypeScript/JS codebase. Implement it fully: produce every file you would add/modify with the key code, covering all layers the change needs (page markup + code-behind, client .ts + its committed compiled .js bundle, every .resx language, SQL, settings/permissions).\n\n${base(s)}\n\nReturn the full set of file changes with actual code.`

const IMPL_STAGE3 = (s) => `You are a senior engineer implementing this story on a VB.NET ASP.NET WebForms + TypeScript/JS codebase. Implement it fully (all layers: markup + code-behind, client .ts + committed compiled .js, every .resx language, SQL, settings/permissions).\n\nBEFORE you implement, read this team's accumulated knowledge — the conventions and recurring mistakes a developer here carries in their head, which the story does NOT state:\n  ${s.ctx_qualitygate_path}\nApply every rule relevant to THIS change (e.g. error handling via Try/Catch + api.LogError, Is Nothing guards, CheckWrite + project/tenant membership for writes, TryParse for request input, encode dynamic output, strongly-typed resx in EVERY language, when a .ts changes update its committed .js bundle, handwritten .js under ~.js/ stays ES5, schema in the SSDT model not post-deploy). Do NOT repeat the team's known mistakes.\n\n${base(s)}\n\nReturn the full set of file changes with actual code.`

const JUDGE = (s, propC, propS) => `You are a SENIOR engineer doing a strict merge review of TWO independent implementations of the same story, against the REAL merged PR.\n\n${storyText(s)}\n\nREAL MERGED files:\n${(s.modified_files || []).map(f => '  - ' + f).join('\n')}\nReal merged diff — READ it: ${s.merged_diff_path}\n\nIMPLEMENTATION A:\n${JSON.stringify(propC, null, 2).slice(0, 12000)}\n\nIMPLEMENTATION B:\n${JSON.stringify(propS, null, 2).slice(0, 12000)}\n\nFor EACH implementation independently: is it at DEVELOPER PARITY (would you merge it as functionally equivalent to the real PR — same behavior, the files that matter, nothing material omitted)? Default parity=false when unsure. Also give each a 0-5 closeness score (5 = merge-equivalent, 0 = wrong/empty) and say which is closer to the real PR. A is "control", B is "stage3". Be strict and even-handed; judge on the real diff, not on which sounds more thorough.`

phase('Implement')
const results = await pipeline(
  stories,
  (s) => parallel([
    () => agent(IMPL_CONTROL(s), { label: `impl:control:pr${s.pr_id}`, phase: 'Implement', model: 'sonnet', schema: PROPOSAL, agentType: 'claude' }),
    () => agent(IMPL_STAGE3(s), { label: `impl:stage3:pr${s.pr_id}`, phase: 'Implement', model: 'sonnet', schema: PROPOSAL, agentType: 'claude' }),
  ]).then(([propC, propS]) => ({ s, propC, propS })),
  ({ s, propC, propS }) => agent(JUDGE(s, propC, propS), { label: `judge:pr${s.pr_id}`, phase: 'Judge', model: 'sonnet', schema: VERDICT, agentType: 'claude' })
    .then(v => ({ pr_id: s.pr_id, title: s.title, v, propC, propS })),
)

const ok = results.filter(Boolean).filter(r => r.v)
const cPar = ok.filter(r => r.v.control_parity).length
const sPar = ok.filter(r => r.v.stage3_parity).length
const cAvg = ok.reduce((a, r) => a + (r.v.control_score || 0), 0) / (ok.length || 1)
const sAvg = ok.reduce((a, r) => a + (r.v.stage3_score || 0), 0) / (ok.length || 1)
const closer = { control: 0, stage3: 0, tie: 0 }
ok.forEach(r => { closer[r.v.closer] = (closer[r.v.closer] || 0) + 1 })
log(`LEAN STAGE-3 A/B (Sonnet, n=${ok.length})  parity: control ${cPar} vs stage3 ${sPar}  |  ` +
    `avg score: control ${cAvg.toFixed(2)} vs stage3 ${sAvg.toFixed(2)} (Δ${(sAvg - cAvg).toFixed(2)})  |  ` +
    `closer: stage3=${closer.stage3} control=${closer.control} tie=${closer.tie}`)
for (const r of ok) {
  log(`  PR ${r.pr_id}: ctrl ${r.v.control_score}/5${r.v.control_parity ? '✓' : ''}  stage3 ${r.v.stage3_score}/5${r.v.stage3_parity ? '✓' : ''}  closer=${r.v.closer} — ${r.title.slice(0, 44)}`)
}
return {
  n: ok.length,
  control_parity: cPar, stage3_parity: sPar,
  control_avg: +cAvg.toFixed(2), stage3_avg: +sAvg.toFixed(2), score_lift: +(sAvg - cAvg).toFixed(2),
  closer,
  results: ok.map(r => ({ pr_id: r.pr_id, title: r.title, ...r.v })),
}
