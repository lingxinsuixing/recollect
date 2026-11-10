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


def read_records(path):
    """读事件流，逐行 JSON 解码。

    **按 ``\\n`` 切行，不按 ``splitlines()``**：JSONL 的行分隔符只有换行，
    而 ``str.splitlines()`` 还会在 U+0085（NEL）、U+2028、U+2029 等字符处断行——
    这些字符能合法地出现在 JSON 字符串里（本机 125 个会话文件里就有 2 个含 NEL），
    断出来的碎片不是合法 JSON，整份会话就废了。

    单行解码失败只丢这一行（计数），不拖垮整份会话。
    """
    with open(path, "rb") as fh:
        raw = zstandard.ZstdDecompressor().stream_reader(fh).read()
    text = raw.decode("utf-8", "ignore")
    recs, malformed = [], 0
    for line in text.split("\n"):
        if not line.strip():
            continue
        try:
            recs.append(json.loads(line))
        except ValueError:
            malformed += 1
    return recs, malformed


def project_from_cwd(cwd, fallback="personal"):
    """会话头里的 ``cwd`` 折成逻辑项目名（无盘符、无分隔符、≤64）。

    DSH 的会话按工作区分目录，作用域按工作区走才不会全部挤成一个 scope。
    """
    if not cwd:
        return fallback
    base = str(cwd).replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]
    out = "".join(c if (c.isalnum() or c in "._-") else "-" for c in base).strip("-")
    return out[:64] or fallback


def convert(path, project, domain, device, source_agent, machine, project_mode="fixed",
            principal_id=None, principal_name=None):
    recs, malformed = read_records(path)
    meta = next((r for r in recs if r.get("type") == "session"), {})
    titles = [r["data"].get("title") for r in recs
              if r.get("type") == "session/title" and (r.get("data") or {}).get("title")]
    session_id = meta.get("id") or os.path.basename(os.path.dirname(path))
    if project_mode == "dir":
        project = project_from_cwd(meta.get("cwd"))
    turns = []
    counts = {"human": 0, "agent": 0, "system": 0, "reasoning": 0, "image": 0,
              "system_message": 0, "other": 0}
    if malformed:
        counts["malformed"] = malformed
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
    # 主人身份：只在显式给出时盖章（不猜默认主人）。
    pid = (principal_id or "").strip()
    if pid:
        doc["sessions"][0]["owner_principal_id"] = pid
        doc["sessions"][0]["principal_source"] = "agent_runtime"
        pname = (principal_name or "").strip()
        if pname:
            doc["sessions"][0]["principal_name"] = pname
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
    ap.add_argument("--project-mode", choices=("fixed", "dir"), default="fixed",
                    help="fixed=用 --project；dir=按会话头里的 cwd basename 推逻辑项目名")
    ap.add_argument("--domain", default="life")
    ap.add_argument("--device", default="local-dsh")
    ap.add_argument("--source-agent", default="dsh-jingyu")
    ap.add_argument("--machine", default="local")
    ap.add_argument("--principal-id", default=None,
                    help="显式给出才写主人身份")
    ap.add_argument("--principal-name", default=None)
    args = ap.parse_args()
    doc, man = convert(args.session, args.project, args.domain,
                       args.device, args.source_agent, args.machine,
                       args.project_mode, args.principal_id, args.principal_name)
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
