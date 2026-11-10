#!/usr/bin/env python3
"""Hermes 本机会话库（state.db）-> recollect ExchangeFile v1。

来源（**只读 URI** 打开，库由 `--db` 必填；本文件不含任何本机路径）：
  - ``sessions``  会话目录：id / source / title / started_at / archived
  - ``messages``  逐条消息：id / session_id / role / content / display_kind / timestamp

为什么是 v1 而不是 v3
---------------------
``hub import`` 的 v3 分支会把 tool_call / tool_result / summary 整行丢掉，``speaker`` 也一律
置空、``project_hint`` 丢成 None（作用域退化成 session_key）。v1 这三格都能带
（``RawTurn.speaker`` / ``RawSession.project_hint``），所以走 v1。

映射口径（一条规则：**role 决定身份，display_kind 决定这条 user 轮是谁说的**）
--------------------------------------------------------------------------
  - role=user, display_kind ∈ {NULL, 'steer'}                -> human 轮
  - role=user, display_kind ∈ {'internal_notification',
                               'process_complete'}           -> agent 轮 + ``speaker='subagent:<deleg_id>'``
                                                                （子 agent 的结论，不冒充主 agent）
  - role=assistant                                            -> agent 轮
  - role=tool / session_meta / 空正文                          -> 不产轮（只计数）

``messages.timestamp`` 是**秒**（REAL）；轮上的 ``created_at_ms`` 取
``int(ts * 1000 + 0.5)``（四舍五入到毫秒）。SQL 侧的游标必须用同一个公式，
否则水位与轮时间会差 1 毫秒、边界消息会被重复导出。

增量（采集器用）
----------------
``--since-ms N`` 按**消息时间**过滤：只保留 ``created_at_ms > N`` 的消息；
一个会话若一条这样的消息都没有，就**不产出**该会话。这样水位与"消息时间"同源，
增量不会漏也不会重。

确定性 / 幂等
--------------
  - 会话按 id 排序、消息按 (timestamp, id) 排序 ⇒ 同一输入两次输出逐字节相同；
  - 不发 turn_uid，身份由 hub 按 (device_id, session_key, role, body, created_at_ms) 计算
    ⇒ 补导只追加新消息，重导整批 skipped_duplicate。

用法
----
  python hermes_v1.py <out.json|out_dir> --db PATH --since-ms N
         [--device local-hermes] [--source-agent hermes-agent] [--machine local]
         [--project hermes] [--sources cli,feishu] [--include-archived]
         [--principal-id ID] [--principal-name NAME] [--limit N] [--report FILE]

输出：路径以 ``.json`` 结尾就写它，否则当目录并写 ``<dir>/hermes.v1.json``。
stdout 只打印一份**无正文、无绝对路径**的计数报告；``--report FILE`` 另存同一份。
退出码：0 = 转换完成；2 = 转换器自身错误（库打不开、输出写不了）。
"""
import argparse
import hashlib
import json
import re
import sqlite3
import sys
from pathlib import Path

from ingest_common import scrub

# display_kind：user 轮里哪些是"人说的"，哪些是系统注入的。
HUMAN_KINDS = {None, "steer"}
# user 轮里哪些其实带着子 agent 的结论（不是人说的）。
SUBAGENT_KINDS = {"internal_notification", "process_complete"}
# 子 agent  delegation id（deleg_<hex>）——只在正文里找，找不到就用 unknown。
DELEG_RE = re.compile(r"deleg_[0-9a-f]+")

OUT_NAME = "hermes.v1.json"


# ─────────────────────────── 参数 ───────────────────────────

def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="Hermes state.db -> recollect exchange v1")
    ap.add_argument("output", nargs="?", default=None,
                    help="输出文件（*.json）或目录（写 <dir>/%s）" % OUT_NAME)
    ap.add_argument("-o", "--out", default=None, help="同 output")
    ap.add_argument("--db", required=True, help="state.db 路径（只读 URI 打开）")
    ap.add_argument("--report", default=None, help="另存一份计数报告到这个文件")
    ap.add_argument("--device", default="local-hermes")
    ap.add_argument("--source-agent", default="hermes-agent")
    ap.add_argument("--machine", default="local")
    ap.add_argument("--project", default="hermes", help="逻辑项目名（作用域键）")
    ap.add_argument("--sources", default=None,
                    help="逗号分隔的 sessions.source 白名单；缺省 = 全部")
    ap.add_argument("--include-archived", action="store_true",
                    help="连已归档的会话一起导（缺省只导未归档）")
    ap.add_argument("--since-ms", type=int, default=None,
                    help="只导出 created_at_ms > N 的消息（水位）")
    ap.add_argument("--principal-id", default=None,
                    help="显式给出才写主人身份")
    ap.add_argument("--principal-name", default=None)
    ap.add_argument("--limit", type=int, default=None, help="最多导多少个会话")
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


def ts_to_ms(ts_s):
    """秒（REAL）-> 毫秒：四舍五入（半数进位）。

    与 SQL 侧的游标 ``CAST(timestamp*1000+0.5 AS INTEGER)`` **同一个公式**：
    两边差 1 毫秒，水位就会与轮时间错位（边界消息忽进忽出）。
    """
    if ts_s is None:
        return None
    return int(ts_s * 1000 + 0.5)


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


# ─────────────────────────── 读源 ───────────────────────────

def open_ro(path):
    """只读 URI 打开；库不存在或不可读 ⇒ 抛 sqlite3.Error（由 main 收成退出码 2）。"""
    uri = "file:" + Path(path).as_posix() + "?mode=ro"
    db = sqlite3.connect(uri, uri=True)
    db.row_factory = sqlite3.Row
    return db


def read_sessions(db, args):
    sql = "select id, source, title, started_at, archived from sessions"
    where, params = [], []
    if not args.include_archived:
        where.append("archived = 0")
    if args.sources:
        names = sorted({s.strip() for s in args.sources.split(",") if s.strip()})
        if names:
            where.append("source IN (%s)" % ",".join("?" for _ in names))
            params.extend(names)
    if where:
        sql += " where " + " and ".join(where)
    # 按 id 排序：同一输入两次读到同一顺序 ⇒ 输出逐字节相同。
    sql += " order by id"
    if args.limit:
        sql += " limit %d" % int(args.limit)
    return db.execute(sql, params).fetchall()


def read_messages(db, session_id, since_ms):
    """一个会话的消息（按 (timestamp, id) 排）。

    ``since_ms`` 按**消息时间**过滤。SQL 侧只做**粗筛**（放宽 1 毫秒），
    精确的毫秒比较在 Python 侧做——``timestamp`` 是 REAL 秒，拿它跟毫秒水位比会在
    边界上差 1 毫秒（同一毫秒里的消息忽进忽出）。
    """
    sql = ("select id, role, content, display_kind, timestamp from messages "
           "where session_id = ?")
    params = [session_id]
    if since_ms is not None:
        sql += " and timestamp > ?"
        # 粗筛放宽 1ms：精确判定交给 build_turns 的毫秒比较。
        params.append((since_ms - 1) / 1000.0)
    sql += " order by timestamp, id"
    return db.execute(sql, params).fetchall()


# ─────────────────────────── 轮构造 ───────────────────────────

def build_turns(rows, redactions, since_ms=None):
    """一个会话的消息行 -> exchange v1 的 turns。

    返回 (turns, dropped)。dropped 只放计数——正文一个字都不进报告。
    """
    turns, seen, dropped = [], set(), {}
    for row in rows:
        role = row["role"]
        dk = row["display_kind"]
        ts_ms = ts_to_ms(row["timestamp"])
        if ts_ms is None:
            bump(dropped, "no_timestamp")
            continue
        # 增量窗口：按**消息时间**过滤（毫秒精确比较，等于水位的不算新）。
        if since_ms is not None and ts_ms <= since_ms:
            bump(dropped, "before_since")
            continue
        body = scrub((row["content"] or "").strip(), redactions)
        if not body:
            bump(dropped, "empty_body")
            continue
        speaker = None
        if role == "user":
            if dk in HUMAN_KINDS:
                turn_role = "human"
            elif dk in SUBAGENT_KINDS:
                m = DELEG_RE.search(body)
                speaker = "subagent:" + (m.group(0) if m else "unknown")
                body = "[子 agent: " + (m.group(0) if m else "background") + "]\n" + body
                turn_role = "agent"
            else:
                # 身份不明的 user 轮不猜归属（它可能是注入的）。
                bump(dropped, "user_unknown_kind")
                continue
        elif role == "assistant":
            turn_role = "agent"
        else:
            bump(dropped, "skipped_role_" + str(role))
            continue
        key = (turn_role, body, ts_ms, speaker)
        if key in seen:
            bump(dropped, "duplicate_turn")
            continue
        seen.add(key)
        turn = {"role": turn_role, "body": body, "created_at_ms": ts_ms}
        if speaker:
            turn["speaker"] = speaker
        turns.append(turn)
    return turns, dropped


# ─────────────────────────── 主流程 ───────────────────────────

def convert(db, args, out_path):
    fields = principal_fields(args)
    redactions = {}
    sessions_out = []
    total_dropped = {}
    role_counts = {"human": 0, "agent": 0, "system": 0}
    skipped_sessions = 0
    since_ms = args.since_ms

    for s in read_sessions(db, args):
        rows = read_messages(db, s["id"], since_ms)
        turns, dropped = build_turns(rows, redactions, since_ms)
        for k, v in dropped.items():
            bump(total_dropped, k, v)
        if not turns:
            # 增量窗口内一条新消息都没有 ⇒ 不产出该会话（避免空会话进库）。
            skipped_sessions += 1
            continue
        sess = {
            "session_key": "hermes:" + s["id"],
            "device_id": args.device,
            "source_agent": args.source_agent,
            "machine": args.machine,
            "project_hint": args.project,
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

    return {
        "device_id": args.device,
        "source_agent": args.source_agent,
        "format_version": 1,
        "sessions": len(sessions_out),
        "turns": sum(len(s["turns"]) for s in sessions_out),
        "roles": role_counts,
        "dropped_blocks": total_dropped,
        "skipped_sessions": skipped_sessions,
        "redactions": redactions,
        "principal": "已盖章" if fields else "未盖章",
        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
        "bytes": len(text.encode("utf-8")),
    }


def main(argv=None):
    args = parse_args(argv)
    try:
        db = open_ro(args.db)
    except sqlite3.Error:
        print("hermes db is unreadable", file=sys.stderr)
        return 2

    out_path = resolve_output(args)
    try:
        report = convert(db, args, out_path)
    except sqlite3.Error:
        print("hermes db is unreadable", file=sys.stderr)
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
