# 采集器水位迁移：ingest/qoder-state.json → collect-state.json
#
# 只搬 files 段的逐文件水位（mtime/size/offset/last_turns），并补 head_sha256；
# daily 的日期键换算成 `day-<epoch 天数>`；sqlite 段留空（首跑会自己 seed）。
#
# 用法：
#   python tools/pipeline/migrate-ingest-state.py \
#     --old data/ingest/qoder-state.json \
#     --new data/collect-state.json \
#     [--dry-run]
#
# 幂等：目标文件已存在时默认拒绝覆盖（--force 才合并覆盖）。旧文件的 offset 缺失按 0。
import argparse
import datetime
import hashlib
import json
import os
import sys

EPOCH = datetime.date(1970, 1, 1)


def head_sha256(path):
    try:
        with open(path, "rb") as f:
            return hashlib.sha256(f.read(4096)).hexdigest()
    except OSError:
        return ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old", required=True)
    ap.add_argument("--new", required=True)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()

    try:
        old = json.load(open(args.old, encoding="utf-8"))
    except Exception as exc:
        print("error: cannot read old state: %s" % exc, file=sys.stderr)
        return 2

    if os.path.exists(args.new) and not args.force:
        print("error: target exists (use --force to merge): %s" % args.new, file=sys.stderr)
        return 2

    try:
        doc = json.load(open(args.new, encoding="utf-8")) if os.path.exists(args.new) else {}
    except Exception:
        doc = {}
    files = dict(doc.get("files") or {})
    daily = dict(doc.get("daily") or {})

    migrated = 0
    for raw_path, cur in (old.get("files") or {}).items():
        if not isinstance(cur, dict):
            continue
        key = raw_path.replace("\\", "/")
        off = cur.get("offset")
        if not isinstance(off, int) or isinstance(off, bool) or off < 0:
            off = 0
        last = cur.get("last_turns")
        if not isinstance(last, int) or isinstance(last, bool) or last < 0:
            last = 0
        size = cur.get("size")
        files[key] = {
            "mtime_ms": int(cur.get("mtime_ms") or 0),
            "size": int(size or 0),
            "offset": off,
            "last_turns": last,
            "head_sha256": head_sha256(key),
        }
        migrated += 1

    # 日帽自 2026-11-10 起改为“滚动 24 小时窗口”（state.daily 只有 "window" 一条），
    # 旧的按日历日的 used 带不过来，也不该带：迁移只负责逐文件水位。
    daily = {}

    out = {"files": files, "daily": daily}
    if "sqlite" in doc:
        out["sqlite"] = doc["sqlite"]
    if "poisoned" in doc:
        out["poisoned"] = doc["poisoned"]

    if args.dry_run:
        print(json.dumps({"files": migrated, "daily": len(daily), "target": args.new}, ensure_ascii=False))
        return 0

    tmp = args.new + ".tmp-%d" % os.getpid()
    os.makedirs(os.path.dirname(args.new) or ".", exist_ok=True)
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        json.dump(out, f, ensure_ascii=False, sort_keys=True, indent=1)
        f.write("\n")
    os.replace(tmp, args.new)
    print(json.dumps({"files": migrated, "daily": len(daily), "target": args.new}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
