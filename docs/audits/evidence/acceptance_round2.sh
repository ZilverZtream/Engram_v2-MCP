#!/bin/bash
# ROUND-2 ACCEPTANCE RUN (docs/audits/10, P1-3): ONE clean, self-contained, reproducible run against the
# live daemon on OciusX. Every probe is embedded here (no scratchpad helpers, no dummy variables); the
# outputs are written ONCE to docs/audits/evidence/acceptance_round2_<stamp>.{log,results.txt} and made
# read-only — no appended corrections. Engram data only: the OciusX repository is never written.
#   usage: bash docs/audits/evidence/acceptance_round2.sh
set -u
REPO="C:/ai-projects/Engram-MCP_v2"
EV="$REPO/docs/audits/evidence"
P=5a35e8e0-d37a-41b3-a250-a26957e7aedb
OX="C:/Users/Dennis/source/repos/OciusX"
BIN="$LOCALAPPDATA/engram/bin/engram_server.exe"
STAMP=$(date +%Y%m%d_%H%M%S)
LOG="$EV/acceptance_round2_$STAMP.log"
R="$EV/acceptance_round2_$STAMP.results.txt"
WORK=$(mktemp -d)
export PYTHONIOENCODING=utf-8
cd "$REPO" || exit 1
exec > >(tee "$LOG") 2>&1
: > "$R"
verdict() { printf "%-52s %-5s %s\n" "$1" "$2" "$3" | tee -a "$R"; }
T() { python tools/engram_drive.py tool "$1" "$2" "${3:-60000}" 2>/dev/null; }
ms_since() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

echo "=== ROUND-2 ACCEPTANCE $(date +%F_%T)"
echo "repo commit: $(git rev-parse --short HEAD) ($(git log -1 --format=%cd --date=iso | cut -c1-19)) | branch $(git rev-parse --abbrev-ref HEAD) | dirty files: $(git status --short | wc -l | tr -d ' ')"
echo "binary: $(ls -la --time-style=+%F_%T "$BIN" | awk '{print $6, $5" bytes"}') | sha256 $(sha256sum "$BIN" | cut -c1-16)…"
echo "corpus: eval/data/ask_golden_ociusx.jsonl ($(wc -l < eval/data/ask_golden_ociusx.jsonl | tr -d ' ') rows, sha256 $(sha256sum eval/data/ask_golden_ociusx.jsonl | cut -c1-16)…)"

echo; echo "== restart the daemon ONCE (same binary) so the first change-set call is a true first call"
powershell -NoProfile -Command "Get-Process engram_server -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Milliseconds 800; (Get-Process engram_server -ErrorAction SilentlyContinue | Measure-Object).Count" | tr -d '\r'
python tools/engram_drive.py list 2>/dev/null | head -1 | cut -c1-80; sleep 75

echo; echo "== P0-3 reference story: FIRST call after restart — critical files PRIMARY within top 30, primary <= 40, <= 5 s"
REF="As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category so that time reports roll up to it"
T0=$(date +%s%N); T get_change_set "{\"project_id\":\"$P\",\"story\":\"$REF\",\"output_json\":true}" 900000 > "$WORK/cs_ref.json"; SECS=$(( $(ms_since $T0) / 1000 ))
python - "$WORK/cs_ref.json" "$SECS" "$R" <<'PY'
import json,io,sys
t=io.open(sys.argv[1],encoding="utf-8").read(); v=json.loads(t[t.index("{"):])
fs=v.get("files",[]); paths=[f.get("path","").lower() for f in fs]; prim=[f for f in fs if f.get("set")=="primary"]
must=["productioncodelistmaincategory.aspx","productioncodelistmaincategory.aspx.vb","rk_redovisningskategorier.sql","redovisningskategorier.vb","api-redovisning.vb","ifalt.dbml"]
rows=[]
for m in must:
    pos=next((i for i,p in enumerate(paths) if p.endswith(m)),None); f=fs[pos] if pos is not None else {}
    rows.append((m,pos,f.get("set"),f.get("tier")))
ok_rank=all(p is not None and p<30 and s=="primary" for _,p,s,_ in rows); ok_cap=1<=len(prim)<=40
print("  ",[(m,p,s,t) for m,p,s,t in rows]); print(f"   candidates {len(fs)} | primary {len(prim)} | secs {sys.argv[2]}")
w=io.open(sys.argv[3],"a",encoding="utf-8")
w.write(f"{'P0-3 critical files PRIMARY within top 30':<52} {'PASS' if ok_rank else 'FAIL'}  positions {[p for _,p,_,_ in rows]}\n")
w.write(f"{'P0-3 primary set <= 40 candidates':<52} {'PASS' if ok_cap else 'FAIL'}  primary {len(prim)} of {len(fs)}\n")
w.write(f"{'P0-3 first call after restart <= 5 s':<52} {'PASS' if int(sys.argv[2])<=5 else 'FAIL'}  {sys.argv[2]} s\n")
PY
tail -n 3 "$R"

echo; echo "== P0-2 health / freshness: path-set completeness per store, no percentage"
H=$(T project_health "{\"project_id\":\"$P\"}" 120000); echo "$H" | grep -n "^Health\|generation completeness\|missing sample\|active_generation" | cut -c1-200
echo "$H" | grep -q "^Health: OK" && echo "$H" | grep -q "generation completeness.*— complete" && ! echo "$H" | grep -q "[0-9] %" && verdict "P0-2 health: path sets per store, complete, no %" PASS "$(echo "$H" | grep -o "expected paths: [0-9]*; tantivy: [0-9]*, vectors: [0-9]*, graph: [0-9]*" | head -1)" || verdict "P0-2 health: path sets per store, complete, no %" FAIL "$(echo "$H" | grep "^Health\|completeness" | head -2 | tr '\n' ' ' | cut -c1-120)"
F=$(T get_index_freshness "{\"project_id\":\"$P\",\"check_disk\":false}" 60000); echo "$F" | grep -n "generation_complete" | cut -c1-200
echo "$F" | grep -q "generation_complete: true" && verdict "P0-2 freshness generation_complete (path sets)" PASS "$(echo "$F" | grep -o "generation_complete: true ([^)]*)" | head -1 | cut -c1-100)" || verdict "P0-2 freshness generation_complete (path sets)" FAIL "$(echo "$F" | grep generation_complete | head -1 | cut -c1-100)"

echo; echo "== P0-1 update_project (wait): purge reported, completeness survives, stores agree"
T0=$(date +%s); U=$(T update_project "{\"project_id\":\"$P\",\"wait\":true}" 900000); echo "  update secs: $(( $(date +%s) - T0 ))"; echo "$U" | grep -n "active_generation\|purge:" | head -3 | cut -c1-160
echo "$U" | grep -q "purge: ok" && verdict "P0-1 update reports the post-publish purge" PASS "$(echo "$U" | grep -o "purge: [^\n]*" | head -1 | cut -c1-80)" || verdict "P0-1 update reports the post-publish purge" FAIL "$(echo "$U" | grep -o "purge: [^\n]*" | head -1 | cut -c1-100)"
H2=$(T project_health "{\"project_id\":\"$P\"}" 120000)
echo "$H2" | grep -q "^Health: OK" && echo "$H2" | grep -q "generation completeness.*— complete" && verdict "P0-1 completeness survives an update" PASS "$(echo "$H2" | grep -o "active_generation: [0-9]*" | head -1)" || verdict "P0-1 completeness survives an update" FAIL "$(echo "$H2" | grep "^Health\|completeness" | head -2 | tr '\n' ' ' | cut -c1-120)"
CI=$(T check_integrity "{\"project_id\":\"$P\"}" 120000); TC=$(echo "$CI" | grep -o "\"tantivy_doc_count\": *[0-9]*" | grep -o "[0-9]*$"); VC=$(echo "$CI" | grep -o "\"vector_doc_count\": *[0-9]*" | grep -o "[0-9]*$"); OH=$(echo "$CI" | grep -o "\"overall_healthy\": *[a-z]*" | grep -o "[a-z]*$")
[ -n "$TC" ] && [ "$TC" = "$VC" ] && [ "$OH" = "true" ] && verdict "P0-1 Tantivy == LanceDB after update + GC" PASS "tantivy $TC = vectors $VC, healthy" || verdict "P0-1 Tantivy == LanceDB after update + GC" FAIL "tantivy ${TC:-?} vs vectors ${VC:-?}, healthy=${OH:-?}"

echo; echo "== P0-4 golden suite with the CORRECTNESS gate (required modality / symbols / distractors / precision)"
python eval/ask_engine_golden.py "$P" eval/data/ask_golden_ociusx.jsonl --out "$EV/golden_round2_$STAMP.json" > "$WORK/golden.log" 2>&1; grep -E "status-match|abstain:|correct:|  FAIL |GATE" "$WORK/golden.log" | cut -c1-160
grep -q "GATE: PASS" "$WORK/golden.log" && verdict "P0-4 golden: every row CORRECT (not just status)" PASS "$(grep "correct:" "$WORK/golden.log" | cut -c1-30)" || verdict "P0-4 golden: every row CORRECT (not just status)" FAIL "$(grep "correct:" "$WORK/golden.log" | cut -c1-30); $(grep -c '  FAIL ' "$WORK/golden.log") rows"

echo; echo "== P1-1 pre_commit_review (diff=head): gates run; a missing backing document would DEGRADE with an integrity note"
T pre_commit_review "{\"project_id\":\"$P\",\"diff\":\"head\",\"output_json\":true}" 2000000 > "$WORK/pcr.json"
python - "$WORK/pcr.json" "$R" <<'PY'
import json,io,sys
t=io.open(sys.argv[1],encoding="utf-8",errors="replace").read()
try: v=json.loads(t[t.index("{"):])
except Exception as e:
    io.open(sys.argv[2],"a",encoding="utf-8").write(f"{'P1-1 pre-commit gates run, integrity-aware':<52} FAIL  parse: {e}\n"); raise SystemExit
outs=v.get("gate_status") or v.get("gate_outcomes") or v.get("gates") or []
deg=[o.get("name") for o in outs if str((o.get("status") or {}).get("kind","")).lower() in ("degraded","error","failed")]
integ=t.lower().count("integrity failure")
ok=len(outs)>=19 and not deg
line=f"{'P1-1 pre-commit gates run, integrity-aware':<52} {'PASS' if ok else 'FAIL'}  {len(outs)} gates, degraded={deg}, integrity notes={integ}, verdict={v.get('verdict')}\n"
print(" ",line.rstrip()); io.open(sys.argv[2],"a",encoding="utf-8").write(line)
PY

echo; echo "== P1-2 co-change: served from the warm snapshot, no git walk at call time"
for i in 1 2; do T0=$(date +%s%N); FS=$(T find_similar_changes "{\"project_id\":\"$P\",\"files\":[\"Site/App_Code/redovisning/code/redovisningskategorier.vb\"],\"max_commits\":800,\"top\":5}" 120000); MS=$(ms_since $T0); LINE=$(echo "$FS" | grep -o "co-change snapshot: [^)]*)" | head -1); echo "  call $i: $MS ms | ${LINE:-no coverage line}"; done
echo "$LINE" | grep -q "warm (served without a git walk" && [ "$MS" -le 1500 ] && verdict "P1-2 warm snapshot, no call-time git walk" PASS "$MS ms; $LINE" || verdict "P1-2 warm snapshot, no call-time git walk" FAIL "$MS ms; ${LINE:-no coverage line}"

echo; echo "== P1-4 Dream default OFF (opt-in with include_insights: true)"
Q="How does marker clustering work on the map?"
A0=$(T ask_codebase "{\"project_id\":\"$P\",\"question\":\"$Q\",\"output_format\":\"json\",\"depth\":\"standard\"}" 400000 | grep -c "\"provider\": *\"insight")
A1=$(T ask_codebase "{\"project_id\":\"$P\",\"question\":\"$Q\",\"output_format\":\"json\",\"depth\":\"standard\",\"include_insights\":true}" 400000 | grep -c "\"provider\": *\"insight")
echo "  insight providers: default=$A0 opt-in=$A1"
[ "$A0" = "0" ] && [ "$A1" -ge 1 ] && verdict "P1-4 Dream arm off by default, on when asked" PASS "default $A0, opt-in $A1" || verdict "P1-4 Dream arm off by default, on when asked" FAIL "default $A0, opt-in $A1"

echo; echo "== Round-1 rows the auditor re-verified"
EN="As a project manager I want the reporting of quantities to show the change requests per fiber installation plan so that invoicing matches the field work"
T0=$(date +%s); C=$(T get_change_set "{\"project_id\":\"$P\",\"story\":\"$EN\"}" 900000 | grep "^concepts:" | head -1 | cut -c1-140); SE=$(( $(date +%s) - T0 ))
echo "$C" | grep -qi "redovisning" && [ "$SE" -le 8 ] && verdict "Row 1 EN story -> SV concepts (<= 8 s)" PASS "$SE s; $C" || verdict "Row 1 EN story -> SV concepts (<= 8 s)" FAIL "$SE s; $C"
python eval/edit_context_parity.py "$P" > "$WORK/parity.log" 2>&1; tail -n 2 "$WORK/parity.log" | cut -c1-120
grep -qE "20/20|all gates pass|PASS" "$WORK/parity.log" && ! grep -qE "FAIL" "$WORK/parity.log" && verdict "Row 2 edit-context parity" PASS "$(grep -oE '[0-9]+/[0-9]+' "$WORK/parity.log" | tail -1)" || verdict "Row 2 edit-context parity" FAIL "$(tail -n 1 "$WORK/parity.log" | cut -c1-80)"
echo "  Row 4 G1 literal completeness (graph+text >= git grep, <= 2 s each):"; BAD=""; SLOW=""
for c in redovisningskategori installationsobjekt arbetslag personalliggare tidrapport; do
  T0=$(date +%s); out=$(T get_concept_footprint "{\"project_id\":\"$P\",\"concept\":\"$c\",\"max_per_group\":400}" 400000); s=$(( $(date +%s) - T0 ))
  g=$(echo "$out" | grep -o "graph touchpoints: [0-9]*" | head -1 | grep -o "[0-9]*$"); tx=$(echo "$out" | grep -o "^## Mentioned only in text — [0-9]*" | grep -o "[0-9]*$")
  gg=$(git -C "$OX" grep -il "$c" -- '*.vb' '*.aspx' '*.ascx' '*.master' '*.sql' '*.dbml' '*.js' '*.ts' '*.cs' '*.config' '*.ml' '*.mlinc' 2>/dev/null | wc -l | tr -d ' ')
  printf "    %-22s graph %4s text %4s gitgrep %4s  %ss\n" "$c" "${g:-?}" "${tx:-?}" "$gg" "$s"
  [ $(( ${g:-0} + ${tx:-0} )) -lt "$gg" ] && BAD="$BAD $c"; [ "$s" -gt 2 ] && SLOW="$SLOW $c"
done
[ -z "$BAD" ] && [ -z "$SLOW" ] && verdict "Row 4 G1 literal completeness (5 concepts)" PASS "none short, none slow" || verdict "Row 4 G1 literal completeness (5 concepts)" FAIL "short:$BAD slow:$SLOW"
T0=$(date +%s%N); HS=$(T get_page_context "{\"project_id\":\"$P\",\"aspx_file\":\"Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx\"}" 60000); MS=$(ms_since $T0); NS=$(echo "$HS" | grep -c "^- \*\*Sibling\*\*")
[ "$NS" -ge 1 ] && [ "$MS" -le 3000 ] && verdict "Row 5 house_style section (siblings, <= 3 s)" PASS "$NS siblings, $MS ms" || verdict "Row 5 house_style section (siblings, <= 3 s)" FAIL "$NS siblings, $MS ms"
python - "$P" "$R" <<'PY'
import json,subprocess,sys
pid=sys.argv[1]; page="Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx"
diff=(f"diff --git a/{page} b/{page}\n--- a/{page}\n+++ b/{page}\n@@ -360,0 +361,2 @@\n+<asp:Panel ID=\"panProbe\" runat=\"server\" CssClass=\"alert alert-primary\">\n+</asp:Panel>\n")
out=subprocess.run([sys.executable,"tools/engram_drive.py","tool","pre_commit_review",json.dumps({"project_id":pid,"diff":diff}),"180000"],capture_output=True,text=True,encoding="utf-8").stdout
n=sum(1 for l in out.splitlines() if "alert-primary" in l and "no sibling" in l)
open(sys.argv[2],"a",encoding="utf-8").write(f"{'Row 5 ui_house_style gate (live probe)':<52} {'PASS' if n>=1 else 'FAIL'}  {n} finding(s) naming alert-primary\n"); print("  gate findings naming alert-primary:",n)
PY
UC=$(T get_ui_conformance "{\"project_id\":\"$P\",\"region\":\"Site/modules/dashboard/pages/admin/production/\",\"min_instances\":2}" 60000)
! echo "$UC" | grep -qi "TOOL ERROR" && echo "$UC" | grep -qi "famil" && verdict "Row 5 get_ui_conformance (region pull)" PASS "$(echo "$UC" | grep -io "[0-9]* famil[a-z]*" | head -1)" || verdict "Row 5 get_ui_conformance (region pull)" FAIL "$(echo "$UC" | head -1 | cut -c1-80)"
TD=$(T trace_data_flow "{\"project_id\":\"$P\",\"file_path\":\"Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx\",\"entry_point\":\"btnSave_Click\"}" 60000)
echo "$TD" | grep -qi "TOOL ERROR\|failure:" && verdict "Row 7 trace_data_flow smoke" FAIL "$(echo "$TD" | head -1 | cut -c1-80)" || verdict "Row 7 trace_data_flow smoke" PASS "$(echo "$TD" | head -1 | cut -c1-80)"
python - "$P" "$R" <<'PY'
import json,subprocess,sys
pid=sys.argv[1]; RULE="audit-round2-forbid-eval-probe"
def tool(name,payload,cap="400000"):
    return subprocess.run([sys.executable,"tools/engram_drive.py","tool",name,json.dumps(payload),cap],capture_output=True,text=True,encoding="utf-8").stdout
def diff_with(line):
    return ("diff --git a/Site/js/audit_probe.js b/Site/js/audit_probe.js\n--- a/Site/js/audit_probe.js\n+++ b/Site/js/audit_probe.js\n@@ -1,3 +1,4 @@\n function render(data) {\n+"+line+"\n   return data;\n }\n")
def review(diff):
    out=tool("pre_commit_review",{"project_id":pid,"diff":diff,"output_json":True,"min_severity":"info"})
    try: return json.loads(out[out.index("{"):])
    except Exception: return {}
def rr(rep): return [f for f in (rep.get("findings") or rep.get("items") or []) if (f.get("gate") or f.get("category") or "")=="repo_rules"]
tool("add_repo_rule",{"project_id":pid,"file_pattern":"*.js","priority":90,"rule_id":RULE,"rule_text":"Never evaluate strings as code in client scripts; use JSON.parse or a lookup table. [check: forbid=eval\\(]"})
try:
    bad=rr(review(diff_with("  var v = eval(data.expr);"))); good=rr(review(diff_with("  var v = JSON.parse(data.expr);")))
    ok=len(bad)>=1 and any((f.get("severity") or "").lower()=="critical" for f in bad) and not good
    open(sys.argv[2],"a",encoding="utf-8").write(f"{'Row 8 enforced rule -> Critical; clean -> none':<52} {'PASS' if ok else 'FAIL'}  violating {[f.get('severity') for f in bad]}, clean {len(good)}\n")
    print("  row 8:", "PASS" if ok else "FAIL", [f.get("severity") for f in bad], len(good))
finally:
    tool("delete_repo_rule",{"project_id":pid,"rule_id":RULE})
PY
T0=$(date +%s%N); DI=$(T detect_incomplete_changes "{\"project_id\":\"$P\",\"edited_files\":[\"Site/App_Code/redovisning/code/redovisningskategorier.vb\"],\"max_partners\":10}" 60000); MS=$(ms_since $T0)
! echo "$FS$DI" | grep -qi "TOOL ERROR" && [ "$MS" -le 3000 ] && verdict "Row 9 similar + incomplete changes (<= 3 s)" PASS "$MS ms" || verdict "Row 9 similar + incomplete changes (<= 3 s)" FAIL "$MS ms"
FR=$(T find_symbol_references "{\"project_id\":\"$P\",\"symbol_name\":\"Check_pr_id\",\"max_incoming\":5000}" 60000); IA=$(T impact_analysis "{\"project_id\":\"$P\",\"symbol_fqn\":\"_us.accessctrl.Check_pr_id\",\"limit\":5000}" 60000)
NFR=$(echo "$FR" | grep -o "[0-9]* distinct caller" | head -1 | grep -o "^[0-9]*"); NIA=$(echo "$IA" | grep -o "dependents\*\*: [0-9]*" | head -1 | grep -o "[0-9]*$")
! echo "$FR$IA" | grep -qi "ceiling reached\|failures: [1-9]\|TOOL ERROR" && [ -n "$NFR" ] && [ "$NFR" = "$NIA" ] && verdict "Row 10 caller parity (refs == impact)" PASS "$NFR callers" || verdict "Row 10 caller parity (refs == impact)" FAIL "refs ${NFR:-?} vs impact ${NIA:-?}"
NT=$(python tools/engram_drive.py tools 2>/dev/null | grep -c "^[a-z_]*$")
[ "$NT" -le 32 ] && [ "$NT" -ge 20 ] && verdict "Tool surface: <= 32 core advertised" PASS "$NT advertised" || verdict "Tool surface: <= 32 core advertised" FAIL "$NT advertised"
PC=$(T produce_claude_md "{\"project_id\":\"$P\",\"merge_existing\":false}" 400000)
echo "$PC" | grep -q "edited_files=" && ! echo "$PC" | grep -q "(files=" && verdict "produce_claude_md contract (edited_files=, dry render)" PASS "$(echo "$PC" | grep -c "edited_files=") mention(s)" || verdict "produce_claude_md contract (edited_files=, dry render)" FAIL "edited_files= $(echo "$PC" | grep -c "edited_files="), (files= $(echo "$PC" | grep -c "(files=")"

echo; echo "=== SUMMARY $(date +%F_%T)"; cat "$R"; NP=$(grep -c ' PASS ' "$R"); NF=$(grep -c ' FAIL ' "$R"); echo "PASS: $NP | FAIL: $NF"
echo "results sha256: $(sha256sum "$R" | cut -c1-64)"
rm -rf "$WORK"
attrib +R "$(cygpath -w "$R")" >/dev/null 2>&1; echo "immutable: $R"; echo "log: $LOG (made read-only after this line)"
( sleep 1; attrib +R "$(cygpath -w "$LOG")" >/dev/null 2>&1 ) &
[ "$NF" = "0" ]
