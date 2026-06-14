// Domain-decomposed, Engram-specialist-backed planning -> implement -> parity.
//
// The user's design: specialist planners, each grounded in the matching Engram
// SPECIALIST output (pre-fetched, since workflow agents can't call Engram), not
// just raw code reading. Per story:
//   precedent planner  <- find_similar_changes + git precedent diffs
//   ui planner         <- get_page_context (control table + method index)
//   style planner      <- analyze_file_coding_style (VB-language-aware)
//   coverage planner   <- the get_change_set dossier (file list + checklist)
// -> architect synthesizes one decided, file-by-file plan
// -> implementer builds from the plan -> 3 strict reviewers judge parity.
// Baselines: one-shot 2/15, completeness-force 0/15, generic plan-fanout 3/15.
export const meta = {
  name: 'engram-domainplan-parity',
  description: 'Engram-specialist-backed domain planners -> implement -> 3-judge parity vs baselines',
  phases: [
    { title: 'Plan', detail: '4 specialist planners (precedent/ui/style/coverage), each on its Engram specialist output', model: 'opus' },
    { title: 'Synthesize', detail: 'architect merges into one decided implementation plan', model: 'opus' },
    { title: 'Implement', detail: 'dev agent implements from the plan', model: 'opus' },
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
const base = (s) => `USER STORY:\n${storyText(s)}\n\nCodebase (BEFORE this story), READ-ONLY at:\n  ${s.worktree}\nUse Read/Grep/Glob (and \`git -C ${s.worktree} ...\`) to VERIFY against real code. Do NOT edit.`

// role -> (story) -> prompt. Each planner leads with its Engram specialist file.
const PLANNERS = [
  ['precedent', (s) => `You are the PRECEDENT planner. Engram fetched how this TEAM actually made the most similar past changes — read it first:\n  ${s.ctx_similar_path || s.dossier_path}\nFrom these real diffs, decide the MECHANISM and LAYERING this kind of change uses on this codebase (e.g. does enabling a behavior ADD a gate/setting or REMOVE one? which layers does it span — server, client TS + compiled bundle, SQL, resx?). Cite the precedent that justifies each decision.\n\n${base(s)}`],
  ['ui', (s) => `You are the UI planner. Engram fetched the structure (control table + method index with line ranges) of the top candidate page(s) — read it first:\n  ${s.ctx_ui_path || s.dossier_path}\nDetermine the exact controls/handlers/methods to change and how the UI currently behaves; identify any PARALLEL client surface (a .ts source + its committed compiled .js bundle) that implements the same feature. If no server page applies, say the change is client-only and locate the .ts/.js.\n\n${base(s)}`],
  ['style', (s) => `You are the STYLE/CONVENTIONS planner. Engram fetched this file's coding-style profile and language-specific risks — read it first:\n  ${s.ctx_style_path || s.dossier_path}\nSpecify how the new code must match the file's conventions (naming, error handling, disposal, doc comments) and which VB/TS footguns to avoid (e.g. On Error Resume Next, disposable without Using, = Nothing vs Is Nothing). Be concrete.\n\n${base(s)}`],
  ['coverage', (s) => `You are the COVERAGE planner. Engram's change-set dossier ranks the files to change and ends with a completeness checklist — read it first:\n  ${s.dossier_path}\nProduce the COMPLETE file/layer list this change must touch and, for each, one-line justification: page markup + code-behind; client .ts source AND its committed compiled .js bundle; EVERY .resx language; SQL migration; settings/permissions. For each dossier candidate decide include/exclude (don't dismiss resx/SQL without checking).\n\n${base(s)}`],
]

const SYNTH = (s, plans) => `You are the ARCHITECT. Merge the four specialist plans into ONE decided, deduplicated, file-by-file implementation plan the implementer can follow without re-deciding scope. Resolve conflicts explicitly (e.g. precedent says REMOVE a setting but coverage says keep it — decide, citing the precedent). Be explicit about EVERY file + the precise change, including client .ts + compiled .js and all resx languages where the precedent/coverage indicate, and OMIT files that don't belong.\n\n${base(s)}\n\nSPECIALIST PLANS:\n${PLANNERS.map(([r], i) => `### ${r}\n${(plans[i] || '(none)').slice(0, 7000)}`).join('\n\n')}\n\nReturn the final plan (markdown): ordered steps, each naming the exact file + precise change.`

const IMPL = (s, plan) => `You are a senior engineer. Implement the story by following the PLAN exactly (built by specialist planners against the real code + this team's precedents). Produce every file you would add/modify with the key code, including client .ts + compiled .js and all resx languages the plan specifies. If the plan is wrong about a file, fix it; otherwise implement it fully.\n\n${base(s)}\n\nPLAN:\n${(plan || '').slice(0, 16000)}\n\nReturn the full set of file changes with actual code.`

const JUDGE = (s, lens, prop) => `You are a SENIOR engineer doing a strict merge review. Is the proposal at DEVELOPER PARITY with the real merged PR — would you merge it as functionally equivalent to what shipped?\n\n${storyText(s)}\n\nREAL MERGED files:\n${(s.modified_files || []).map(f => '  - ' + f).join('\n')}\nReal merged diff — READ it: ${s.merged_diff_path}\n\nPROPOSAL:\n${JSON.stringify(prop, null, 2).slice(0, 14000)}\n\nEmphasize: ${lens}. Parity = TRUE only if same behavior, the files that matter, nothing material omitted. Partial/headline-only, wrong mechanism, or missing companions (client TS/JS, resx, SQL, settings) = NOT parity. Be strict; default false when unsure.`

phase('Plan')
const results = await pipeline(
  stories,
  (s) => parallel(PLANNERS.map(([role, fn]) =>
    () => agent(fn(s), { label: `plan:${role}:pr${s.pr_id}`, phase: 'Plan', model: 'opus', agentType: 'claude' })
  )).then(plans => ({ s, plans })),
  ({ s, plans }) => agent(SYNTH(s, plans), { label: `synth:pr${s.pr_id}`, phase: 'Synthesize', model: 'opus', agentType: 'claude' }).then(plan => ({ s, plan })),
  ({ s, plan }) => agent(IMPL(s, plan), { label: `impl:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL, agentType: 'claude' }).then(prop => ({ s, prop })),
  ({ s, prop }) => parallel(['behavioral equivalence', 'completeness (all files/companions)', 'correctness & edge cases'].map((lens, i) =>
    () => agent(JUDGE(s, lens, prop), { label: `parity:pr${s.pr_id}:j${i + 1}`, phase: 'Audit', model: 'opus', schema: VERDICT, agentType: 'claude' })
  )).then(votes => {
    const v = votes.filter(Boolean); const yes = v.filter(x => x.parity).length
    return { pr_id: s.pr_id, title: s.title, votes_yes: yes, parity: yes >= 2, proposal: prop, votes: v }
  }),
)
const ok = results.filter(Boolean)
const pc = ok.filter(r => r.parity).length
log(`DOMAINPLAN PARITY: ${pc}/${ok.length} (baselines: one-shot 2/15, completeness 0/15, plan-fanout 3/15). Per-PR: ${ok.map(r => r.pr_id + '=' + r.votes_yes + '/3' + (r.parity ? '✓' : '')).join(' ')}`)
return { parity_rate: `${pc}/${ok.length}`, parity_count: pc, total: ok.length, baselines: { oneshot: '2/15', completeness: '0/15', planfanout: '3/15' }, results: ok }
