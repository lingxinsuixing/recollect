#!/usr/bin/env python3
"""`mcode_v1.py` 的行为测试（stdlib unittest，不联网、不读真实库）。

临时 sqlite 造 2 个会话 / 3 条消息：
  - ses-main（agent_name='mavis'）：一条旧 human（1000）+ 一条新 assistant（5000，
    msg_type=1，带 tool_calls）——用来验证 role 定身份、工具块不入正文；
  - ses-sub（session_kind='task'）：一条派发提示（6000）——用来验证子代理独立会话、
    role='system' 与 speaker='subagent:<id>'。

跑法：`python tools/converters/test_mcode_v1.py`
"""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "mcode_v1.py")

HUMAN_OLD = "EARLY-HUMAN-1110"
AGENT_NEW = "LATE-AGENT-1110"
DISPATCH = "dispatch-prompt-1110"
TOOL_ARG = "TOOL-ARGS-MUST-NOT-LEAK"
TOOL_RESULT = "TOOL-RESULT-MUST-NOT-LEAK"

SCHEMA = """
create table local_runtime_sessions (
  session_id text primary key,
  agent_name text,
  session_type text,
  session_kind text,
  parent_session_id text,
  title text,
  workspace_dir text,
  created_at_ms integer,
  updated_at_ms integer,
  archived integer not null default 0
);
create table local_runtime_message_rows (
  id integer primary key autoincrement,
  session_id text not null,
  role text,
  msg_id text,
  created_at_ms integer,
  data_json text not null
);
"""


def build_db(path):
    con = sqlite3.connect(path)
    try:
        con.executescript(SCHEMA)
        con.executemany(
            "insert into local_runtime_sessions (session_id, agent_name, session_type,"
            " session_kind, parent_session_id, title, workspace_dir, created_at_ms,"
            " updated_at_ms, archived) values (?,?,?,?,?,?,?,?,?,?)",
            [
                ("ses-main", "mavis", "code", "main", None, "t1", "D:/work/main", 1000, 6000, 0),
                ("ses-sub", "mavis", "code", "task", "ses-main", "t2", "D:/work/main", 6000, 6000, 0),
            ],
        )
        rows = [
            ("ses-main", "user", "m1", 1000,
             json.dumps({"role": "user", "msg_id": "m1", "msg_type": 1,
                         "msg_content": HUMAN_OLD}, ensure_ascii=False)),
            ("ses-main", "assistant", "m2", 5000,
             json.dumps({"role": "assistant", "msg_id": "m2", "msg_type": 1,
                         "msg_content": AGENT_NEW,
                         "tool_calls": [{
                             "tool_name": "shell",
                             "tool_call_id": "tc-1",
                             "tool_call_args": TOOL_ARG,
                             "tool_call_result_data": TOOL_RESULT,
                             "tool_call_status": 1,
                         }]}, ensure_ascii=False)),
            ("ses-sub", "user", "m3", 6000,
             json.dumps({"role": "user", "msg_id": "m3", "msg_type": 1,
                         "msg_content": DISPATCH}, ensure_ascii=False)),
        ]
        con.executemany(
            "insert into local_runtime_message_rows (session_id, role, msg_id,"
            " created_at_ms, data_json) values (?,?,?,?,?)",
            rows,
        )
        con.commit()
    finally:
        con.close()


def run_converter(args, cwd=None):
    return subprocess.run(
        [sys.executable, SCRIPT] + args,
        cwd=cwd or HERE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def bodies(doc):
    out = []
    for s in doc["sessions"]:
        for t in s["turns"]:
            out.append(t["body"])
    return out


class McodeV1Test(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name
        self.db = os.path.join(self.tmp, "runtime-state.sqlite")
        build_db(self.db)

    def tearDown(self):
        self._tmp.cleanup()

    def convert(self, name, *extra):
        out = os.path.join(self.tmp, name)
        r = run_converter([out, "--db", self.db] + list(extra))
        self.assertEqual(r.returncode, 0, r.stderr.decode("utf-8", "replace"))
        return out

    # ① --since-ms 只输出更新的消息
    def test_since_ms_filters_old_messages(self):
        full = load(self.convert("full.json"))
        self.assertEqual(len(bodies(full)), 3)
        self.assertIn(HUMAN_OLD, bodies(full))

        newer = load(self.convert("newer.json", "--since-ms", "2000"))
        self.assertEqual(sorted(bodies(newer)), sorted([AGENT_NEW, DISPATCH]))
        self.assertNotIn(HUMAN_OLD, bodies(newer))
        self.assertEqual(len(newer["sessions"]), 2)

    # ② role='assistant' 即使 msg_type=1 也算 agent
    def test_assistant_role_wins_over_msg_type(self):
        doc = load(self.convert("role.json"))
        by_body = {}
        for s in doc["sessions"]:
            for t in s["turns"]:
                by_body[t["body"]] = t
        self.assertEqual(by_body[AGENT_NEW]["role"], "agent")
        self.assertEqual(by_body[HUMAN_OLD]["role"], "human")

    # ③ 子代理独立会话 + speaker='subagent:<id>' + 派发提示 role='system'
    def test_subagent_session_is_separate_and_speaks_as_subagent(self):
        doc = load(self.convert("sub.json"))
        keys = {s["session_key"]: s for s in doc["sessions"]}
        self.assertIn("mcode:ses-main", keys)
        self.assertIn("mcode:ses-sub", keys)

        sub = keys["mcode:ses-sub"]
        self.assertEqual(len(sub["turns"]), 1)
        turn = sub["turns"][0]
        self.assertEqual(turn["role"], "system")
        self.assertEqual(turn["speaker"], "subagent:ses-sub")
        self.assertEqual(turn["body"], DISPATCH)

        # 主会话的话不冒充子代理，也不带 speaker 格。
        for t in keys["mcode:ses-main"]["turns"]:
            self.assertNotIn("speaker", t)

    # ④ 工具块不入正文（只计数）
    def test_tool_blocks_never_reach_turn_bodies(self):
        doc = load(self.convert("tools.json"))
        for body in bodies(doc):
            self.assertNotIn(TOOL_ARG, body)
            self.assertNotIn(TOOL_RESULT, body)
            self.assertNotIn("[tool_call", body)
            self.assertNotIn("[tool_result", body)
        # 带工具的那一轮正文仍是纯 msg_content。
        self.assertIn(AGENT_NEW, bodies(doc))

    # ⑤ 二跑输出确定性（同输入逐字节相同）
    def test_output_is_byte_identical_across_runs(self):
        a = self.convert("a.json", "--since-ms", "2000")
        b = self.convert("b.json", "--since-ms", "2000")
        with open(a, "rb") as fa, open(b, "rb") as fb:
            self.assertEqual(fa.read(), fb.read())

    def test_missing_db_exits_2(self):
        r = run_converter([os.path.join(self.tmp, "x.json"), "--db",
                           os.path.join(self.tmp, "nope.sqlite")])
        self.assertEqual(r.returncode, 2)

    def test_output_dir_receives_single_json(self):
        out_dir = os.path.join(self.tmp, "out")
        os.makedirs(out_dir, exist_ok=True)
        r = run_converter(["--db", self.db, "-o", out_dir])
        self.assertEqual(r.returncode, 0, r.stderr.decode("utf-8", "replace"))
        names = sorted(os.listdir(out_dir))
        self.assertEqual(names, ["mcode.v1.json"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
