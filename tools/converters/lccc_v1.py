#!/usr/bin/env python3
"""LCCC（中文对话，MIT）-> ExchangeFile v1，用于生活/闲聊覆盖探针。

LCCC 每行是一段对话的 JSON 数组，文本是**字符级分词**（"我 饿 了 。"）。
这里只做两件事：去掉字符间空格、把对话按交替角色映射成 human/agent。
不做摘要/改写；脱敏交给核心 import。

用法：
  python3 lccc_v1.py <lccc.jsonl.gz> --out <file.v1.json> [--limit 100] [--seed 7]
"""
import argparse
import gzip
import hashlib
import json
import os
import random
import re

BASE_MS = 1790304000000
MAX_TURN = 300


def clean(text):
    t = re.sub(r"\s+", "", text or "")
    return t.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("source")
    ap.add_argument("--out", required=True)
    ap.add_argument("--limit", type=int, default=100)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    dialogs = []
    with gzip.open(args.source, "rt", encoding="utf-8") as f:
        for line in f:
            try:
                d = json.loads(line)
            except Exception:
                continue
            turns = [clean(x) for x in d if isinstance(x, str)]
            turns = [t for t in turns if 2 <= len(t) <= MAX_TURN
                     and not re.search(r"https?://|[A-Za-z]{20,}", t)]
            if len(turns) >= 2:
                dialogs.append(turns)
    random.seed(args.seed)
    random.shuffle(dialogs)
    picked = dialogs[:args.limit]

    sessions = []
    domains = {}
    for i, turns in enumerate(picked):
        raw = "\n".join(turns)
        key = "lccc:" + hashlib.sha1(raw.encode()).hexdigest()[:12]
        rows = []
        for j, body in enumerate(turns):
            rows.append({
                "role": "human" if j % 2 == 0 else "agent",
                "body": body,
                "created_at_ms": BASE_MS + i * 86400000 + j * 60000,
            })
        sessions.append({
            "session_key": key,
            "device_id": "local-lccc",
            "source_agent": "lccc",
            "machine": "local",
            "project_hint": "personal",
            "last_saved_at_ms": BASE_MS + i * 86400000 + len(turns) * 60000,
            "turns": rows,
        })
        domains[key] = {"domain": "life", "turns": len(turns)}

    doc = {"format_version": 1, "sessions": sessions}
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    json.dump(doc, open(args.out, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    side = os.path.splitext(args.out)[0] + ".domains.json"
    json.dump(domains, open(side, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    print(json.dumps({"sessions": len(sessions),
                      "turns": sum(len(s["turns"]) for s in sessions),
                      "human": sum(1 for s in sessions for t in s["turns"] if t["role"] == "human"),
                      "out": args.out}, ensure_ascii=False))


if __name__ == "__main__":
    main()
