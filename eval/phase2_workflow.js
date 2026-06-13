// Phase 2b — Model vs Model+Engram A/B on OciusX user stories.
//
// args = [{ pr_id, title, description, acceptance, worktree, dossier_path,
//           modified_files: [..], merged_diff: "..." }]
// Each story: two Opus agents propose an implementation from the SAME base-commit
// code (read-only); one gets the validated Engram dossier, one does not. A judge
// scores both against the REAL merged diff (passed in — no git needed). US-only
// input: code-gen agents never see the PR / ground truth; only the judge does.
export const meta = {
  name: 'phase2-engram-ab',
  description: 'Model-alone vs Model+Engram implementation A/B vs the merged OciusX PR, judged by Opus',
  phases: [
    { title: 'Implement', detail: 'two Opus agents propose an implementation per story (±Engram dossier)', model: 'opus' },
    { title: 'Judge', detail: 'Opus judge scores both arms vs the real merged diff', model: 'opus' },
  ],
}

// Story data: prefer baked-in STORIES (a per-run builder injects it, because the
// `args` global does not reliably reach scriptPath runs); fall back to `args`.
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
  required: ['arms', 'winner', 'engram_delta', 'summary'],
  properties: {
    arms: {
      type: 'array',
      items: {
        type: 'object',
        required: ['arm', 'file_recall', 'impl_score', 'notes'],
        properties: {
          arm: { type: 'string', enum: ['alone', 'engram'] },
          file_recall: { type: 'number', description: 'fraction of the real MODIFIED files this arm named (0-1)' },
          impl_score: { type: 'integer', description: '1-5: does the proposed code achieve the same behavior/approach as the merged diff?' },
          notes: { type: 'string' },
        },
      },
    },
    winner: { type: 'string', enum: ['alone', 'engram', 'tie'] },
    engram_delta: { type: 'number', description: 'engram impl_score minus alone impl_score' },
    summary: { type: 'string', description: 'one-line: did Engram help, and how?' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance criteria:\n${s.acceptance}`
  return t
}

const CODEGEN = (s, withEngram) => `You are a senior engineer implementing a user story in the OciusX codebase — a large legacy ASP.NET WebForms app (VB.NET, with TypeScript/JavaScript/jQuery and SQL). You are given ONLY the user story, exactly as a developer would be (sometimes just a headline).

USER STORY:
${storyText(s)}

The codebase, as it existed BEFORE this story was implemented, is checked out READ-ONLY at:
  ${s.worktree}
Investigate ONLY this checkout — do NOT search other directories or repositories on the machine. Explore it with Read/Grep/Glob (and \`git -C ${s.worktree} ...\` for history if useful). Do NOT edit files — instead RETURN your implementation: every file you would add or modify, with the key code (a snippet or unified diff) and what it does. If the worktree path does not exist or is empty, return an empty files list and say so — do NOT go hunting for a different task elsewhere.
${withEngram ? `
Engram (a codebase-intelligence tool) has already analyzed this story against the code and history. READ its dossier first — it ranks the files most likely to need changing, by concept-footprint + git co-change + structural-graph signals:
  ${s.dossier_path}
Use it to focus your investigation, but verify against the actual code; include any files it missed and drop any that don't fit.` : `
You do NOT have Engram. Use only standard tools (Read/Grep/Glob/git) to locate what to change.`}

Be concrete and realistic: name real files that exist at ${s.worktree} (or new files in the right place), and show the actual code you'd write. Aim for the complete change the story needs, including the easy-to-forget companions (settings, resources, permissions, registrations, UI bindings).`

const JUDGE = (s) => `You are judging two implementation proposals for one user story against the REAL implementation a developer merged.

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

ARM "engram" (with Engram dossier):
{{ENGRAM}}

Return both arms' scores, the winner, engram_delta (engram impl_score − alone impl_score), and a one-line summary of whether/how Engram helped.`

phase('Implement')
const results = await pipeline(
  stories,
  // Stage 1: two proposals in parallel (same base code, ±dossier).
  (s) => parallel([
    () => agent(CODEGEN({ ...s, worktree: s.worktree_alone || s.worktree }, false), { label: `impl:alone:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
    () => agent(CODEGEN(s, true), { label: `impl:engram:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
  ]).then(([alone, engram]) => ({ s, alone, engram })),
  // Stage 2: judge vs the real merged diff.
  ({ s, alone, engram }) => {
    const prompt = JUDGE(s)
      .replace('{{ALONE}}', JSON.stringify(alone, null, 2).slice(0, 12000))
      .replace('{{ENGRAM}}', JSON.stringify(engram, null, 2).slice(0, 12000))
    return agent(prompt, { label: `judge:pr${s.pr_id}`, phase: 'Judge', model: 'opus', schema: VERDICT_SCHEMA, agentType: 'claude' })
      .then((verdict) => ({ pr_id: s.pr_id, title: s.title, alone, engram, verdict }))
  },
)

const ok = results.filter(Boolean)
const deltas = ok.map(r => r.verdict?.engram_delta).filter(v => typeof v === 'number')
const meanDelta = deltas.length ? deltas.reduce((a, b) => a + b, 0) / deltas.length : 0
const wins = { alone: 0, engram: 0, tie: 0 }
ok.forEach(r => { if (r.verdict?.winner) wins[r.verdict.winner]++ })
log(`Phase 2: ${ok.length} stories — engram wins ${wins.engram}, alone ${wins.alone}, tie ${wins.tie}; mean impl_score delta ${meanDelta.toFixed(2)}`)
return { results: ok, wins, mean_engram_delta: meanDelta }
