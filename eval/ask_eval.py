"""Phase G arm-B wrapper: one ask_codebase question against a leak-free eval
index. Spawns a dedicated eval server (engram_client config: prod data_dir,
single-writer), asks, prints the report, exits. A crude cross-process file
lock serializes concurrent agents — the redb store admits ONE writer.

Usage: python eval/ask_eval.py <pr_id> "<question>"
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import engram_client as ec  # noqa: E402

LOCK = os.path.join(HERE, "data", "p2", "_ask_eval.lock")
IMAP = os.path.join(HERE, "data", "p2", "index_map.json")


def acquire(timeout=420):
    deadline = time.time() + timeout
    while True:
        try:
            fd = os.open(LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(fd, str(os.getpid()).encode())
            os.close(fd)
            return
        except FileExistsError:
            # A stale lock older than 10 min belongs to a dead run.
            try:
                if time.time() - os.path.getmtime(LOCK) > 600:
                    os.unlink(LOCK)
                    continue
            except OSError:
                pass
            if time.time() > deadline:
                raise TimeoutError("ask_eval lock busy — another agent is asking")
            time.sleep(3)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    pr, question = sys.argv[1], sys.argv[2]
    pid = json.load(open(IMAP, encoding="utf-8")).get(str(pr))
    if not pid:
        print(f"no index for PR {pr}")
        return 2
    acquire()
    try:
        eng = ec.Engram()
        try:
            out = eng.tool(
                "ask_codebase", {"project_id": pid, "question": question}
            )
            print(out)
        finally:
            eng.close()
    finally:
        try:
            os.unlink(LOCK)
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
