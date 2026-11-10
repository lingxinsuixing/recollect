#!/usr/bin/env python3
"""MiniMax Code 会话（messages.jsonl）-> ExchangeFile v1，用于「工作模式」语料。

源结构（一行一条消息）：
  {"message_id": "...", "turn_id": "legacy:mvs_<sid>:<n>",
   "message": {"role": "user|assistant|toolResult|compactionSummary|custom",
               "content": [{"type": "text|thinking|toolCall|image", ...}],
               "timestamp": <ms>, "toolCallId": ..., "toolName": ..., "isError": ...,
               "customType": ..., "summary": ..., ...}}

映射（按段保真；不做摘要/改写/合并）：
  user  + text                          -> human
       （先去掉 <system-reminder>…</system-reminder> 与
         <agent-context>…</agent-context> 块，剩下的才是人说的话）
  user  + 整段是系统/任务通知             -> system
       （<background-task-finished> / <plugin-hook-context> /
         <user-provided-context> / <task-notification> 打头；这些不是主人发言）
  assistant + text                      -> agent（image 只留 `[image]`）
  assistant + toolCall                  -> system，`[tool_call name=… id=…] <args>`
  toolResult                            -> system，`[tool_result[ error] for=<name>/<id>] <text>`
  compactionSummary                     -> system，`[[compaction_summary]] <summary>`
       （压缩摘要绝不冒充 human / agent）
  custom（todo_cadence_reminder 等）     -> 默认跳过并计数（`--keep-custom` 可落 system）
  thinking                              -> 丢弃（计数）
  空串                                  -> 跳过（计数）

纪律：
  * 超长正文截断到 8000 字并标 `[truncated]`；
  * created_at_ms 用 message.timestamp，缺失时用「上一条 + 1ms」兜底；
  * 确定性：同一输入两次运行逐字节相同（无随机、无时间依赖）；
  * 脱敏交给核心 `hub import`（本层不碰原文）。

用法：
  python3 tools/converters/minimax_v1.py <sessions 根目录 | messages.jsonl> \
      --out-dir <dir> [--limit 30] [--keep-custom] [--no-prefer-human]
  输出 <out-dir>/minimax.v1.json + <out-dir>/minimax.manifest.json
"""
import argparse
import hashlib
import json
import os
import re

MAX_BODY = 8000
# 「伪装成 user 的系统内容」：整段以此开头就不是主人说的话。
NOTICE_TAGS = (
    "system-reminder",
    "background-task-finished",
    "plugin-hook-context",
    "user-provided-context",
    "task-notification",
    "task-reminder",
    "agent-context",
)
SR_BLOCK = re.compile(r"<system-reminder>.*?</system-reminder>", re.S)
AC_BLOCK = re.compile(r"<agent-context>.*?</agent-context>", re.S)
# ask_user 的问卷回执也走 user 轮：XML 头（requestId/schemaVersion/选项 id）是机器噪音，
# 但它后面自带一段 "Q: … A: …" 的可读渲染，那才是主人选的答案。
QR_BLOCK = re.compile(r"^\s*<questionnaire-response>.*?</questionnaire-response>", re.S)
NOTICE_RE = re.compile(r"^\s*<(" + "|".join(NOTICE_TAGS) + r")>")


# 转换器层的值级脱敏：核心 import 目前只覆盖"关键词+值/固定前缀/URL userinfo"，
# 不覆盖邮箱与手机号；MiniMax 真实会话里这两类很多，所以在写 v1 之前先挡一层。
# 不是通用脱敏器：只挡已知形状，替换成占位符，保持可读与确定性。
REDACT_PATTERNS = [
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", re.S),
     "<private-key>"),
    (re.compile(r"\bsk-[A-Za-z0-9_\-]{16,}"), "<key>"),
    (re.compile(r"\b(ghp|gho|ghu|ghs|github_pat)_[A-Za-z0-9_]{20,}"), "<key>"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "<key>"),
    (re.compile(r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"), "<key>"),
    (re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._\-]{12,}"), "Bearer <token>"),
    (re.compile(r"(?i)\b(api[_-]?key|secret|token|passwd|password|passphrase)\s*[:=]\s*\S{6,}"),
     r"\1: <secret>"),
    (re.compile(r"(密码|口令|密钥)\s*[:：=是为]\s*\S{4,}"), r"\1: <secret>"),
    (re.compile(r"([a-z][a-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@"), r"\1<creds>@"),
    (re.compile(r"(?<!\d)1[3-9]\d{9}(?!\d)"), "<phone>"),
    (re.compile(r"[\w.+-]+@[\w-]+\.[\w.]{2,}"), "<email>"),
]


def redact(text):
    for rx, repl in REDACT_PATTERNS:
        text = rx.sub(repl, text)
    return text


def clip(s, counter):
    if len(s) <= MAX_BODY:
        return s
    counter[0] += 1
    return s[:MAX_BODY] + "\n[truncated]"


def strip_blocks(t):
    """去掉注入块：反复删直到稳定（块可能嵌套）。"""
    prev = None
    while prev != t:
        prev = t
        t = SR_BLOCK.sub("", t)
        t = AC_BLOCK.sub("", t)
    return t


def text_of(msg, counts):
    """拼接 content 里的 text（image 折成 [image]，thinking 计数后丢弃）。"""
    content = msg.get("content")
    if isinstance(content, str):
        return content.strip()
    parts = []
    for b in content or []:
        if not isinstance(b, dict):
            continue
        bt = b.get("type")
        if bt == "text":
            parts.append(b.get("text") or "")
        elif bt == "image":
            counts["image"] += 1
            parts.append("[image]")
        elif bt == "thinking":
            counts["thinking_skipped"] += 1
    return "\n".join(p for p in parts if p).strip()


def session_key_for(path):
    """会话目录名（<时间>-session_<id>）；同一会话内的 .history-* 子目录文件另加后缀。"""
    d = os.path.dirname(path)
    name = os.path.basename(d)
    parent = os.path.basename(os.path.dirname(d))
    if name.startswith("."):                      # .history-mutation-…/messages.jsonl
        outer = os.path.basename(os.path.dirname(os.path.dirname(d)))
        return "%s-%s" % (outer, hashlib.sha1(path.encode("utf-8")).hexdigest()[:8])
    return name


def convert(path, args, agg):
    counts = {k: 0 for k in (
        "human", "agent", "system", "empty_skipped", "thinking_skipped", "image",
        "tool_call", "tool_result", "tool_result_error", "compaction_summary",
        "custom_skipped", "notice_to_system", "truncated", "other_role_skipped",
        "unparsable", "ts_fallback")}
    trunc = [0]
    turns = []
    seq = 0
    last_ts = None
    sid = session_key_for(path)
    seen_uids = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except Exception:
                counts["unparsable"] += 1
                continue
            msg = d.get("message") or {}
            role = msg.get("role")
            ts = msg.get("timestamp")
            if isinstance(ts, (int, float)):
                ts = int(ts)
            elif last_ts is not None:
                ts = last_ts + 1
                counts["ts_fallback"] += 1
            else:
                ts = 0
            last_ts = ts
            part = [0]  # 本条源消息已产出的段数（一条 assistant 可拆成文本 + N 个工具轮）

            def emit(r, body, tag=None):
                nonlocal seq
                body = redact(clip(body.strip(), trunc))
                if not body:
                    counts["empty_skipped"] += 1
                    return
                # 会话分叉会让不同 session 共用同一批 message_id；一条源消息还可能
                # 拆成多轮（assistant 文本 + N 个工具调用）。turn_uid 用
                # 「会话 + message_id + 段号」保证全局唯一且稳定。
                mid = d.get("message_id") or ("seq%d" % seq)
                uid = "%s:%s" % (sid, mid)
                if part[0]:
                    counts["multi_part_turns"] = counts.get("multi_part_turns", 0) + 1
                    uid = "%s#%d" % (uid, part[0])
                part[0] += 1
                while uid in seen_uids:  # 同一 (会话, 消息, 段) 重复出现：再退一格
                    counts["dup_uid"] = counts.get("dup_uid", 0) + 1
                    uid = "%s~%d" % (uid, counts["dup_uid"])
                seen_uids.add(uid)
                turns.append({"turn_uid": uid,
                              "seq": seq, "tier": "id_bearing",
                              "role": r, "body": body, "created_at_ms": ts})
                counts[r] += 1
                seq += 1
                if tag:
                    counts[tag] = counts.get(tag, 0) + 1

            if role == "user":
                raw = text_of(msg, counts)
                body = strip_blocks(raw).strip()
                if not body:
                    counts["empty_skipped"] += 1
                    continue
                if QR_BLOCK.match(body):
                    tail = QR_BLOCK.sub("", body, count=1).strip()
                    if tail:                    # 只留 "Q: … A: …" 渲染，去掉 XML 头
                        body = tail
                        counts["questionnaire"] = counts.get("questionnaire", 0) + 1
                    else:                       # 没有可读渲染：原样落 system，不冒充主人
                        emit("system", "[questionnaire-response] %s" % body,
                             "questionnaire_raw")
                        continue
                m = NOTICE_RE.match(body)
                if m:  # 系统/任务通知伪装成 user：落 system，不冒充主人
                    emit("system", "[user-notice:%s] %s" % (m.group(1), body),
                         "notice_to_system")
                else:
                    emit("human", body)
            elif role == "assistant":
                body = text_of(msg, counts)
                if body:
                    emit("agent", body)
                for b in msg.get("content") or []:
                    if isinstance(b, dict) and b.get("type") == "toolCall":
                        args_json = b.get("arguments")
                        if not isinstance(args_json, str):
                            args_json = json.dumps(args_json, ensure_ascii=False,
                                                   sort_keys=True)
                        emit("system", "[tool_call name=%s id=%s] %s" % (
                            b.get("name") or "?", b.get("id") or "?", args_json),
                            "tool_call")
            elif role == "toolResult":
                body = text_of(msg, counts)
                err = bool(msg.get("isError"))
                emit("system", "[tool_result%s for=%s/%s] %s" % (
                    " error" if err else "", msg.get("toolName") or "?",
                    msg.get("toolCallId") or "?", body),
                    "tool_result_error" if err else "tool_result")
            elif role == "compactionSummary":
                body = msg.get("summary") or text_of(msg, counts)
                emit("system", "[[compaction_summary]] %s" % body,
                     "compaction_summary")
            elif role == "custom":
                if args.keep_custom:
                    body = strip_blocks(text_of(msg, counts)).strip()
                    emit("system", "[custom:%s] %s" % (msg.get("customType") or "?", body))
                else:
                    counts["custom_skipped"] += 1
            else:
                counts["other_role_skipped"] += 1

    if not turns:
        return None
    sess = {
        "session_key": "minimax:" + session_key_for(path),
        "device_id": args.device,
        "source_agent": args.source_agent,
        "machine": args.machine,
        "project_hint": args.project,
        "last_saved_at_ms": turns[-1]["created_at_ms"],
        "turns": turns,
    }
    counts["truncated"] = trunc[0]
    for k, v in counts.items():
        agg[k] = agg.get(k, 0) + v
    return sess


def discover(root):
    files = []
    if os.path.isfile(root):
        return [root]
    for dirpath, dirnames, filenames in os.walk(root):
        if "messages.jsonl" not in filenames:
            continue
        dirnames.sort()
        files.append(os.path.join(dirpath, "messages.jsonl"))
    files.sort(key=lambda p: p.replace("\\", "/"))
    return files


def count_human(path):
    """预扫描：这个会话里主人真说了几句（用于优先挑用户发言多的会话）。"""
    n = 0
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    msg = (json.loads(line).get("message") or {})
                except Exception:
                    continue
                if msg.get("role") != "user":
                    continue
                content = msg.get("content")
                parts = []
                if isinstance(content, str):
                    parts.append(content)
                for b in content or []:
                    if isinstance(b, dict) and b.get("type") == "text":
                        parts.append(b.get("text") or "")
                body = strip_blocks("\n".join(parts)).strip()
                if body and not NOTICE_RE.match(body):
                    n += 1
    except Exception:
        return 0
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("source", help="sessions 根目录，或单个 messages.jsonl")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--limit", type=int, default=30, help="最多几个会话（按 user 发言数优先）")
    ap.add_argument("--device", default="local-minimax")
    ap.add_argument("--source-agent", default="minimax")
    ap.add_argument("--machine", default="local")
    ap.add_argument("--project", default="work")
    ap.add_argument("--keep-custom", action="store_true",
                    help="custom 轮（todo 提醒等）落 system 而不是跳过")
    ap.add_argument("--no-prefer-human", action="store_true",
                    help="不按 user 发言数排序，只按路径")
    args = ap.parse_args()

    files = discover(args.source)
    # 只保留会话目录里的 messages.jsonl（排除 .history-* 之类子目录里的派生文件）
    files = [f for f in files
             if not os.path.basename(os.path.dirname(f)).startswith(".")]
    if args.no_prefer_human:
        picked = files[:args.limit] if args.limit > 0 else files
    else:
        scored = sorted(((count_human(f), f) for f in files),
                        key=lambda x: (-x[0], x[1].replace("\\", "/")))
        picked = [f for _, f in scored][:args.limit] if args.limit > 0 else [f for _, f in scored]

    agg = {}
    sessions = []
    for f in picked:
        s = convert(f, args, agg)
        if s is not None:
            sessions.append(s)

    doc = {"format_version": 1, "sessions": sessions}
    os.makedirs(args.out_dir, exist_ok=True)
    out = os.path.join(args.out_dir, "minimax.v1.json")
    with open(out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(doc, fh, ensure_ascii=False, indent=1)
        fh.write("\n")
    digest = hashlib.sha256(open(out, "rb").read()).hexdigest()

    manifest = {
        "format_version": 1,
        "source": os.path.abspath(args.source),
        "sessions_available": len(files),
        "sessions": len(sessions),
        "turns": sum(len(s["turns"]) for s in sessions),
        "roles": {r: sum(1 for s in sessions for t in s["turns"] if t["role"] == r)
                  for r in ("human", "agent", "system")},
        "counts": dict(sorted(agg.items())),
        "sha256": digest,
        "session_keys": [s["session_key"] for s in sessions],
    }
    mpath = os.path.join(args.out_dir, "minimax.manifest.json")
    with open(mpath, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(manifest, fh, ensure_ascii=False, indent=1)
        fh.write("\n")
    print(json.dumps({"out": out, "manifest": mpath,
                      "sessions": len(sessions), "turns": manifest["turns"],
                      "roles": manifest["roles"]}, ensure_ascii=False))


if __name__ == "__main__":
    main()
