# VB fallback declarations and index repair

The fallback extractor searched entire lines for `Function` or `Sub`, allowing
documentation, comments and string contents to become method symbols. A generic
fixture expected two methods and produced seven before the fix. A declaration
prefix gate removes the reproduced phantom methods and their misleading ranges.

Twenty-five focused checks passed, including modifiers, multiline signatures,
basic inline attributes, external/delegate declarations, generic methods and
escaped names. These are bounded fallback-parser checks, not VB grammar
completeness. The existing sidecar smoke can be a no-op when disabled.

A real MCP upgrade probe indexed unchanged synthetic source with the previous
binary, confirmed a phantom through graph listing and identity resolution,
reopened the same data with the candidate and explicitly re-extracted the file.
The phantom then resolved as not found; both real methods survived at the new
generation. The whole-file document remained valid. This small fixture did not
have a dedicated phantom document, so no standalone phantom-document deletion
claim is made.

The first upgrade attempt lacked the Git repository required by `update_project`.
Initializing a fixture-only repository corrected that setup prerequisite without
changing source. Failed attempts are retained. Old-binary fallback was forced by
a child-only source-size limit; the candidate used fallback because its adjacent
Roslyn sidecar was absent.

Existing affected indexes require re-extraction. `update_project.reindex_paths`
can refresh unchanged indexed paths and purge their old graph generations; no
automatic parser-version invalidation is claimed. The live reference-project
control already lacked the reported phantom name before deployment.

Planning-schema descriptions also now distinguish indexed merged work from
reviewer approval and a date cutoff from full historical source isolation.
Single-trial efficacy claims were removed from those parameter descriptions.

The optimized build deployed at 06:41 UTC on September 10. All 147 tools,
unrelated schemas, live health and unchanged configuration passed acceptance.
Real MCP caller-excerpt and synthetic Git-history regression probes also passed.
Binary SHA256: `ff5c8031a5f1f23b84fef887475c66a6b0c0e0150b348ff59c63e612c7f93da8`.

External evidence and rollback receipt:
`C:/ai-projects/audits/Engram/vb-comment-symbol-20260910/`.

Follow-up review reproduced two attribute defects: `>` inside quoted attribute
text hid valid methods, and `Function` inside attribute text supplied the wrong
method name. The gate now respects quoted/doubled-quote strings and takes the
method name after the actual declaration keyword. Twenty-eight focused checks
pass. A real MCP fixture confirms all four expected method identities, no
phantoms, and exact source retrieval. Caller/history probes pass again.

The follow-up deployed at 06:56 UTC with all 147 schemas unchanged, healthy live
acceptance and unchanged configuration. Current binary SHA256:
`45ad7be6e2e573d23c3a487e89f9da61e76c2743cd5dece37f22b123d20e3624`.
Versioned receipts are under `attribute-edge-review/`; the prior release evidence
remains intact. These checks do not establish full VB grammar support or measured
improvement in agent implementation quality.
