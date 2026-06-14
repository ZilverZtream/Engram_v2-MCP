// Parity audit — does Model+Engram reach DEVELOPER PARITY (merge-equivalent)?
//
// For each story, 3 INDEPENDENT strict reviewers judge the Engram-arm proposal
// against the REAL merged diff with a BINARY question: would a senior reviewer
// merge this as functionally equivalent to what shipped — same behavior, right
// files, no missing companions? Majority of 3 = parity for that story. This
// measures parity directly (binary, merge-ready) rather than via a 1-5 proxy.
export const meta = {
  name: 'engram-parity-audit',
  description: 'Independent 3-judge binary parity audit of Engram-arm proposals vs the merged OciusX PRs',
  phases: [{ title: 'Audit', detail: '3 strict reviewers per story vote merge-equivalent yes/no', model: 'opus' }],
}

let STORIES = null // INJECTED_STORIES
const stories = (STORIES && STORIES.length) ? STORIES : (Array.isArray(args) ? args : (args ? [args] : []))

const VERDICT = {
  type: 'object',
  required: ['parity', 'confidence', 'missing', 'reason'],
  properties: {
    parity: { type: 'boolean', description: 'true ONLY if a senior reviewer would merge this as functionally equivalent to the real PR (same behavior, all required files/companions, no material gaps). Default to false when unsure.' },
    confidence: { type: 'number', description: '0-1' },
    missing: { type: 'array', items: { type: 'string' }, description: 'material things the proposal lacks vs the merged diff (files, behavior, edge cases). Empty if none.' },
    reason: { type: 'string', description: 'one or two sentences' },
  },
}

function storyText(s) {
  let t = `Title: ${s.title}`
  if (s.description) t += `\n\nDescription:\n${s.description}`
  if (s.acceptance) t += `\n\nAcceptance:\n${s.acceptance}`
  return t
}

const JUDGE = (s, lens) => `You are a SENIOR engineer doing a strict merge review. Decide ONE thing: is the proposed implementation at DEVELOPER PARITY with the real merged PR — i.e., would you merge it as functionally equivalent to what the developer actually shipped?

USER STORY:
${storyText(s)}

THE REAL MERGED IMPLEMENTATION (gold standard):
Real MODIFIED files:
${(s.modified_files || []).map(f => '  - ' + f).join('\n')}
Real merged diff — READ it from this file:
  ${s.merged_diff_path}

THE PROPOSAL TO JUDGE (Model+Engram) — READ it from this file:
  ${s.engram_proposal_path}

Review lens to emphasize: ${lens}.

Parity = TRUE only if the proposal achieves the same behavior, touches the files that matter, and omits nothing material a reviewer would block on. A good-but-incomplete draft, a wrong mechanism, or missing companions (resx languages, SQL migration, permissions, the second layer of a change) = NOT parity. Be strict; default to false when unsure. Return your binary verdict, confidence, what's missing, and a one-line reason.`

phase('Audit')
const LENSES = ['behavioral equivalence (does it do the same thing)', 'completeness (all required files/companions present)', 'correctness & edge cases (would it pass review without changes)']
const results = await pipeline(
  stories,
  (s) => parallel(LENSES.map((lens, i) =>
    () => agent(JUDGE(s, lens), { label: `parity:pr${s.pr_id}:j${i + 1}`, phase: 'Audit', model: 'opus', schema: VERDICT, agentType: 'claude' })
  )).then((votes) => {
    const v = votes.filter(Boolean)
    const yes = v.filter(x => x.parity).length
    const parity = yes >= 2 // majority of 3
    return { pr_id: s.pr_id, title: s.title, votes_yes: yes, votes_total: v.length, parity, votes: v }
  }),
)

const ok = results.filter(Boolean)
const parityCount = ok.filter(r => r.parity).length
log(`PARITY AUDIT: ${parityCount}/${ok.length} stories at developer parity (majority of 3 strict reviewers). Per-PR: ${ok.map(r => r.pr_id + '=' + r.votes_yes + '/3' + (r.parity ? '✓' : '')).join(' ')}`)
return { parity_rate: `${parityCount}/${ok.length}`, parity_count: parityCount, total: ok.length, results: ok }
