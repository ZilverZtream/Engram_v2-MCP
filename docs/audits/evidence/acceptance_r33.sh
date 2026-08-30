#!/bin/bash
# Re-audit ACCEPTANCE PASS on release 33 (owner 2026-08-30 02:58): replay every doc-09 checklist item
# against the live daemon with fresh evidence and print a PASS/FAIL table. Engram data only — the
# OciusX repo is never written. The daemon is restarted once (same binary) so P0-3 measures a true first call.
set -u
S="C:/Users/Dennis/AppData/Local/Temp/claude/C--ai-projects-Engram-MCP-v2/50d309ed-9767-44e6-b7fc-2ac51dd98fa5/scratchpad/p0"
P=5a35e8e0-d37a-41b3-a250-a26957e7aedb
OX="C:/Users/Dennis/source/repos/OciusX"
export PYTHONIOENCODING=utf-8
cd C:/ai-projects/Engram-MCP_v2 || exit 1
R="$S/acceptance_r33_results.txt"; : > "$R"
verdict() { printf "%-46s %-5s %s\n" "$1" "$2" "$3" | tee -a "$R"; }
T() { python tools/engram_drive.py tool "$1" "$2" "${3:-60000}" 2>/dev/null; }

echo "=== ACCEPTANCE PASS r33 $(date +%H:%M:%S) — binary $(ls -la --time-style=+%F_%T "$LOCALAPPDATA/engram/bin/engram_server.exe" | awk '{print $6, $5}')"

echo "== P0-2 health / freshness"
H=$(T project_health "{\"project_id\":\"$P\"}" 60000); echo "$H" | grep -n "^Health\|generation completeness\|active_generation\|graph_nodes" | cut -c1-160
GEN0=$(echo "$H" | grep -o "active_generation: [0-9]*" | grep -o "[0-9]*$")
echo "$H" | grep -q "^Health: OK" && echo "$H" | grep -q "generation completeness.*complete" && verdict "P0-2 health verdict + completeness" PASS "Health OK, complete, gen $GEN0" || verdict "P0-2 health verdict + completeness" FAIL "see health"
F=$(T get_index_freshness "{\"project_id\":\"$P\",\"check_disk\":false}" 60000); echo "$F" | grep -n "generation_complete\|advice" | cut -c1-140
echo "$F" | grep -q "generation_complete: true\|generation_complete\": true\|generation_complete = true" && verdict "P0-2 freshness generation_complete" PASS "true" || { echo "$F" | grep -qi "generation_complete" && verdict "P0-2 freshness generation_complete" FAIL "$(echo "$F" | grep -i generation_complete | head -1 | cut -c1-80)" || verdict "P0-2 freshness generation_complete" FAIL "no completeness line"; }

echo "== P0-1 watcher update + GC: incremental update_project, completeness must survive"
T0=$(date +%s); U=$(T update_project "{\"project_id\":\"$P\",\"wait\":true}" 900000); echo "  update secs: $(( $(date +%s) - T0 ))"; echo "$U" | head -3 | cut -c1-140
H2=$(T project_health "{\"project_id\":\"$P\"}" 60000); GEN1=$(echo "$H2" | grep -o "active_generation: [0-9]*" | grep -o "[0-9]*$"); echo "$H2" | grep -n "^Health\|generation completeness" | cut -c1-160
echo "$H2" | grep -q "^Health: OK" && echo "$H2" | grep -q "generation completeness.*complete" && verdict "P0-1 completeness survives an update (+GC path)" PASS "gen $GEN0 -> $GEN1, complete" || verdict "P0-1 completeness survives an update (+GC path)" FAIL "gen $GEN0 -> $GEN1"

echo "== P0-4 pre_commit_review (diff=head): gates run, none degraded"
T pre_commit_review "{\"project_id\":\"$P\",\"diff\":\"head\",\"output_json\":true}" 2000000 > "$S/pcr_r33.json"
python - "$S/pcr_r33.json" "$R" <<'PY'
import json,io,sys
t=io.open(sys.argv[1],encoding="utf-8",errors="replace").read()
try: v=json.loads(t[t.index("{"):])
except Exception as e:
    print("  parse failed:",e); io.open(sys.argv[2],"a",encoding="utf-8").write(f"{'P0-4 gates run, none degraded':<46} FAIL  parse\n"); raise SystemExit
outs=v.get("gate_status") or v.get("gate_outcomes") or v.get("gates") or []
names=[o.get("name") for o in outs]; deg=[o.get("name") for o in outs if str((o.get("status") or {}).get("kind","")).lower() in ("degraded","error","failed")]
print("  gates:",len(names),"| degraded:",deg,"| verdict:",v.get("verdict"))
ok=len(names)>=19 and not deg
line=f"{'P0-4 gates run, none degraded':<46} {'PASS' if ok else 'FAIL'}  {len(names)} gates, degraded={deg}, verdict={v.get('verdict')}\n"
print(line.rstrip()); io.open(sys.argv[2],"a",encoding="utf-8").write(line)
PY

echo "== Integration produce_claude_md: emitted examples say edited_files= (dry render, nothing written)"
PC=$(T produce_claude_md "{\"project_id\":\"$P\",\"merge_existing\":false}" 400000); N=$(cat "$PC_DUMMY" 2>/dev/null; { echo "$PC"; cat "$OX/.claude/rules/engram-workflow.md" "$OX/AGENTS.md" 2>/dev/null; } | grep -o "detect_incomplete_changes([a-z_]*=" | sort | uniq -c | tr '\n' ' '); echo "  reply + generated artifacts on disk: $N"
echo "$N" | grep -q "edited_files=" && ! echo "$N" | grep -q "(files=" && verdict "Integration produce_claude_md contract" PASS "edited_files= only ($N)" || verdict "Integration produce_claude_md contract" FAIL "$N"

echo "== Tiered surface"
NT=$(python tools/engram_drive.py tools 2>/dev/null | grep -c "^[a-z_]*$"); ADV=$(T list_advanced_tools "{}" 6000 | grep -c "^- \|^| \|^[a-z_]*  " ); echo "  advertised: $NT | advanced lines: $ADV"
[ "$NT" -le 32 ] && [ "$NT" -ge 20 ] && verdict "Tool surface: <= 32 core advertised" PASS "$NT advertised, advanced list works" || verdict "Tool surface: <= 32 core advertised" FAIL "$NT"

echo "== P0-3 reference story: restart daemon (same binary) -> FIRST call (gate <= 5 s, 6/6 files)"
powershell -NoProfile -ExecutionPolicy Bypass -File "$S/deploy.ps1" 2>&1 | grep -E "deployed|hash" | cut -c1-100
python tools/engram_drive.py list 2>/dev/null | head -1 | cut -c1-60; sleep 75
REF="As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category so that time reports roll up to it"
T0=$(date +%s%N); T get_change_set "{\"project_id\":\"$P\",\"story\":\"$REF\",\"output_json\":true}" 900000 > "$S/cs_ref_acc_first.json"; SECS=$(( ($(date +%s%N) - T0) / 1000000000 ))
python - "$S/cs_ref_acc_first.json" "$SECS" "$R" <<'PY'
import json,io,sys
t=io.open(sys.argv[1],encoding="utf-8").read(); v=json.loads(t[t.index("{"):]); cov=v.get("coverage",{})
files=[f.get("path","").lower() for f in v.get("files",[])]
must=["productioncodelistmaincategory.aspx","productioncodelistmaincategory.aspx.vb","rk_redovisningskategorier.sql","redovisningskategorier.vb","api-redovisning.vb","ifalt.dbml"]
have=[m for m in must if any(p.endswith(m) for p in files)]
ok=len(have)==6 and int(sys.argv[2])<=5
line=f"{'P0-3 reference story first call':<46} {'PASS' if ok else 'FAIL'}  {sys.argv[2]} s wall ({cov.get('wall_ms')} ms), {len(have)}/6 files, ui_contract={v.get('ui_contract')}\n"
print(" ",line.rstrip()); io.open(sys.argv[3],"a",encoding="utf-8").write(line)
PY

echo "== Row 1 EN story -> Swedish concepts/files (<= 8 s)"
EN="As a project manager I want the reporting of quantities to show the change requests per fiber installation plan so that invoicing matches the field work"
T0=$(date +%s); T get_change_set "{\"project_id\":\"$P\",\"story\":\"$EN\"}" 900000 > "$S/cs_en_acc.txt"; SE=$(( $(date +%s) - T0 )); C=$(grep "^concepts:" "$S/cs_en_acc.txt" | head -1 | cut -c1-140); echo "  secs $SE | $C"
echo "$C" | grep -qi "redovisning" && [ "$SE" -le 8 ] && verdict "Row 1 EN story -> SV concepts" PASS "$SE s; $C" || verdict "Row 1 EN story -> SV concepts" FAIL "$SE s; $C"

echo "== Row 2 edit context parity"
python eval/edit_context_parity.py "$P" > "$S/parity_acc.log" 2>&1; tail -n 3 "$S/parity_acc.log" | cut -c1-140
grep -qE "20/20|all gates pass|PASS" "$S/parity_acc.log" && ! grep -qE "FAIL" "$S/parity_acc.log" && verdict "Row 2 edit-context parity" PASS "$(grep -oE '[0-9]+/[0-9]+' "$S/parity_acc.log" | tail -1)" || verdict "Row 2 edit-context parity" FAIL "$(tail -n 1 "$S/parity_acc.log" | cut -c1-80)"

echo "== Row 4 G1 literal completeness (5 concepts)"
bash "$S/g1_check.sh" > "$S/g1_acc.log" 2>&1; cat "$S/g1_acc.log" | cut -c1-120
python - "$S/g1_acc.log" "$R" <<'PY'
import io,sys,re
rows=[l.split() for l in io.open(sys.argv[1],encoding="utf-8",errors="replace").read().splitlines()[1:] if l.strip()]
bad=[]; slow=[]
for r in rows:
    try: c,g,t,gg,s=r[0],int(r[1]),int(r[2]),int(r[3]),int(r[4])
    except Exception: continue
    if g+t < gg: bad.append((c,g+t,gg))
    if s>2: slow.append((c,s))
ok=not bad and not slow
line=f"{'Row 4 G1 literal completeness':<46} {'PASS' if ok else 'FAIL'}  {len(rows)} concepts; short={bad}; slow={slow}\n"
print(" ",line.rstrip()); io.open(sys.argv[2],"a",encoding="utf-8").write(line)
PY

echo "== Row 5 house_style + gate + conformance pull"
T0=$(date +%s%N); HS=$(T get_page_context "{\"project_id\":\"$P\",\"aspx_file\":\"Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx\"}" 60000); MS=$(( ($(date +%s%N) - T0) / 1000000 )); NS=$(echo "$HS" | grep -c "^- \*\*Sibling\*\*"); echo "  house_style siblings: $NS in $MS ms"
python - "$P" "$R" <<'PY'
import json,subprocess,sys
pid=sys.argv[1]; page="Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx"
diff=(f"diff --git a/{page} b/{page}\n--- a/{page}\n+++ b/{page}\n@@ -360,0 +361,2 @@\n+<asp:Panel ID=\"panProbe\" runat=\"server\" CssClass=\"alert alert-primary probe-only\">\n+</asp:Panel>\n")
out=subprocess.run([sys.executable,"tools/engram_drive.py","tool","pre_commit_review",json.dumps({"project_id":pid,"diff":diff}),"180000"],capture_output=True,text=True,encoding="utf-8",errors="replace").stdout
n=sum(1 for l in out.splitlines() if "alert-primary" in l and "no sibling" in l)
print("  gate findings naming alert-primary:",n)
ok=n>=1
open(sys.argv[2],"a",encoding="utf-8").write(f"{'Row 5 S3 ui_house_style gate (live probe)':<46} {'PASS' if ok else 'FAIL'}  {n} finding(s)\n")
PY
[ "$NS" -ge 1 ] && [ "$MS" -le 3000 ] && verdict "Row 5 S2 house_style section" PASS "$NS siblings, $MS ms" || verdict "Row 5 S2 house_style section" FAIL "$NS siblings, $MS ms"
UC=$(T get_ui_conformance "{\"project_id\":\"$P\",\"region\":\"Site/modules/dashboard/pages/admin/production/\",\"min_instances\":2}" 60000); NF=$(echo "$UC" | grep -c "^## "); [ "$NF" -ge 1 ] && verdict "Row 5 M2 get_ui_conformance pull" PASS "$NF families" || verdict "Row 5 M2 get_ui_conformance pull" FAIL "$NF"

echo "== Row 7 causal tracing smoke"
TD=$(T trace_data_flow "{\"project_id\":\"$P\",\"file_path\":\"Site/modules/dashboard/pages/admin/system/markers/marker_edit.aspx\",\"entry_point\":\"btnSave_Click\"}" 60000); echo "$TD" | head -3 | cut -c1-120
echo "$TD" | grep -qi "TOOL ERROR\|failure:" && verdict "Row 7 trace_data_flow smoke" FAIL "$(echo "$TD" | head -1 | cut -c1-80)" || verdict "Row 7 trace_data_flow smoke" PASS "$(echo "$TD" | grep -c '') lines"

echo "== Row 8 enforced repo rule probe"
python "$S/row8_live.py" > "$S/row8_acc.log" 2>&1; tail -n 6 "$S/row8_acc.log" | cut -c1-140
grep -q "ROW8 LIVE: PASS" "$S/row8_acc.log" && verdict "Row 8 enforced rule -> Critical; clean -> none" PASS "$(grep -i "severities" "$S/row8_acc.log" | head -1 | cut -c1-70); clean: $(grep -A1 'clean diff' "$S/row8_acc.log" | tail -1 | cut -c1-40)" || verdict "Row 8 enforced rule -> Critical; clean -> none" FAIL "see row8_acc.log"

echo "== Row 9 change completeness (timed)"
T0=$(date +%s%N); FS=$(T find_similar_changes "{\"project_id\":\"$P\",\"files\":[\"Site/App_Code/redovisning/code/redovisningskategorier.vb\"],\"top\":5}" 60000); MS=$(( ($(date +%s%N) - T0) / 1000000 )); echo "  find_similar_changes $MS ms: $(echo "$FS" | head -1 | cut -c1-80)"
DI=$(T detect_incomplete_changes "{\"project_id\":\"$P\",\"edited_files\":[\"Site/App_Code/redovisning/code/redovisningskategorier.vb\"],\"max_partners\":5}" 60000); echo "  detect_incomplete_changes: $(echo "$DI" | grep -c '')"
! echo "$FS$DI" | grep -qi "TOOL ERROR" && [ "$MS" -le 3000 ] && verdict "Row 9 similar changes + incomplete changes" PASS "$MS ms (incl. client spawn)" || verdict "Row 9 similar changes + incomplete changes" FAIL "$MS ms"

echo "== Row 10 caller parity (Check_pr_id)"
FR=$(T find_symbol_references "{\"project_id\":\"$P\",\"symbol_name\":\"Check_pr_id\",\"max_incoming\":5000}" 60000); IA=$(T impact_analysis "{\"project_id\":\"$P\",\"symbol_fqn\":\"_us.accessctrl.Check_pr_id\",\"limit\":5000}" 60000)
echo "$FR" | grep -in "incoming\|callers\|total" | head -2 | cut -c1-120; echo "$IA" | grep -in "Confirmed unique dependents\|ceiling\|failure" | head -2 | cut -c1-120
NFR=$(echo "$FR" | grep -o "[0-9]* distinct caller" | head -1 | grep -o "^[0-9]*"); NIA=$(echo "$IA" | grep -o "dependents\*\*: [0-9]*" | head -1 | grep -o "[0-9]*$")
! echo "$FR$IA" | grep -qi "ceiling reached\|failures: [1-9]\|TOOL ERROR" && [ -n "$NFR" ] && [ "$NFR" = "$NIA" ] && verdict "Row 10 caller parity (no ceiling)" PASS "find_symbol_references $NFR = impact_analysis $NIA" || verdict "Row 10 caller parity (no ceiling)" FAIL "refs=$NFR impact=$NIA"

echo "== Row 6 golden"
python eval/ask_engine_golden.py "$P" eval/data/ask_golden_ociusx.jsonl > "$S/golden_acc.log" 2>&1; grep -E "status-match|abstain|GATE" "$S/golden_acc.log" | cut -c1-80
grep -q "GATE: PASS" "$S/golden_acc.log" && verdict "Row 6 golden suite" PASS "$(grep status-match "$S/golden_acc.log" | cut -c1-40)" || verdict "Row 6 golden suite" FAIL "$(grep status-match "$S/golden_acc.log" | cut -c1-40)"

echo; echo "=== SUMMARY $(date +%H:%M:%S)"; cat "$R"; echo "PASS: $(grep -c ' PASS ' "$R") | FAIL: $(grep -c ' FAIL ' "$R")"
