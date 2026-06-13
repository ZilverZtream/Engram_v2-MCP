// Phase 2d — 2-arm A/B: Model+Engram(file list) vs Model+Engram(file list + APPROACH PRECEDENT).
//
// Tests whether appending "how similar changes were made before" (clean git diffs
// of the most similar historical commits) lifts the impl score over the plain file
// list — targeting the reasoning errors (wrong mechanism / over-engineering /
// misdiagnosis) that richer STRUCTURE could not fix. Both Opus agents implement from
// the SAME read-only base code; a judge scores both vs the real merged diff.
// US-only input: code-gen agents never see the PR / ground truth.
export const meta = {
  name: 'phase2-engram-pattern',
  description: 'Engram(file-list) vs Engram(file-list + approach precedent diffs) A/B vs the merged OciusX PR',
  phases: [
    { title: 'Implement', detail: 'two Opus agents propose an implementation per story (file-list / +approach-precedent)', model: 'opus' },
    { title: 'Judge', detail: 'Opus judge scores both arms vs the real merged diff', model: 'opus' },
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
          path: { type: 'string' },
          action: { type: 'string', enum: ['add', 'modify'] },
          change: { type: 'string', description: 'what changes here + the key code' },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['arms', 'winner', 'pattern_vs_engram', 'summary'],
  properties: {
    arms: {
      type: 'array',
      items: {
        type: 'object',
        required: ['arm', 'file_recall', 'impl_score', 'notes'],
        properties: {
          arm: { type: 'string', enum: ['engram', 'pattern'] },
          file_recall: { type: 'number' },
          impl_score: { type: 'integer', description: '1-5 vs the merged diff' },
          notes: { type: 'string' },
        },
      },
    },
    winner: { type: 'string', enum: ['engram', 'pattern', 'tie'] },
    pattern_vs_engram: { type: 'number', description: 'pattern impl_score minus engram impl_score' },
    summary: { type: 'string', description: 'one line: did the approach precedent help, hurt, or no-op — and did it anchor on a wrong mechanism?' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance criteria:\n${s.acceptance}`
  return t
}

const CODEGEN = (s, withPrecedent) => `You are a senior engineer implementing a user story in the OciusX codebase — a large legacy ASP.NET WebForms app (VB.NET, with TypeScript/JavaScript/jQuery and SQL). You are given ONLY the user story, exactly as a developer would be.

USER STORY:
${storyText(s)}

The codebase, as it existed BEFORE this story was implemented, is checked out READ-ONLY at:
  ${s.worktree}
Investigate ONLY this checkout — do NOT search other directories or repositories. Explore it with Read/Grep/Glob (and \`git -C ${s.worktree} ...\` if useful). Do NOT edit files — RETURN your implementation: every file you would add or modify, with the key code and what it does. If the worktree path does not exist or is empty, return an empty files list and say so.

Engram (a codebase-intelligence tool) has already analyzed this story. READ its dossier first:
  ${withPrecedent ? s.dossier_pattern_path : s.dossier_path}
It ranks the files most likely to need changing (concept-footprint + git co-change + structural-graph).${withPrecedent ? ` It ALSO appends "how similar changes were made before" — clean diffs of the most similar PAST commits. Use those precedents to choose the right MECHANISM and layering (e.g. which layers a change like this usually spans), but they are similar by file-overlap, not necessarily the same feature — adapt to THIS story, do not copy their scope.` : ''} Verify against the actual code; include files it missed and drop any that don't fit.

Be concrete: name real files that exist at ${s.worktree} (or new files in the right place), and show the actual code you'd write. Aim for the complete change including easy-to-forget companions (settings, resources, permissions, registrations, UI bindings).`

const JUDGE = (s) => `You are judging TWO implementation proposals for one user story against the REAL implementation a developer merged.

USER STORY:
${storyText(s)}

THE REAL MERGED IMPLEMENTATION (ground truth):
Real MODIFIED files (the fair retrieval target):
${(s.modified_files || []).map(f => '  - ' + f).join('\n')}

Real merged diff — READ it from this file (the gold standard for the CODE):
  ${s.merged_diff_path}

For EACH arm: file_recall (fraction of real MODIFIED files named; a page family foo.aspx/.aspx.vb counts if any member is named) and impl_score (1-5: does the proposed CODE achieve the same behavior/approach as the merged diff?).

ARM "engram" (Engram file-list dossier):
{{ENGRAM}}

ARM "pattern" (Engram dossier + approach-precedent diffs of similar past commits):
{{PATTERN}}

Return both arms' scores, the winner, pattern_vs_engram (pattern impl_score − engram impl_score), and a one-line summary: did the approach precedent help, no-op, or HURT (e.g. anchor the agent on a wrong mechanism from an unrelated past change)?`

phase('Implement')
const results = await pipeline(
  stories,
  (s) => parallel([
    () => agent(CODEGEN(s, false), { label: `impl:engram:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
    () => agent(CODEGEN(s, true), { label: `impl:pattern:pr${s.pr_id}`, phase: 'Implement', model: 'opus', schema: PROPOSAL_SCHEMA, agentType: 'claude' }),
  ]).then(([engram, pattern]) => ({ s, engram, pattern })),
  ({ s, engram, pattern }) => {
    const prompt = JUDGE(s)
      .replace('{{ENGRAM}}', JSON.stringify(engram, null, 2).slice(0, 12000))
      .replace('{{PATTERN}}', JSON.stringify(pattern, null, 2).slice(0, 12000))
    return agent(prompt, { label: `judge:pr${s.pr_id}`, phase: 'Judge', model: 'opus', schema: VERDICT_SCHEMA, agentType: 'claude' })
      .then((verdict) => ({ pr_id: s.pr_id, title: s.title, engram, pattern, verdict }))
  },
)

const ok = results.filter(Boolean)
const pd = ok.map(r => r.verdict?.pattern_vs_engram).filter(v => typeof v === 'number')
const meanP = pd.length ? pd.reduce((a, b) => a + b, 0) / pd.length : 0
const wins = { engram: 0, pattern: 0, tie: 0 }
ok.forEach(r => { if (r.verdict?.winner) wins[r.verdict.winner]++ })
const meanBy = (arm) => {
  const xs = ok.map(r => (r.verdict?.arms || []).find(a => a.arm === arm)?.impl_score).filter(v => typeof v === 'number')
  return xs.length ? (xs.reduce((a, b) => a + b, 0) / xs.length) : 0
}
log(`pattern A/B: ${ok.length} stories — wins engram ${wins.engram}/pattern ${wins.pattern}/tie ${wins.tie}; mean impl engram ${meanBy('engram').toFixed(2)} pattern ${meanBy('pattern').toFixed(2)}; mean pattern−engram ${meanP.toFixed(2)}`)
return { results: ok, wins, mean_pattern_minus_engram: meanP, mean_impl: { engram: meanBy('engram'), pattern: meanBy('pattern') } }
