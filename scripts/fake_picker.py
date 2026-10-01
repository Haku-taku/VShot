#!/usr/bin/env python3
"""端到端原型验证：驱动真实的 `vshot window pick`，替身扮演 Qt helper。

对每个候选窗口发一次 {"request":"elements","index":N}，把 CLI 回答的真实
元素树打出来，然后取消（不截图）。

用法：
    VSHOT_QT_HELPER=$PWD/scripts/fake_picker.py ./target/debug/vshot window pick --output /dev/null
"""
import json, os, sys, time

def main():
    session_path = None
    for i, a in enumerate(sys.argv):
        if a == "--session" and i + 1 < len(sys.argv):
            session_path = sys.argv[i + 1]
    if not session_path or not os.path.exists(session_path):
        sys.exit("no --session given")
    session = json.load(open(session_path))
    candidates = session.get("candidates", [])
    log = open("/tmp/fake_picker.log", "w", buffering=1)
    print(f"mode={session.get('mode')} candidates={len(candidates)}", file=log)

    for target in range(len(candidates)):
        c = candidates[target]
        sys.stdout.write(json.dumps({"request": "elements", "index": target}) + "\n")
        sys.stdout.flush()
        reply = None
        deadline = time.time() + 25
        while time.time() < deadline:
            line = sys.stdin.readline()
            if not line:
                break
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "elements" in obj:
                reply = obj["elements"]
                break
            if not obj:
                reply = []
                break
        n = "TIMEOUT" if reply is None else len(reply)
        print(f"\n=== window {target} {c.get('label','')!r} -> {n} elements ===", file=log)
        if not reply:
            continue
        winx, winy = c["x"], c["y"]
        inside = sum(1 for e in reply
                     if winx <= e["x"] < winx + c["width"]
                     and winy <= e["y"] < winy + c["height"])
        at_origin = sum(1 for e in reply if e["x"] == 0 and e["y"] == 0)
        bad = [i for i, e in enumerate(reply)
               if (p := e.get("parent")) is not None and p >= i]
        print(f"  inside window: {inside}/{len(reply)}", file=log)
        print(f"  at 0,0 (AT-SPI leak): {at_origin}", file=log)
        print(f"  parent out of order: {len(bad)}", file=log)
        for i, e in enumerate(reply[:14]):
            p = e.get("parent")
            d, seen, n2 = 0, set(), p
            while n2 is not None and n2 not in seen:
                seen.add(n2); n2 = reply[n2].get("parent") if n2 < len(reply) else None; d += 1
            print("  " * d + f"[{i}] {e.get('label','')!r} "
                  f"{e.get('width')}x{e.get('height')}+{e.get('x')}+{e.get('y')} parent={p}", file=log)

    sys.stdout.write(json.dumps({"status": "cancelled"}) + "\n")
    sys.stdout.flush()

if __name__ == "__main__":
    main()
