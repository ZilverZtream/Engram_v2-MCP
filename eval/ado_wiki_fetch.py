"""Fetch the OciusX documentation corpus (OX-Docs-Content repo — the wiki
equivalent, 173 markdown domain docs) via the ADO git items API (code
scope; the dedicated wiki API needs a scope this PAT lacks).

Writes eval/data/ox_docs/<path>.md mirroring the repo tree plus
eval/data/qg_wikis.json for ingestion. READ-ONLY GETs; PAT from ADO_PAT
env or eval/.secrets/ado_pat.txt (never echo it).
"""
import base64
import json
import os
import urllib.parse
import urllib.request

ORG, PROJECT, REPO = "patric0375", "OciusX", "OX-Docs-Content"


def pat() -> str:
    v = os.environ.get("ADO_PAT")
    if v:
        return v.strip()
    p = os.path.join(os.path.dirname(__file__), ".secrets", "ado_pat.txt")
    return open(p, encoding="utf-8").read().strip()


TOK = base64.b64encode(f":{pat()}".encode()).decode()


def get_json(url: str):
    req = urllib.request.Request(url)
    req.add_header("Authorization", f"Basic {TOK}")
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read().decode("utf-8"))


def get_text(url: str) -> str:
    req = urllib.request.Request(url)
    req.add_header("Authorization", f"Basic {TOK}")
    req.add_header("Accept", "text/plain")
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read().decode("utf-8", errors="replace")


def main() -> None:
    base = (
        f"https://dev.azure.com/{ORG}/{PROJECT}/_apis/git/repositories/{REPO}"
    )
    tree = get_json(f"{base}/items?recursionLevel=full&api-version=7.0")
    md_paths = [
        i["path"]
        for i in tree.get("value", [])
        if i.get("gitObjectType") == "blob"
        and i["path"].lower().endswith((".md", ".markdown"))
    ]
    print(f"{len(md_paths)} markdown docs in {REPO}")

    outdir = os.path.join(os.path.dirname(__file__), "data", "ox_docs")
    corpus = []
    for path in md_paths:
        url = (
            f"{base}/items?path={urllib.parse.quote(path)}"
            f"&includeContent=true&api-version=7.0&$format=text"
        )
        try:
            content = get_text(url)
        except Exception as e:  # noqa: BLE001
            print(f"  skip {path}: {e}")
            continue
        rel = path.lstrip("/")
        dst = os.path.join(outdir, rel.replace("/", os.sep))
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        open(dst, "w", encoding="utf-8").write(content)
        corpus.append({"wiki": REPO, "path": path, "content": content})
    json.dump(
        corpus,
        open(os.path.join(os.path.dirname(__file__), "data", "qg_wikis.json"),
             "w", encoding="utf-8"),
        ensure_ascii=False,
        indent=1,
    )
    print(f"DONE: {len(corpus)} docs -> {outdir} + qg_wikis.json")


if __name__ == "__main__":
    main()
