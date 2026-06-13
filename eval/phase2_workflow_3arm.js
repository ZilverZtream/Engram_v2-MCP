// Phase 2c — 3-arm A/B: Model-alone vs Model+Engram(file list) vs Model+Engram(ENRICHED).
//
// Tests whether enriching the dossier from a FILE LIST to an IMPLEMENTATION MAP
// (the top pages' control table + method index with exact line ranges) lifts the
// implementation score above plain file recall. Three Opus agents implement each
// story from the SAME read-only base-commit code; a judge scores all three vs the
// real merged diff. US-only input: code-gen agents never see the PR / ground truth.
export const meta = {
  name: 'phase2-engram-3arm',
  description: 'Alone vs Engram(file-list) vs Engram(enriched implementation map) A/B vs the merged OciusX PR',
  phases: [
    { title: 'Implement', detail: 'three Opus agents propose an implementation per story (none / file-list / enriched)', model: 'opus' },
    { title: 'Judge', detail: 'Opus judge scores all three arms vs the real merged diff', model: 'opus' },
  ],
}

let STORIES = null // INJECTED_STORIES
const stories = (STORIES && STORIES.length) ? STORIES
  : (Array.isArray(args) ? args : (args ? [args] : []))

const PROPOSAL_SCHEMA = {
  type: 'object',
  required: ['approach', 'files'],
  properties: {
    approach: { type: 'string', description: '2-4 sentences: how you implement the story' },
    files: {
      type: 'array',
      description: 'every file you would create or modify',
      items: {
        type: 'object',
        required: ['path', 'action', 'change'],
        properties: {
          path: { type: 'string', description: 'repo-relative path, e.g. Site/App_Code/...' },
          action: { type: 'string', enum: ['add', 'modify'] },
          change: { type: 'string', description: 'what changes here + the key code (snippet or unified diff)' },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['arms', 'winner', 'summary'],
  properties: {
    arms: {
      type: 'array',
      items: {
        type: 'object',
        required: ['arm', 'file_recall', 'impl_score', 'notes'],
        properties: {
          arm: { type: 'string', enum: ['alone', 'engram', 'rich'] },
          file_recall: { type: 'number', description: 'fraction of the real MODIFIED files this arm named (0-1)' },
          impl_score: { type: 'integer', description: '1-5: does the proposed code achieve the same behavior/approach as the merged diff?' },
          notes: { type: 'string' },
        },
      },
    },
    winner: { type: 'string', enum: ['alone', 'engram', 'rich', 'tie'] },
    rich_vs_engram: { type: 'number', description: 'rich impl_score minus engram(file-list) impl_score' },
    summary: { type: 'string', description: 'one line: did the enriched implementation map help beyond the file list?' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance criteria:\n${s.acceptance}`
  return t
}

// mode: 'none' | 'list' | 'rich'
const CODEGEN = (s, mode) => `You are a senior engineer implementing a user story in the OciusX codebase — a large legacy ASP.NET WebForms app (VB.NET, with TypeScript/JavaScript/jQuery and SQL). You are given ONLY the user story, exactly as a developer would be (sometimes just a headline).

USER STORY:
${storyText(s)}

The codebase, as it existed BEFORE this story was implemented, is checked out READ-ONLY at:
  ${mode === 'none' ? (s.worktree_alone || s.worktree) : s.worktree}
Investigate ONLY this checkout — do NOT search other directories or repositories on the machine. Explore it with Read/Grep/Glob (and \`git -C <checkout> ...\` for history if useful). Do NOT edit files — instead RETURN your implementation: every file you would add or modify, with the key code (a snippet or unified diff) and what it does. If the worktree path does not exist or is empty, return an empty files list and say so — do NOT go hunting for a different task elsewhere.
${mode === 'none' ? `
You do NOT have Engram. Use only standard tools (Read/Grep/Glob/git) to locate what to change.` : ''}${mode === 'list' ? `
Engram (a codebase-intelligence tool) has already analyzed this story against the code and history. READ its dossier first — it ranks the files most likely to need changing, by concept-footprint + git co-change + structural-graph signals:
  ${s.dossier_path}
Use it to focus your investigation, but verify against the actual code; include any files it missed and drop any that don't fit.` : ''}${mode === 'rich' ? `
Engram (a codebase-intelligence tool) has already analyzed this story against the code and history. READ its dossier first:
  ${s.dossier_rich_path}
The dossier ranks the files most likely to need changing (concept-footprint + git co-change + structural-graph), AND for the top pages it includes an IMPLEMENTATION MAP: the control table and a method index with exact line ranges. Use the map to jump straight to the precise methods/controls to modify (Read those line ranges from the checkout), so you change the right code and don't miss companion methods. Verify against the actual code; include any files it missed and drop any that don't fit.` : ''}

Be concrete and realistic: name real files that exist at the checkout (or new files in the right place), and show the actual code you'd write. Aim for the complete change the story needs, including the easy-to-forget companions (settings, resources, permissions, registrations, UI bindings).`

const JUDGE = (s) => `You are judging THREE implementation proposals for one user story against the REAL implementation a developer merged.

USER STORY:
${storyText(s)}

THE REAL MERGED IMPLEMENTATION (ground truth):
Real MODIFIED files (existed before the PR — the fair retrieval target):
${(s.modified_files || []).map(f => '  - ' + f).join('\n')}

Real merged diff — READ it from this file (it is the gold standard to compare code against):
  ${s.merged_diff_path}

For EACH arm below, compute:
  - file_recall: fraction of the real MODIFIED files the arm named (match by path; a WebForms page family foo.aspx / foo.aspx.vb / foo.aspx.designer.vb counts as found if any member is named).
  - impl_score (1-5): does the arm's proposed CODE achieve the same behavior and approach as the merged diff? (5 = essentially equivalent, 1 = wrong/irrelevant.)

ARM "alone" (no Engram):
{{ALONE}}

ARM "engram" (Engram file-list dossier):
{{ENGRAM}}

ARM "rich" (Engram dossier + implementation map: control table + method index with line ranges):
{{RICH}}

Return all three arms' scores, the overall winner, rich_vs_engram (rich impl_score − engram impl_score), and a one-line summary of whether the enriched implementation map helped BEYOND the plain file list.`

phase('Implement')
const results = await pipeline(
  stories,
  (s) => parallel([
    () => agent(CODEGEN(s, 'none'), { label: `impl:alone:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
    () => agent(CODEGEN(s, 'list'), { label: `impl:engram:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
    () => agent(CODEGEN(s, 'rich'), { label: `impl:rich:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
  ]).then(([alone, engram, rich]) => ({ s, alone, engram, rich })),
  ({ s, alone, engram, rich }) => {
    const prompt = JUDGE(s)
      .replace('{{ALONE}}', JSON.stringify(alone, null, 2).slice(0, 11000))
      .replace('{{ENGRAM}}', JSON.stringify(engram, null, 2).slice(0, 11000))
      .replace('{{RICH}}', JSON.stringify(rich, null, 2).slice(0, 11000))
    return agent(prompt, { label: `judge:pr${s.pr_id}`, phase: 'Judge', model: 'opus', schema: VERDICT_SCHEMA, agentType: 'claude' })
      .then((verdict) => ({ pr_id: s.pr_id, title: s.title, alone, engram, rich, verdict }))
  },
)

const ok = results.filter(Boolean)
const rd = ok.map(r => r.verdict?.rich_vs_engram).filter(v => typeof v === 'number')
const meanRich = rd.length ? rd.reduce((a, b) => a + b, 0) / rd.length : 0
const wins = { alone: 0, engram: 0, rich: 0, tie: 0 }
ok.forEach(r => { if (r.verdict?.winner) wins[r.verdict.winner]++ })
const meanBy = (arm) => {
  const xs = ok.map(r => (r.verdict?.arms || []).find(a => a.arm === arm)?.impl_score).filter(v => typeof v === 'number')
  return xs.length ? (xs.reduce((a, b) => a + b, 0) / xs.length) : 0
}
log(`3-arm: ${ok.length} stories — wins alone ${wins.alone}/engram ${wins.engram}/rich ${wins.rich}/tie ${wins.tie}; mean impl alone ${meanBy('alone').toFixed(2)} engram ${meanBy('engram').toFixed(2)} rich ${meanBy('rich').toFixed(2)}; mean rich−engram ${meanRich.toFixed(2)}`)
return { results: ok, wins, mean_rich_minus_engram: meanRich, mean_impl: { alone: meanBy('alone'), engram: meanBy('engram'), rich: meanBy('rich') } }
