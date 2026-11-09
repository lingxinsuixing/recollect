#!/usr/bin/env python3
"""百度贴吧对话语料 -> ExchangeFile v1（生活/闲聊/陪伴覆盖探针）。

源：HuggingFace `Hanversion/Tieba-SomeInteresting`（Apache-2.0），2025-03-05 抓取的
孙笑川吧 / 弱智吧 / 中国人口吧 / 航空母舰吧 帖子。每个文件是 JSON 数组，一行一条：

  {"question": <帖子标题>, "answer": <最热门回复>, "think": <DeepSeek-v3 生成的思考链>}

映射（按源结构交替）：
  question -> human（发帖人）
  answer   -> agent（最高赞回复者；语料里就是"对话的另一方"）
  think    -> 丢弃并计数（这是 LLM 生成的推理，不是人类语料，混进会话会污染读数）

源里没有会话/楼层的边界，只有"帖"。本转换器把**同一吧内连续 N 条帖**串成一个 session
（默认 3 条 = 6 轮），session 边界是合成的、只为满足样本规模；`--per-session 1` 即"一帖一会话"。
不跨吧拼 session。

清洗：正文去首尾空白、把换行/制表/连续空白压成单空格；空串跳过；超过 `--max-chars`
（默认 500）的整条帖跳过（不截半句）；完全重复的 (question, answer) 只留第一条。
不做摘要、不改写；脱敏交给核心 import（本层不落任何 PII 明文文件）。

用法：
  python3 tieba_v1.py raw/*_train.json --out v1/combined.v1.json [--limit 100] [--per-session 3]
"""
import argparse
import hashlib
import json
import os
import re
import sys

BASE_MS = 1741132800000  # 2025-03-05T00:00:00Z，数据集抓取日
MAX_CHARS = 500
PER_SESSION = 3
WS = re.compile(r"\s+")
KEY_LEN = 12


def clean(text):
    """去首尾空白 + 折叠内部空白（含换行）。非字符串一律当空。"""
    if not isinstance(text, str):
        return ""
    t = text.replace("\u3000", " ").replace("\u200b", "")
    return WS.sub(" ", t).strip()


def load_records(path):
    """源是 JSON 数组；顺带容忍 JSONL 与 {'data': [...]} 包装。"""
    with open(path, encoding="utf-8-sig") as fh:
        raw = fh.read()
    try:
        data = json.loads(raw)
        if isinstance(data, dict):
            for k in ("data", "records", "rows"):
                if isinstance(data.get(k), list):
                    data = data[k]
                    break
        if not isinstance(data, list):
            raise ValueError("not a list")
        return data
    except Exception:
        out = []
        for line in raw.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except Exception:
                continue
        return out


def subforum_of(path):
    stem = os.path.basename(path)
    stem = re.sub(r"\.(json|jsonl)(\.(gz|zst|zstd))?$", "", stem, flags=re.I)
    stem = re.sub(r"[_\-](train|dev|test|original)$", "", stem, flags=re.I)
    return stem.lower() or "tieba"


def session_key(subforum, idx, first_q):
    raw = f"{subforum}|{idx}|{first_q}"
    return "tieba:" + hashlib.sha1(raw.encode("utf-8")).hexdigest()[:KEY_LEN]


def convert(sources, limit, per_session, max_chars):
    stats = {
        "sources": [],
        "records_read": 0,
        "records_used": 0,
        "dropped_empty_question": 0,
        "dropped_empty_answer": 0,
        "dropped_too_long": 0,
        "dropped_duplicate": 0,
        "dropped_think": 0,
        "human": 0,
        "agent": 0,
    }
    seen = set()
    per_source = []
    for path in sources:
        subforum = subforum_of(path)
        recs = load_records(path)
        st = {"file": os.path.basename(path), "subforum": subforum, "records": len(recs),
              "used": 0, "groups": 0, "sessions": 0}
        groups, buffer = [], []
        for rec in recs:
            stats["records_read"] += 1
            if isinstance(rec, dict) and rec.get("think"):
                stats["dropped_think"] += 1
            q = clean(rec.get("question")) if isinstance(rec, dict) else ""
            a = clean(rec.get("answer")) if isinstance(rec, dict) else ""
            if not q:
                stats["dropped_empty_question"] += 1
                continue
            if not a:
                stats["dropped_empty_answer"] += 1
                continue
            if len(q) > max_chars or len(a) > max_chars:
                stats["dropped_too_long"] += 1
                continue
            k = (subforum, q, a)
            if k in seen:
                stats["dropped_duplicate"] += 1
                continue
            seen.add(k)
            stats["records_used"] += 1
            st["used"] += 1
            buffer.append((q, a))
            if len(buffer) >= per_session:
                groups.append(buffer)
                buffer = []
        if buffer:
            groups.append(buffer)
        st["groups"] = len(groups)
        per_source.append((subforum, groups, st))
        stats["sources"].append(st)

    # 按吧轮转取 session，保证样本覆盖多个吧而不是被第一个文件吃满 --limit
    ordered = []
    depth = max((len(g) for _, g, _ in per_source), default=0)
    for i in range(depth):
        for subforum, groups, st in per_source:
            if i < len(groups):
                ordered.append((subforum, groups[i], st))
    if limit is not None:
        ordered = ordered[:limit]

    sessions = []
    for subforum, group, st in ordered:
        sessions.append(_mk_session(subforum, len(sessions), group, stats))
        st["sessions"] += 1
    return {"format_version": 1, "sessions": sessions}, stats


def _mk_session(subforum, idx, pairs, stats):
    key = session_key(subforum, idx, pairs[0][0])
    base = BASE_MS + idx * 86400000
    turns = []
    for j, (q, a) in enumerate(pairs):
        turns.append({"role": "human", "body": q, "created_at_ms": base + j * 120000})
        turns.append({"role": "agent", "body": a, "created_at_ms": base + j * 120000 + 60000})
    stats["human"] += len(pairs)
    stats["agent"] += len(pairs)
    return {
        "session_key": key,
        "device_id": "local-tieba",
        "source_agent": "tieba",
        "machine": "local",
        "project_hint": "personal",
        "last_saved_at_ms": base + (len(pairs) - 1) * 120000 + 60000,
        "turns": turns,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("sources", nargs="+", help="*_train.json（同一吧的帖）")
    ap.add_argument("--out", required=True, help="合并后的 v1 文件")
    ap.add_argument("--limit", type=int, default=100, help="最多 session 数（默认 100）")
    ap.add_argument("--per-session", type=int, default=PER_SESSION,
                    help="每个 session 装几条帖（默认 3；1 = 一帖一会话）")
    ap.add_argument("--max-chars", type=int, default=MAX_CHARS,
                    help="单轮正文上限，超过整条帖跳过（默认 500）")
    args = ap.parse_args()

    doc, stats = convert(args.sources, args.limit, max(1, args.per_session), args.max_chars)
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, ensure_ascii=False, indent=1)
    stats["sessions"] = len(doc["sessions"])
    stats["turns"] = sum(len(s["turns"]) for s in doc["sessions"])
    stats["out"] = args.out
    stats["sha256"] = hashlib.sha256(
        json.dumps(doc, ensure_ascii=False, sort_keys=True).encode("utf-8")).hexdigest()
    print(json.dumps(stats, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    sys.exit(main())
