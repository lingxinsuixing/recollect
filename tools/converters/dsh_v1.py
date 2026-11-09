#!/usr/bin/env python3
"""DeepSeek Harness（DSH / 小鲸鱼）会话 -> ExchangeFile v1。

DSH 的 session.v4.jsonl.zstd 是**事件流**：`user/message`、`assistant/message`（content 里
reasoning/text/tool-call 分块）、`tool/call`、`tool/result`、`system/message` 等。

映射（按段保真；不做摘要/改写/合并）：
  user/message        -> human（content 的 text；图片只留 `[image]`，不留路径）
  assistant/message   -> agent（content 的 text 拼接；reasoning 丢弃并计数）
  tool/call           -> system，`[tool_call name=… id=…] <arguments>`
  tool/result         -> system，`[tool_result for=…] <content>`（超长截断并标注）
  system/message      -> 丢弃（计数）：这是 agent 人格/系统提示，不是对话内容
  其余事件              -> 不产轮（计数）

纪律：脱敏交给核心 import；本层只做结构映射；同一输入两次运行逐字节相同。
"""
import argparse
import hashlib
import json
import os

import zstandard

MAX_TOOL_CHARS = 8000


def text_blocks(content):
    out = []
    for c in content or []:
        if not isinstance(c, dict):
            continue
        if c.get("type") == "text":
            out.append(c.get("text") or "")
        elif c.get("type") in ("image", "image_url"):
            out.append("[image]")
    return "\n".join(x for x in out if x).strip()


def clip(s):
    if len(s) <= MAX_TOOL_CHARS:
        return s
    return s[:MAX_TOOL_CHARS] + "\n[truncated]"


def convert(path, project, domain, device, source_agent, machine):
    with open(path, "rb") as fh:
        raw = zstandard.ZstdDecompressor().stream_reader(fh).read()
    recs = [json.loads(l) for l in raw.decode("utf-8", "ignore").splitlines() if l.strip()]
    meta = next((r for r in recs if r.get("type") == "session"), {})
    titles = [r["data"].get("title") for r in recs
              if r.get("type") == "session/title" and (r.get("data") or {}).get("title")]
    session_id = meta.get("id") or os.path.basename(os.path.dirname(path))
    turns = []
    counts = {"human": 0, "agent": 0, "system": 0, "reasoning": 0, "image": 0,
              "system_message": 0, "other": 0}
    last_ts = meta.get("createdAt") or 0
    for r in recs:
        t = r.get("type")
        d = r.get("data") or {}
        ts = r.get("time") or last_ts
        if t == "user/message":
            body = text_blocks(d.get("content"))
            counts["image"] += sum(1 for c in d.get("content") or []
                                   if isinstance(c, dict) and c.get("type") in ("image", "image_url"))
            if body:
                turns.append({"role": "human", "body": body, "created_at_ms": ts})
                counts["human"] += 1
                last_ts = ts
        elif t == "assistant/message":
            msg = d.get("message") or {}
            content = msg.get("content") or []
            counts["reasoning"] += sum(1 for c in content
                                       if isinstance(c, dict) and c.get("type") == "reasoning")
            body = text_blocks(content)
            if body:
                turns.append({"role": "agent", "body": body, "created_at_ms": ts})
                counts["agent"] += 1
                last_ts = ts
        elif t == "tool/call":
            name = d.get("name") or "?"
            call = d.get("callId") or "?"
            args = d.get("arguments") or ""
            turns.append({"role": "system",
                          "body": f"[tool_call name={name} id={call}] {clip(args)}",
                          "created_at_ms": ts})
            counts["system"] += 1
            last_ts = ts
        elif t == "tool/result":
            msg = d.get("message") or {}
            call = msg.get("toolCallId") or "?"
            body = text_blocks(msg.get("content"))
            turns.append({"role": "system",
                          "body": f"[tool_result for={call}] {clip(body)}",
                          "created_at_ms": ts})
            counts["system"] += 1
            last_ts = ts
        elif t == "system/message":
            counts["system_message"] += 1
        elif t in ("session", "session/title", "session/end-seed", "turn/start", "turn/end",
                   "step/start", "step/end", "assistant/attempt", "workspace/changes",
                   "agent/inbox/spliced", "request/header", "request/context", "permission/preset",
                   "sandbox/mode", "approval/policy"):
            pass
        else:
            counts["other"] += 1

    doc = {
        "format_version": 1,
        "sessions": [{
            "session_key": f"dsh:{session_id}",
            "device_id": device,
            "source_agent": source_agent,
            "machine": machine,
            "project_hint": project,
            "last_saved_at_ms": last_ts,
            "turns": turns,
        }],
    }
    return doc, {
        "source": path,
        "session": session_id,
        "agent_preset": meta.get("agentPreset"),
        "cwd": meta.get("cwd"),
        "title": titles[-1] if titles else "",
        "domain": domain,
        "project": project,
        "turns_out": len(turns),
        "counts": counts,
        "sha256": hashlib.sha256(json.dumps(doc, ensure_ascii=False, sort_keys=True).encode()).hexdigest(),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--project", default="personal")
    ap.add_argument("--domain", default="life")
    ap.add_argument("--device", default="local-dsh")
    ap.add_argument("--source-agent", default="dsh-jingyu")
    ap.add_argument("--machine", default="local")
    args = ap.parse_args()
    doc, man = convert(args.session, args.project, args.domain,
                       args.device, args.source_agent, args.machine)
    os.makedirs(args.out_dir, exist_ok=True)
    stem = str(man.get("session") or os.path.splitext(os.path.basename(args.session))[0])
    stem = stem.replace("/", "_").replace("\\", "_")
    out = os.path.join(args.out_dir, stem + ".v1.json")
    json.dump(doc, open(out, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    json.dump(man, open(os.path.join(args.out_dir, stem + ".manifest.json"), "w",
                        encoding="utf-8"), ensure_ascii=False, indent=1)
    print(json.dumps({"out": out, "turns": man["turns_out"], "counts": man["counts"],
                      "title": man["title"]}, ensure_ascii=False))


if __name__ == "__main__":
    main()
