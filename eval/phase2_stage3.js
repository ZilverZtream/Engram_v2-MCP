// Stage-3 A/B: does Engram serving the team's accumulated knowledge (the context
// the user story OMITS) lift developer parity above the story-alone ceiling?
//
// Head-to-head on the SAME pilot PRs, identical 3-judge parity (judges never see
// the quality gates — they score against the real merged diff only). Both arms
// share the 4 base domain planners AND both get a revise pass, so the ONLY
// difference is whether the team quality-gate knowledge is injected:
//
//   CONTROL  : 4 base plans -> architect -> impl -> generic self-review -> judge×3
//   STAGE3   : 4 base plans + TEAM-MEMORY plan -> architect(QG-aware) -> impl
//              -> pre-push QG audit -> revise -> judge×3
//
// ctx_qualitygate.md per PR = copilot-instructions + recurring-issues board +
// file-scoped CodeRabbit/Sonar findings on the predicted-touched files.
// Baseline (no QG, domainplan): 3/15.
export const meta = {
  name: 'engram-stage3-parity-ab',
  description: 'Stage-3 quality-gate A/B: control vs team-knowledge-injected, 3-judge parity',
  phases: [
    { title: 'Plan', detail: '4 base planners (shared) + team-memory planner (stage3)', model: 'opus' },
    { title: 'Synthesize', detail: 'control architect + QG-aware architect', model: 'opus' },
    { title: 'Implement', detail: 'control impl + stage3 impl', model: 'opus' },
    { title: 'Revise', detail: 'control generic review + stage3 pre-push QG audit→revise', model: 'opus' },
    { title: 'Audit', detail: '3 strict judges per arm (no QG; vs real diff)', model: 'opus' },
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
const base = (s) => `USER STORY:\n${storyText(s)}\n\nCodebase (BEFORE this story), READ-ONLY at:\n  ${s.worktree}\nUse Read/Grep/Glob (and \`git -C ${s.worktree} ...\`) to VERIFY against real code. Do NOT edit.`

// 4 base domain planners (shared by both arms).
const PLANNERS = [
  ['precedent', (s) => `You are the PRECEDENT planner. Engram fetched how this TEAM actually made the most similar past changes — read it first:\n  ${s.ctx_similar_path || s.dossier_path}\nFrom these real diffs, decide the MECHANISM and LAYERING this kind of change uses on this codebase (e.g. does enabling a behavior ADD a gate/setting or REMOVE one? which layers does it span — server, client TS + compiled bundle, SQL, resx?). Cite the precedent that justifies each decision.\n\n${base(s)}`],
  ['ui', (s) => `You are the UI planner. Engram fetched the structure (control table + method index with line ranges) of the top candidate page(s) — read it first:\n  ${s.ctx_ui_path || s.dossier_path}\nDetermine the exact controls/handlers/methods to change and how the UI currently behaves; identify any PARALLEL client surface (a .ts source + its committed compiled .js bundle) that implements the same feature.\n\n${base(s)}`],
  ['style', (s) => `You are the STYLE/CONVENTIONS planner. Engram fetched this file's coding-style profile and language-specific risks — read it first:\n  ${s.ctx_style_path || s.dossier_path}\nSpecify how the new code must match the file's conventions (naming, error handling, disposal, doc comments) and which VB/TS footguns to avoid (On Error Resume Next, disposable without Using, = Nothing vs Is Nothing).\n\n${base(s)}`],
  ['coverage', (s) => `You are the COVERAGE planner. Engram's change-set dossier ranks the files to change and ends with a completeness checklist — read it first:\n  ${s.dossier_path}\nProduce the COMPLETE file/layer list this change must touch and, for each, one-line justification: page markup + code-behind; client .ts source AND its committed compiled .js bundle; EVERY .resx language; SQL migration; settings/permissions.\n\n${base(s)}`],
]

// Stage-3 only: the team-memory planner, grounded in the quality-gate corpus.
const MEMORY_PLAN = (s) => `You are the TEAM-MEMORY planner. Engram serves this team's accumulated knowledge that a user story does NOT state — read it first:\n  ${s.ctx_qualitygate_path}\nThis is the team's coding rulebook (copilot-instructions), the recurring-issues board, and a GENERIC ruleset DISTILLED from ~2400 of this team's historical CodeRabbit/Sonar review findings (each rule tagged with how often the team tripped on it). It is NOT file-specific — it is the conventions and recurring mistakes that apply across the codebase. Your job: from this corpus, select the rules that APPLY to THIS story and turn them into CONCRETE directives for the implementer — which conventions to follow (e.g. error handling via Try/Catch + api.LogError, Is Nothing guards on every lookup/DAO return, permission gates via _us.UserAccess.CheckWrite for writes + project/tenant membership, TryParse for request input, encode dynamic output, strongly-typed resx in every language) and which recurring mistakes to avoid (e.g. when a .ts changes its committed .js bundle MUST be updated; ES5/WebGrease constraints on handwritten .js; schema in the SSDT model not post-deploy scripts; multi-window/session). Cite the rule and say exactly how it applies to the files this change touches.\n\n${base(s)}`

const SYNTH_CONTROL = (s, plans) => `You are the ARCHITECT. Merge the four specialist plans into ONE decided, deduplicated, file-by-file implementation plan the implementer can follow without re-deciding scope. Resolve conflicts explicitly. Be explicit about EVERY file + the precise change, including client .ts + compiled .js and all resx languages where indicated, and OMIT files that don't belong.\n\n${base(s)}\n\nSPECIALIST PLANS:\n${PLANNERS.map(([r], i) => `### ${r}\n${(plans[i] || '(none)').slice(0, 6500)}`).join('\n\n')}\n\nReturn the final plan (markdown): ordered steps, each naming the exact file + precise change.`

const SYNTH_STAGE3 = (s, plans, memPlan) => `You are the ARCHITECT. Merge the four specialist plans AND the team-memory plan into ONE decided, deduplicated, file-by-file implementation plan. The team-memory plan encodes decisions this team has already made that the story omits — HONOR them (conventions to follow, known mistakes to avoid, companion files the team requires like the committed .js for a changed .ts). Resolve conflicts explicitly, citing the team rule or precedent. Be explicit about EVERY file + the precise change.\n\n${base(s)}\n\nSPECIALIST PLANS:\n${PLANNERS.map(([r], i) => `### ${r}\n${(plans[i] || '(none)').slice(0, 5500)}`).join('\n\n')}\n\n### team-memory (team knowledge the story omits — honor it)\n${(memPlan || '(none)').slice(0, 7000)}\n\nReturn the final plan (markdown): ordered steps, each naming the exact file + precise change, noting which team rule each satisfies.`

const IMPL = (s, plan) => `You are a senior engineer. Implement the story by following the PLAN exactly (built by specialist planners against the real code + this team's precedents). Produce every file you would add/modify with the key code, including client .ts + compiled .js and all resx languages the plan specifies. If the plan is wrong about a file, fix it; otherwise implement it fully.\n\n${base(s)}\n\nPLAN:\n${(plan || '').slice(0, 16000)}\n\nReturn the full set of file changes with actual code.`

// Control gets a generic self-review pass (to control for "an extra revision pass"
// being the lever rather than the QG knowledge itself).
const REVIEW_CONTROL = (s, prop) => `You are a senior engineer doing a final pre-push self-review of your own change against general best practice (completeness, correctness, edge cases, that every layer a change like this needs is covered — markup+code-behind, client .ts+compiled .js, resx, SQL, settings/permissions). Revise the proposal to fix any gap you find.\n\n${base(s)}\n\nCURRENT PROPOSAL:\n${JSON.stringify(prop, null, 2).slice(0, 14000)}\n\nReturn the REVISED full set of file changes.`

// Stage3 gets the real pre-push audit against the team quality gates.
const AUDIT_STAGE3 = (s, prop) => `You are the FINAL PRE-PUSH AUDITOR. Check this proposed change against the team's quality gates BEFORE it is pushed — read them:\n  ${s.ctx_qualitygate_path}\nThese are the team's coding rulebook, recurring-mistake board, and a GENERIC ruleset distilled from ~2400 of the team's past review findings (with recurrence counts). Go through the rules that apply to this change and verify the proposal complies; where it violates one or omits something the team requires (e.g. the committed .js bundle for a changed .ts; a missing CheckWrite/membership gate; unguarded lookup/DAO return; throwing CInt/Parse on request input; unencoded dynamic output; a missing resx language; schema placed in a post-deploy script), FIX it. Then return the corrected, complete implementation.\n\n${base(s)}\n\nCURRENT PROPOSAL:\n${JSON.stringify(prop, null, 2).slice(0, 14000)}\n\nReturn the REVISED full set of file changes that passes the team's pre-push audit.`

const JUDGE = (s, lens, prop) => `You are a SENIOR engineer doing a strict merge review. Is the proposal at DEVELOPER PARITY with the real merged PR — would you merge it as functionally equivalent to what shipped?\n\n${storyText(s)}\n\nREAL MERGED files:\n${(s.modified_files || []).map(f => '  - ' + f).join('\n')}\nReal merged diff — READ it: ${s.merged_diff_path}\n\nPROPOSAL:\n${JSON.stringify(prop, null, 2).slice(0, 14000)}\n\nEmphasize: ${lens}. Parity = TRUE only if same behavior, the files that matter, nothing material omitted. Partial/headline-only, wrong mechanism, or missing companions (client TS/JS, resx, SQL, settings) = NOT parity. Be strict; default false when unsure.`

const LENSES = ['behavioral equivalence', 'completeness (all files/companions)', 'correctness & edge cases']
const judgeArm = (s, prop, tag) => parallel(LENSES.map((lens, i) =>
  () => agent(JUDGE(s, lens, prop), { label: `judge:${tag}:pr${s.pr_id}:j${i + 1}`, phase: 'Audit', model: 'opus', schema: VERDICT, agentType: 'claude' })
)).then(votes => {
  const v = votes.filter(Boolean); const yes = v.filter(x => x.parity).length
  return { tag, pr_id: s.pr_id, votes_yes: yes, parity: yes >= 2, proposal: prop, votes: v }
})

async function runStory(s) {
  // 1. base planners (shared) + memory planner (stage3) — all concurrent
  const basePlans = await parallel(PLANNERS.map(([role, fn]) =>
    () => agent(fn(s), { label: `plan:${role}:pr${s.pr_id}`, phase: 'Plan', model: 'opus', agentType: 'claude' })))
  const memPlan = await agent(MEMORY_PLAN(s), { label: `plan:memory:pr${s.pr_id}`, phase: 'Plan', model: 'opus', agentType: 'claude' })

  // 2. two architects
  const [planC, planS] = await Promise.all([
    agent(SYNTH_CONTROL(s, basePlans), { label: `synth:ctrl:pr${s.pr_id}`, phase: 'Synthesize', model: 'opus', agentType: 'claude' }),
    agent(SYNTH_STAGE3(s, basePlans, memPlan), { label: `synth:s3:pr${s.pr_id}`, phase: 'Synthesize', model: 'opus', agentType: 'claude' }),
  ])

  // 3. two implementers
  const [propC0, propS0] = await Promise.all([
    agent(IMPL(s, planC), { label: `impl:ctrl:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL, agentType: 'claude' }),
    agent(IMPL(s, planS), { label: `impl:s3:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL, agentType: 'claude' }),
  ])

  // 4. revise: control generic; stage3 QG audit
  const [propC, propS] = await Promise.all([
    agent(REVIEW_CONTROL(s, propC0), { label: `revise:ctrl:pr${s.pr_id}`, phase: 'Revise', model: 'opus', schema: PROPOSAL, agentType: 'claude' }),
    agent(AUDIT_STAGE3(s, propS0), { label: `audit:s3:pr${s.pr_id}`, phase: 'Revise', model: 'opus', schema: PROPOSAL, agentType: 'claude' }),
  ])

  // 5. judge both arms with identical judges
  const [rC, rS] = await Promise.all([judgeArm(s, propC, 'control'), judgeArm(s, propS, 'stage3')])
  return { pr_id: s.pr_id, title: s.title, control: rC, stage3: rS }
}

phase('Plan')
const results = (await parallel(stories.map(s => () => runStory(s)))).filter(Boolean)

const cC = results.filter(r => r.control.parity).length
const cS = results.filter(r => r.stage3.parity).length
const flips = results.filter(r => !r.control.parity && r.stage3.parity).map(r => r.pr_id)
const regress = results.filter(r => r.control.parity && !r.stage3.parity).map(r => r.pr_id)
log(`STAGE-3 A/B  control=${cC}/${results.length}  stage3=${cS}/${results.length}  ` +
    `gained=[${flips.join(',')}]  lost=[${regress.join(',')}]`)
for (const r of results) {
  log(`  PR ${r.pr_id}: control ${r.control.votes_yes}/3${r.control.parity ? '✓' : ''}  ` +
      `stage3 ${r.stage3.votes_yes}/3${r.stage3.parity ? '✓' : ''}  — ${r.title.slice(0, 50)}`)
}
return {
  control: `${cC}/${results.length}`, stage3: `${cS}/${results.length}`,
  lift: cS - cC, gained: flips, lost: regress,
  results: results.map(r => ({
    pr_id: r.pr_id, title: r.title,
    control_yes: r.control.votes_yes, control_parity: r.control.parity,
    stage3_yes: r.stage3.votes_yes, stage3_parity: r.stage3.parity,
    control_proposal: r.control.proposal, stage3_proposal: r.stage3.proposal,
    control_votes: r.control.votes, stage3_votes: r.stage3.votes,
  })),
}
