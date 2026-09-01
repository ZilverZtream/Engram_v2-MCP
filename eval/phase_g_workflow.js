// Phase G (doc 13 item 7) — dossier vs dossier+ask_codebase-loop A/B on the
// fresh (r73) substrate. Two SONNET agents implement each story from the SAME
// frozen base snapshot (read-only; _snap_verify gates scoring): arm "dossier"
// gets the get_change_set dossier; arm "ask_loop" gets the dossier AND may
// query the live ask_codebase engine (eval/ask_eval.py) against the story's
// leak-free index. A Sonnet judge scores both against the real merged diff.
// Stories run SERIALLY (the ask wrapper serializes on the single-writer
// store); the two arms of one story run in parallel.
export const meta = {
  name: 'phase-g-ask-loop-ab',
  description: 'Dossier vs dossier+ask_codebase agent A/B vs merged OciusX PRs (doc 13 Phase G)',
  phases: [
    { title: 'Implement', detail: 'two Sonnet agents per story (dossier vs dossier+ask loop)', model: 'sonnet' },
    { title: 'Judge', detail: 'Sonnet judge scores both arms vs the real merged diff', model: 'sonnet' },
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
    asks: {
      type: 'array',
      description: 'ask_loop arm only: the ask_codebase questions you asked and what each contributed',
      items: {
        type: 'object',
        required: ['question', 'useful'],
        properties: {
          question: { type: 'string' },
          useful: { type: 'boolean', description: 'did the answer change your implementation?' },
          contribution: { type: 'string' },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['arms', 'winner', 'ask_delta', 'summary'],
  properties: {
    arms: {
      type: 'array',
      items: {
        type: 'object',
        required: ['arm', 'file_recall', 'impl_score', 'notes'],
        properties: {
          arm: { type: 'string', enum: ['dossier', 'ask_loop'] },
          file_recall: { type: 'number', description: 'fraction of the real MODIFIED files this arm named (0-1)' },
          impl_score: { type: 'integer', description: '1-5: does the proposed code achieve the same behavior/approach as the merged PR?' },
          notes: { type: 'string' },
        },
      },
    },
    winner: { type: 'string', enum: ['dossier', 'ask_loop', 'tie'] },
    ask_delta: { type: 'number', description: 'ask_loop impl_score minus dossier impl_score' },
    summary: { type: 'string', description: 'one line: did the ask loop help, and how?' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance criteria:\n${s.acceptance}`
  return t
}

function implPrompt(s, withAsk) {
  const tree = withAsk ? s.worktree : s.worktree_alone
  let p = `You are implementing a user story in a legacy VB.NET WebForms + TypeScript codebase (OciusX).

USER STORY
${storyText(s)}

THE CODE (read-only snapshot at the story's base commit): ${tree}
Explore with Read/Grep/Glob. Do NOT write or create files anywhere in that tree; propose changes instead.

A change-set dossier from the Engram code-intelligence tool (candidate files with signal tags and a completeness checklist — advisory, not a scope limit):
Read it at: ${s.dossier_path}

`
  if (withAsk) {
    p += `You may ALSO interrogate the codebase's knowledge engine (call graph, routes, tables, history) — up to 4 questions, each a Bash call (timeout 300000):
  cd C:/ai-projects/Engram-MCP_v2 && python eval/ask_eval.py ${s.pr_id} "<your question>"
Good questions: "Which server API functions does <file>.ts call?", "Which files call <api function>?", "Where is <symbol> defined?", "Which database tables does <api function> query?". Each answer lists evidence items with paths. Record every question in the "asks" field of your final output.

`
  }
  p += `Deliver via StructuredOutput: your approach and EVERY file you would create or modify (the real change usually spans server VB, client TS/JS, resx resources and sometimes SQL — enumerate the full surface you believe ships).`
  return p
}

function judgePrompt(s, dossierProp, askProp) {
  return `You are judging two implementation proposals for an OciusX user story against the REAL merged PR.

USER STORY
${storyText(s)}

THE REAL MERGED DIFF (ground truth): read ${s.merged_diff_path}
REAL MODIFIED FILES: ${JSON.stringify(s.modified_files)}

ARM "dossier" PROPOSAL:
${JSON.stringify(dossierProp)}

ARM "ask_loop" PROPOSAL:
${JSON.stringify(askProp)}

Score each arm: file_recall = fraction of the real modified files the arm named (path or clear page-family match); impl_score 1-5 = would the proposed code achieve the same behavior via a comparable approach (5 = merge-equivalent mechanism, 3 = right direction with gaps, 1 = wrong mechanism/scope). Be strict and identical in standards across arms. Then name the winner by impl_score (tie-break file_recall) and ask_delta = ask_loop impl - dossier impl.`
}

const results = []
for (const s of stories) {
  phase('Implement')
  log(`PR ${s.pr_id}: implementing (2 arms)`)
  const [dossierProp, askProp] = await parallel([
    () => agent(implPrompt(s, false), {
      label: `impl:${s.pr_id}:dossier`, phase: 'Implement', model: 'sonnet', schema: PROPOSAL_SCHEMA,
    }),
    () => agent(implPrompt(s, true), {
      label: `impl:${s.pr_id}:ask`, phase: 'Implement', model: 'sonnet', schema: PROPOSAL_SCHEMA,
    }),
  ])
  if (!dossierProp || !askProp) {
    results.push({ pr: s.pr_id, error: 'an implement arm died' })
    continue
  }
  const verdict = await agent(judgePrompt(s, dossierProp, askProp), {
    label: `judge:${s.pr_id}`, phase: 'Judge', model: 'sonnet', schema: VERDICT_SCHEMA,
  })
  results.push({ pr: s.pr_id, verdict, asks: askProp.asks || [] })
  log(`PR ${s.pr_id}: ${verdict ? verdict.summary : 'judge died'}`)
}

const ok = results.filter(r => r.verdict)
const mean = a => a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0
const dossierMean = mean(ok.map(r => r.verdict.arms.find(a => a.arm === 'dossier').impl_score))
const askMean = mean(ok.map(r => r.verdict.arms.find(a => a.arm === 'ask_loop').impl_score))
return {
  n: ok.length,
  mean_dossier: dossierMean,
  mean_ask_loop: askMean,
  mean_ask_delta: mean(ok.map(r => r.verdict.ask_delta)),
  wins: {
    ask_loop: ok.filter(r => r.verdict.winner === 'ask_loop').length,
    dossier: ok.filter(r => r.verdict.winner === 'dossier').length,
    tie: ok.filter(r => r.verdict.winner === 'tie').length,
  },
  verdicts: results,
}
