#!/usr/bin/env python3
"""MiniMax Code (MCode) 本机会话库 -> recollect ExchangeFile v1（hub import 解析的格式）。

来源（**只读 URI** 打开，库由 `--db` 必填；本文件不含任何本机路径）：
  - ``local_runtime_sessions``      会话目录：agent_name / session_kind / parent_session_id / workspace_dir
  - ``local_runtime_message_rows``  逐条消息行：data_json 内含 role / msg_content / tool_calls / thinking

为什么是 v1 而不是 v3
---------------------
``hub import`` 的 v3 分支会把 tool_call / tool_result / summary 整行丢掉，``speaker`` 也一律
置空、``project_hint`` 丢成 None（作用域退化成 session_key）。v1 这三格都能带
（``RawTurn.speaker`` / ``RawSession.project_hint``），所以走 v1。

映射口径（一条规则：**role 决定身份，msg_type 不决定**）
------------------------------------------------------
本机库里四种真实组合都出现过：
  role='user'       msg_type=1    → human 轮（正文 = msg_content）
  role='assistant'  msg_type=1    → agent 轮（旧 runtime 的历史行，msg_type 写 1）
  role='assistant'  msg_type=2    → agent 轮（新 runtime；正文空 = 这一回合只调了工具）
  role='assistant'  msg_type=None → 压缩事件（msg_id 以 ``compaction-`` 开头），不是发言
  role=None         msg_type=3    → todo 事件（msg_id 以 ``todo_`` 开头），不是发言

不导入的块（只计数、不打印、不落正文）：tool_call / tool_result / thinking / 附件。
契约硬规则第 5 条：v1 是纯文本轮，工具块跳过并计数；压缩摘要不得当 user 原话。

子代理（``session_kind='task'``，即派出去的 worker/explore/verifier）
--------------------------------------------------------------------
v1 没有父子链字段，按契约「独立成 session_key 并在报告里说明」：
  - 子会话独立成自己的 session_key（``mcode:<session_id>``）；
  - 它的**派发提示**是 harness 说的，不是人说的 ⇒ role='system'；
  - 它说的每一轮都带 ``speaker='subagent:<session_id>'`` ⇒ 子 agent 的话不冒充主 agent。

增量（采集器用）
----------------
``--since-ms N`` 只输出 ``created_at_ms > N`` 的轮（**按消息过滤**，不是按会话创建时间：
老会话里也会有新消息）。被过滤掉的会话不出现；整批无新轮时输出 ``sessions: []``。

确定性 / 幂等
------------
  - 会话按 session_id 排序，轮按 (created_at_ms, 构造序) 排序，JSON 键序固定 ⇒ 同一输入逐字节相同；
  - 不发 turn_uid，身份由 hub 按 (device_id, session_key, role, body, created_at_ms) 计算
    ⇒ 补导只追加新轮，重导整批 skipped_duplicate。

用法
----
  python mcode_v1.py <output.json|目录> --db PATH --since-ms N
         [--device local-mcode] [--source-agent mcode] [--machine local]
         [--project NAME]         # 缺省：按工作区目录名取逻辑项目名
         [--agent mavis]          # 哪个 agent 的"自有会话"算我的
         [--no-subagents]         # 不导子代理会话
         [--principal-id ID] [--principal-name NAME]   # 只在显式给出时盖章
         [--tool-blocks] [--include-archived] [--session ID]... [--limit N]

输出：路径以 ``.json`` 结尾就写它，否则当目录并写 ``<dir>/mcode.v1.json``（采集器的 `{out}`）。
stdout 只打印一份**无正文、无绝对路径**的计数报告；`--report FILE` 另存同一份。
退出码：0 = 转换完成；2 = 转换器自身错误（库打不开、输出写不了、参数缺失）。
"""
import argparse
import hashlib
import json
import re
import sqlite3
import sys
from pathlib import Path

from ingest_common import scrub

CONTROL_RE = re.compile(r"[\x00-\x1f\x7f]")
# 工具正文截断上限（与 rust/tools/converters/minimax_v1.py 的 MAX_BODY 同口径）。
MAX_BODY = 8000
OUT_NAME = "mcode.v1.json"


# ─────────────────────────── 参数 ───────────────────────────

def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="MCode 会话 -> recollect exchange v1")
    ap.add_argument("output", nargs="?", default=None,
                    help="输出文件（*.json）或目录（写 <dir>/%s）" % OUT_NAME)
    ap.add_argument("-o", "--out", default=None, help="同 output（collect 传 {out} 目录）")
    ap.add_argument("--db", required=True, help="runtime-state.sqlite 路径（只读 URI 打开）")
    ap.add_argument("--report", default=None, help="另存一份计数报告到这个文件")
    ap.add_argument("--device", default="local-mcode")
    ap.add_argument("--source-agent", default="mcode")
    ap.add_argument("--machine", default="local")
    ap.add_argument("--project", default=None,
                    help="逻辑项目名；缺省按工作区目录名取（不含盘符/路径）")
    ap.add_argument("--agent", default="mavis",
                    help="哪个 agent 的自有会话算「我的」")
    ap.add_argument("--no-subagents", action="store_true",
                    help="不导子代理会话（session_kind='task'）")
    ap.add_argument("--tool-blocks", action="store_true",
                    help="把 tool_call / tool_result 也落成 system 轮（缺省只计数；"
                         "契约第 5 条：v1 是纯文本轮）")
    ap.add_argument("--principal-id", default=None,
                    help="显式给出才写主人身份三格")
    ap.add_argument("--principal-name", default=None)
    ap.add_argument("--since-ms", type=int, default=None,
                    help="只导出 created_at_ms > N 的轮（采集器水位）")
    ap.add_argument("--until-ms", type=int, default=None,
                    help="只导出 created_at_ms <= N 的轮")
    ap.add_argument("--session", action="append", default=None,
                    help="只导这一个会话（可重复）；试跑取样用")
    ap.add_argument("--limit", type=int, default=None, help="扫描上限（最多看多少个会话）")
    ap.add_argument("--include-archived", action="store_true")
    args = ap.parse_args(argv)
    if args.output and args.out and args.output != args.out:
        ap.error("output 与 --out 同时给出且不一致")
    if not (args.output or args.out):
        ap.error("缺少输出路径（位置参数或 -o/--out）")
    return args


def resolve_output(args):
    spec = args.out or args.output
    p = Path(spec)
    if p.suffix.lower() == ".json":
        return p
    return p / OUT_NAME


# ─────────────────────────── 工具函数 ───────────────────────────

def bump(d, key, n=1):
    d[key] = d.get(key, 0) + n


def logical_project(workspace_dir, fallback="mcode"):
    """工作区目录 → 逻辑项目名。

    契约（``rc_contract::validate::logical_scope_name``）只收逻辑名：非空、≤64、
    不含 ``/ \\ :`` 与控制字符、不是 ``.`` / ``..``、首尾无空白。这里不写盘符也不写全路径。
    """
    if not workspace_dir:
        return fallback
    seg = re.split(r"[\\/]+", workspace_dir.strip())[-1]
    seg = CONTROL_RE.sub("", seg).replace(":", "").strip()
    if len(seg) > 64:
        seg = seg[:64].strip()
    if not seg or seg in (".", ".."):
        return fallback
    return seg


def principal_fields(args):
    """主人身份三格：只在显式 ``--principal-id`` 时才盖（不猜默认主人）。"""
    pid = (args.principal_id or "").strip()
    if not pid:
        return {}
    out = {"owner_principal_id": pid, "principal_source": "agent_runtime"}
    name = (args.principal_name or "").strip()
    if name:
        out["principal_name"] = name
    return out


def is_subagent_session(row):
    """子代理会话：session_kind='task'（派出去的 worker/explore/verifier 等）。"""
    return (row["session_kind"] or "") == "task" or bool(row["parent_session_id"])


# ─────────────────────────── 读源 ───────────────────────────

def open_ro(path):
    """只读 URI 打开；库不存在或不可读 ⇒ 抛 sqlite3.Error（由 main 收成退出码 2）。"""
    uri = "file:" + path.as_posix() + "?mode=ro"
    db = sqlite3.connect(uri, uri=True)
    db.row_factory = sqlite3.Row
    return db


def read_sessions(db, args):
    sql = """select session_id, agent_name, session_type, session_kind, parent_session_id,
                    title, workspace_dir, created_at_ms, updated_at_ms, archived
             from local_runtime_sessions"""
    where, params = [], []
    if args.session:
        where.append("session_id IN (%s)" % ",".join("?" for _ in args.session))
        params.extend(args.session)
    if not args.include_archived:
        where.append("archived = 0")
    if where:
        sql += " where " + " and ".join(where)
    # 不按会话 created_at_ms 过滤：增量要看的是**消息**时间，老会话里也会有新轮。
    sql += " order by session_id"
    if args.limit:
        sql += " limit %d" % int(args.limit)
    return db.execute(sql, params).fetchall()


def read_messages(db, session_id):
    return db.execute(
        """select id, role, msg_id, created_at_ms, data_json
           from local_runtime_message_rows where session_id = ? order by id""",
        (session_id,)).fetchall()


# ─────────────────────────── 轮构造 ───────────────────────────

def clip(text, limit=MAX_BODY):
    """工具正文可能几百 KB；超长截断并留标记（与 minimax_v1.py 同一口径）。"""
    if len(text) <= limit:
        return text
    return text[:limit] + "\n[truncated]"


def tool_result_text(raw):
    """MCode 的 tool_call_result_data 有四种形状：JSON 串 / 纯文本 / list / 其它。都要能读。"""
    if raw is None:
        return ""
    if isinstance(raw, (dict, list)):
        return json.dumps(raw, ensure_ascii=False)
    if not isinstance(raw, str):
        return str(raw)
    try:
        res = json.loads(raw)
    except ValueError:
        return raw
    if isinstance(res, dict):
        blocks = res.get("content")
        if isinstance(blocks, list):
            parts = []
            for b in blocks:
                if isinstance(b, dict) and isinstance(b.get("text"), str):
                    parts.append(b["text"])
                elif isinstance(b, str):
                    parts.append(b)
            if parts:
                return "\n".join(parts)
        return json.dumps(res, ensure_ascii=False)
    if isinstance(res, list):
        return json.dumps(res, ensure_ascii=False)
    return str(res)


def tool_turns(d, ts, redactions):
    """一个 assistant 行的 tool_calls → 两条 system 轮（调用 + 结果）。

    形状沿用 rust/tools/converters/README.md 的「元素闭集」：
      `[tool_call name=… id=…] <args>` / `[tool_result[ error] for=…] <text>`
    `tool_call_status == 3` 记为错误（MCode 不带 isError 字段，状态位是唯一错误信号）。
    """
    out = []
    for tc in d.get("tool_calls") or []:
        if not isinstance(tc, dict):
            continue
        name = str(tc.get("tool_name") or "tool")
        cid = str(tc.get("tool_call_id") or "")
        args = tc.get("tool_call_args")
        if not isinstance(args, str):
            args = json.dumps(args, ensure_ascii=False) if args is not None else ""
        out.append({
            "role": "system",
            "body": "[tool_call name=%s id=%s] %s" % (name, cid, clip(args)),
            "created_at_ms": ts,
        })
        text = tool_result_text(tc.get("tool_call_result_data"))
        if not text:
            continue
        tag = "tool_result[ error]" if tc.get("tool_call_status") == 3 else "tool_result"
        out.append({
            "role": "system",
            "body": "[%s for=%s/%s] %s" % (tag, name, cid, clip(text)),
            "created_at_ms": ts,
        })
    for t in out:
        t["body"] = scrub(t["body"], redactions)
    return out


def build_turns(rows, session_id, is_subagent, redactions, keep_tool_blocks=False,
                since_ms=None, until_ms=None):
    """把一个会话的消息行变成 exchange v1 的 turns。

    返回 (turns, dropped)。dropped 只放计数——正文一个字都不进报告。
    """
    turns, seen, dropped = [], set(), {}
    for row in rows:
        raw = row["data_json"]
        try:
            d = json.loads(raw)
        except ValueError:
            bump(dropped, "row_json_malformed")
            continue
        if not isinstance(d, dict):
            bump(dropped, "row_not_object")
            continue

        role = d.get("role")
        msg_id = str(d.get("msg_id") or row["msg_id"] or "")
        ts = row["created_at_ms"]
        if ts is None:
            ts = d.get("timestamp")
        if not isinstance(ts, int):
            ts = None

        # 不是一条发言：压缩事件 / todo 事件 / 未知 role。
        if role not in ("user", "assistant"):
            if msg_id.startswith("compaction-"):
                bump(dropped, "compaction_event")
            elif "todo" in msg_id:
                bump(dropped, "todo_event")
            else:
                bump(dropped, "role_" + str(role))
            continue

        # 增量窗口：按消息时间过滤（等号归旧轮，避免与上一轮重复）。
        if ts is not None and since_ms is not None and ts <= since_ms:
            bump(dropped, "before_since")
            continue
        if ts is not None and until_ms is not None and ts > until_ms:
            bump(dropped, "after_until")
            continue

        if d.get("tool_calls"):
            bump(dropped, "tool_call_block")
        if d.get("thinking_content"):
            bump(dropped, "thinking_block")

        # 工具块：缺省只计数不落轮（契约硬规则第 5 条）；`--tool-blocks` 才落 system 轮。
        if keep_tool_blocks and d.get("tool_calls") and ts is not None:
            for t in tool_turns(d, ts, redactions):
                key = (t["role"], t["body"], t["created_at_ms"])
                if key in seen:
                    bump(dropped, "duplicate_turn")
                    continue
                seen.add(key)
                turns.append(t)

        content = d.get("msg_content")
        body = content.strip() if isinstance(content, str) else ""
        if not body:
            # 正文空的 assistant 行 = 这一回合只调了工具，没有发言。
            bump(dropped, "assistant_tool_only" if role == "assistant" else "empty_body")
            continue

        body = scrub(body, redactions)
        if not body:
            bump(dropped, "empty_after_scrub")
            continue
        if ts is None:
            bump(dropped, "no_timestamp")
            continue

        if role == "user":
            # 子会话里的 user 行是 harness 的派发提示，不是人说的。
            turn_role = "system" if is_subagent else "human"
        else:
            turn_role = "agent"

        key = (turn_role, body, ts)
        if key in seen:
            bump(dropped, "duplicate_turn")
            continue
        seen.add(key)

        turn = {"role": turn_role, "body": body, "created_at_ms": ts}
        if is_subagent:
            turn["speaker"] = "subagent:" + session_id
        turns.append(turn)

    # 稳定排序：只按时间排，同一毫秒内保留构造顺序（工具轮与它那一行发言的先后要保住）。
    turns.sort(key=lambda t: t["created_at_ms"])
    return turns, dropped


# ─────────────────────────── 主流程 ───────────────────────────

def convert(db, args, out_path):
    fields = principal_fields(args)
    redactions = {}
    sessions_out = []
    total_dropped = {}
    skipped = {"not_mine": 0, "empty": 0, "subagent_disabled": 0, "no_new_rows": 0}
    role_counts = {"human": 0, "agent": 0, "system": 0}
    subagent_sessions = 0

    for s in read_sessions(db, args):
        sub = is_subagent_session(s)
        if sub:
            if args.no_subagents:
                skipped["subagent_disabled"] += 1
                continue
        elif s["agent_name"] != args.agent:
            skipped["not_mine"] += 1
            continue

        rows = read_messages(db, s["session_id"])
        if not rows:
            skipped["empty"] += 1
            continue

        turns, dropped = build_turns(
            rows, s["session_id"], sub, redactions,
            keep_tool_blocks=args.tool_blocks,
            since_ms=args.since_ms, until_ms=args.until_ms)
        for k, v in dropped.items():
            bump(total_dropped, k, v)
        if not turns:
            # 有行但全被增量窗口挡掉：不是"空会话"，单独计。
            if dropped.get("before_since") or dropped.get("after_until"):
                skipped["no_new_rows"] += 1
            else:
                skipped["empty"] += 1
            continue
        if sub:
            subagent_sessions += 1

        project = args.project or logical_project(s["workspace_dir"])
        sess = {
            "session_key": "mcode:" + s["session_id"],
            "device_id": args.device,
            "source_agent": args.source_agent,
            "machine": args.machine,
            "project_hint": project,
            "last_saved_at_ms": max(t["created_at_ms"] for t in turns),
        }
        sess.update(fields)
        sess["turns"] = turns
        sessions_out.append(sess)
        for t in turns:
            role_counts[t["role"]] = role_counts.get(t["role"], 0) + 1

    out = {"format_version": 1, "sessions": sessions_out}
    text = json.dumps(out, ensure_ascii=False, indent=1, sort_keys=False) + "\n"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(text, encoding="utf-8", newline="\n")

    turns_total = sum(len(s["turns"]) for s in sessions_out)
    report = {
        "device_id": args.device,
        "source_agent": args.source_agent,
        "format_version": 1,
        "sessions": len(sessions_out),
        "subagent_sessions": subagent_sessions,
        "turns": turns_total,
        # 没落成轮的源行数（压缩/todo/空正文/工具独占/重复窗口外），不是契约拒收。
        "turns_rejected": sum(total_dropped.values()),
        "roles": role_counts,
        "dropped_blocks": total_dropped,
        "skipped_sessions": skipped,
        "redactions": redactions,
        "principal": fields.get("owner_principal_id") or "未盖章",
        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
        "bytes": len(text.encode("utf-8")),
    }
    return report


def main(argv=None):
    args = parse_args(argv)
    try:
        db = open_ro(Path(args.db))
    except sqlite3.Error:
        print("mcode db is unreadable", file=sys.stderr)
        return 2

    out_path = resolve_output(args)
    try:
        report = convert(db, args, out_path)
    except sqlite3.Error:
        print("mcode db is unreadable", file=sys.stderr)
        return 2
    except OSError:
        print("output is not writable", file=sys.stderr)
        return 2
    finally:
        db.close()

    rpt = json.dumps(report, ensure_ascii=False, indent=1, sort_keys=True)
    if args.report:
        try:
            rp = Path(args.report)
            rp.parent.mkdir(parents=True, exist_ok=True)
            rp.write_text(rpt + "\n", encoding="utf-8", newline="\n")
        except OSError:
            print("report is not writable", file=sys.stderr)
            return 2
    print(rpt)
    return 0


if __name__ == "__main__":
    sys.exit(main())
