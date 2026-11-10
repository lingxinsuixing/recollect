#!/usr/bin/env python3
"""`hermes_v1.py` 的行为测试（stdlib unittest，不联网、不读真实库）。

临时 sqlite 造 2 个会话 / 4 条消息：
  - ses-old：一条旧 user 消息（ts=1000.000）+ 一条新 assistant 消息（ts=5000.000）；
  - ses-new：一条旧 user 消息（ts=2000.000）——**增量窗口内没有新消息** ⇒ 不产出。

跑法：`python tools/converters/test_hermes_v1.py`
"""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "hermes_v1.py")

OLD_USER = "OLD-HUMAN-1110"
NEW_ASSISTANT = "NEW-ASSISTANT-1110"
ONLY_OLD = "ONLY-OLD-1110"

SCHEMA = """
create table sessions (
  id text primary key,
  source text,
  title text,
  started_at real,
  archived integer not null default 0
);
create table messages (
  id integer primary key autoincrement,
  session_id text not null,
  role text not null,
  content text,
  display_kind text,
  timestamp real
);
"""


def build_db(path):
    con = sqlite3.connect(path)
    try:
        con.executescript(SCHEMA)
        con.executemany(
            "insert into sessions (id, source, title, started_at, archived) values (?,?,?,?,?)",
            [
                ("ses-old", "cli", "t1", 900.0, 0),
                ("ses-new", "feishu", "t2", 1900.0, 0),
            ],
        )
        rows = [
            ("ses-old", "user", OLD_USER, None, 1000.000),
            ("ses-old", "assistant", NEW_ASSISTANT, None, 5000.000),
            ("ses-new", "user", ONLY_OLD, None, 2000.000),
            ("ses-old", "user", "SUBAGENT-NOTE-1110 deleg_00ab12cd", "process_complete", 6000.000),
        ]
        con.executemany(
            "insert into messages (session_id, role, content, display_kind, timestamp)"
            " values (?,?,?,?,?)",
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


class HermesV1Test(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name
        self.db = os.path.join(self.tmp, "state.db")
        build_db(self.db)

    def tearDown(self):
        self._tmp.cleanup()

    def convert(self, name, *extra):
        out = os.path.join(self.tmp, name)
        r = run_converter([out, "--db", self.db] + list(extra))
        self.assertEqual(r.returncode, 0, r.stderr.decode("utf-8", "replace"))
        return out

    # ① since 过滤只留新消息
    def test_since_ms_filters_by_message_time(self):
        full = load(self.convert("full.json"))
        bodies = [t["body"] for s in full["sessions"] for t in s["turns"]]
        self.assertIn(OLD_USER, bodies)
        self.assertIn(NEW_ASSISTANT, bodies)

        newer = load(self.convert("newer.json", "--since-ms", "3000000"))
        bodies = [t["body"] for s in newer["sessions"] for t in s["turns"]]
        self.assertNotIn(OLD_USER, bodies)
        self.assertIn(NEW_ASSISTANT, bodies)

    # ② 无新消息的会话不出现
    def test_session_without_new_messages_is_dropped(self):
        doc = load(self.convert("drop.json", "--since-ms", "3000000"))
        keys = [s["session_key"] for s in doc["sessions"]]
        self.assertIn("hermes:ses-old", keys)
        self.assertNotIn("hermes:ses-new", keys)

    # ③ 输出是合法 v1
    def test_output_is_valid_exchange_v1(self):
        doc = load(self.convert("v1.json"))
        self.assertEqual(doc["format_version"], 1)
        self.assertIsInstance(doc["sessions"], list)
        for s in doc["sessions"]:
            self.assertTrue(s["session_key"].startswith("hermes:"))
            self.assertTrue(s["device_id"])
            self.assertTrue(s["source_agent"])
            for t in s["turns"]:
                self.assertIn(t["role"], ("human", "agent", "system"))
                self.assertIsInstance(t["body"], str)
                self.assertIsInstance(t["created_at_ms"], int)

    # ④ 同一输入两次输出逐字节相同
    def test_output_is_byte_identical_across_runs(self):
        a = self.convert("a.json", "--since-ms", "3000000")
        b = self.convert("b.json", "--since-ms", "3000000")
        with open(a, "rb") as fa, open(b, "rb") as fb:
            self.assertEqual(fa.read(), fb.read())

    # 子 agent 结论：落成 agent 轮 + speaker，不冒充人
    def test_subagent_conclusion_becomes_agent_turn_with_speaker(self):
        doc = load(self.convert("sub.json"))
        by_body = {t["body"]: t for s in doc["sessions"] for t in s["turns"]}
        sub = by_body["[子 agent: deleg_00ab12cd]\nSUBAGENT-NOTE-1110 deleg_00ab12cd"]
        self.assertEqual(sub["role"], "agent")
        self.assertEqual(sub["speaker"], "subagent:deleg_00ab12cd")
        # 同一会话里的真人轮仍是 human，不带 speaker。
        human = [t for s in doc["sessions"] for t in s["turns"]
                 if t["body"] == OLD_USER][0]
        self.assertEqual(human["role"], "human")
        self.assertNotIn("speaker", human)

    # 输出目录模式：写 <dir>/hermes.v1.json
    def test_output_dir_receives_single_json(self):
        out_dir = os.path.join(self.tmp, "out")
        os.makedirs(out_dir, exist_ok=True)
        r = run_converter(["--db", self.db, "-o", out_dir])
        self.assertEqual(r.returncode, 0, r.stderr.decode("utf-8", "replace"))
        self.assertEqual(sorted(os.listdir(out_dir)), ["hermes.v1.json"])

    def test_missing_db_exits_2(self):
        r = run_converter([os.path.join(self.tmp, "x.json"), "--db",
                           os.path.join(self.tmp, "nope.sqlite")])
        self.assertEqual(r.returncode, 2)

    # 报告里只有计数，没有正文
    def test_report_has_no_body_text(self):
        r = run_converter([os.path.join(self.tmp, "r.json"), "--db", self.db])
        self.assertEqual(r.returncode, 0, r.stderr.decode("utf-8", "replace"))
        out = r.stdout.decode("utf-8", "replace")
        for secret in (OLD_USER, NEW_ASSISTANT, ONLY_OLD):
            self.assertNotIn(secret, out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
