// Completeness-loop parity test — does forcing the agent to cover EVERY layer the
// dossier surfaces lift developer parity above the one-shot baseline (2/15)?
//
// Diagnosis: the one-shot agent drafts the headline (server-side) change and stops,
// missing the client TS/compiled-JS layer, the full resx language set, SQL, and
// setting/permission companions — even though the dossier LISTS them with a
// completeness checklist. This arm mandates dossier-coverage (draft -> self-check
// every dossier file/layer -> complete), then 3 strict reviewers judge parity.
export const meta = {
  name: 'engram-completeness-parity',
  description: 'Completeness-forced Engram arm + 3-judge parity audit vs the one-shot 2/15 baseline',
  phases: [
    { title: 'Implement', detail: 'completeness-forced code-gen: cover every dossier file/layer', model: 'opus' },
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

const CODEGEN = (s) => `You are a senior engineer implementing a user story in the OciusX codebase (ASP.NET WebForms: VB.NET + TypeScript/JavaScript/jQuery + SQL). Implement the COMPLETE change a developer would merge — not just the headline.

USER STORY:
${storyText(s)}

Codebase (BEFORE this story), READ-ONLY at:
  ${s.worktree}
Investigate ONLY this checkout (Read/Grep/Glob, git -C ${s.worktree} ...). Do NOT edit; RETURN every file you would add/modify with the key code.

Engram analyzed this story. READ its dossier first — it ranks the files to change AND ends with a completeness checklist:
  ${s.dossier_path}

CRITICAL — close the loop before you answer (this is where one-shot drafts fail):
1. Draft the change.
2. Then GO BACK through the dossier file list and checklist and verify you addressed EVERY layer this story spans, not just the server side:
   - BOTH the .aspx/.ascx markup AND its .aspx.vb/.ascx.vb code-behind for every page.
   - The CLIENT side: every relevant .ts source AND its committed compiled .js bundle (these legacy apps ship both; a feature on a map/page usually has a parallel TS implementation — do NOT assume "no client change").
   - EVERY .resx language file (de/en/es/no/pt/sl/default), not only the default.
   - The SQL migration for any schema/setting/seed change.
   - Setting/config (add OR remove) + permission/auth changes.
3. For each dossier file you are NOT changing, be sure that's truly correct — the default expectation is that a co-change/golden file IS part of this change.
Add every file you missed. Return the FULL set.

Be concrete: real files at ${s.worktree} (or new files in the right place) with the actual code.`

const JUDGE = (s, lens) => `You are a SENIOR engineer doing a strict merge review. Decide ONE thing: is the proposed implementation at DEVELOPER PARITY with the real merged PR — would you merge it as functionally equivalent to what shipped?

USER STORY:
${storyText(s)}

REAL MERGED files:
${(s.modified_files || []).map(f => '  - ' + f).join('\n')}
Real merged diff — READ it: ${s.merged_diff_path}

PROPOSAL TO JUDGE:
{{PROP}}

Emphasize: ${lens}. Parity = TRUE only if same behavior, the files that matter, nothing material omitted. A partial/headline-only draft, wrong mechanism, or missing companions (client TS/JS, resx languages, SQL, settings) = NOT parity. Be strict; default false when unsure.`

phase('Implement')
const results = await pipeline(
  stories,
  (s) => agent(CODEGEN(s), { label: `complete:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL, agentType: 'claude' }).then(p => ({ s, p })),
  ({ s, p }) => {
    const lenses = ['behavioral equivalence', 'completeness (all files/companions)', 'correctness & edge cases']
    return parallel(lenses.map((lens, i) =>
      () => agent(JUDGE(s, lens).replace('{{PROP}}', JSON.stringify(p, null, 2).slice(0, 14000)),
        { label: `parity:pr${s.pr_id}:j${i + 1}`, phase: 'Audit', model: 'opus', schema: VERDICT, agentType: 'claude' })
    )).then(votes => {
      const v = votes.filter(Boolean); const yes = v.filter(x => x.parity).length
      return { pr_id: s.pr_id, title: s.title, votes_yes: yes, parity: yes >= 2, proposal: p, votes: v }
    })
  },
)
const ok = results.filter(Boolean)
const pc = ok.filter(r => r.parity).length
log(`COMPLETENESS-LOOP PARITY: ${pc}/${ok.length} (baseline one-shot was 2/15). Per-PR: ${ok.map(r => r.pr_id + '=' + r.votes_yes + '/3' + (r.parity ? '✓' : '')).join(' ')}`)
return { parity_rate: `${pc}/${ok.length}`, parity_count: pc, total: ok.length, baseline: '2/15', results: ok }
