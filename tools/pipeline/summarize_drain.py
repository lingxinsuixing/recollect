#!/usr/bin/env python3
"""把 daily-drain 的 JSONL 日志收成一张按 pass 的读数表（操作员用）。

日志的形状是既定的：一行一个事件，`t` ∈ {start, distill, review, episode, anomaly, backup,
cold_backup, preclaim, reuse, state_snapshot, state_uids, end, fatal}；
`preclaim`（G13 补章）与 `reuse`（reuse-ingest 反馈入队）两族**暂不归堆**：本层既没有这两个
分派分支、也不在白名单里，所以它们一行不印也不报错（已知债，登记在 README）；
`out` 是被 `json.dumps` 包了一层的**原始 stdout+stderr**（hub 失败时是"读数 + error: 那一行"两段）。
这一层不重述判定，只做归堆：调用数、吃掉多少轮/条、非零退出码清单，末尾照抄终态分布。
episode 归组档（2026-11-04 I-6b）的 `work` 记的是落库行数 `episodes_written`——这一档零模型调用
（`used` 恒 0），`work` 在它身上**不是**调用数。（`derive` 的事件行/phase 附加格本轮仍不归堆，
缺口登记在 README。）
保底档的两档维护（`--full`）也在这一层：
  - `t:"anomaly"`（W26）归堆成 `anomaly`/`maintenance`，`work` 记"这一扫升起的告警数"
    （`contradiction.raised + ghosts.raised`），解除数抄进备注；
  - `t:"backup"`（W28）归堆成 `backup`/`maintenance`，`work` 记 `memory_rows`（这一组覆盖了多少行），
    `name` / `memory` / `extras` 抄进备注；
  - `t:"cold_backup"`（W28）不是"一发调用"而是本轮的物理冷备读数：单独印一行
    `cold_backup ledger=… core=… pruned=…`（失败时带 `error`），不并进调用桶。

用法：
  python3 summarize_drain.py <log.jsonl>
退出码：0 有读数；3 文件读不出任何事件行（空文件或路径错）。
"""
from __future__ import annotations

import json
import sys
from collections import OrderedDict


def first_json(text: str):
    """取一段文本里**第一个**能整体解析的 JSON 对象（hub 把读数和 error 行打在一起）。"""
    head = text.strip().split("\n", 1)[0]
    try:
        return json.loads(head)
    except Exception:
        return None


def review_json(text: str):
    start = text.find("{")
    if start < 0:
        return None
    try:
        return json.loads(text[start:])
    except Exception:
        return None


def main(path: str) -> int:
    rows = []
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except Exception:
            rows.append({"t": "unparsed", "line": line[:200]})
    if not rows:
        print(f"no events in {path}", file=sys.stderr)
        return 3

    buckets = OrderedDict()
    phase_lines = []
    start = end = None
    snapshot = None
    cold = None
    for r in rows:
        kind = r.get("t")
        # `start` / `end` 两行用时间戳本身当键（`{"start":…}`），没有 `t` 那一格。
        # 早先按 `kind == "start"` 取 ⇒ 两行永远取不到，汇总表上看不见"这一夜用的哪个二进制"
        # 与"总共花掉多少"（原始日志里都在）。
        if "start" in r:
            start = r
            continue
        if "end" in r:
            end = r
            continue
        if kind == "state_snapshot":
            snapshot = r
            continue
        if kind == "cold_backup":
            # 物理冷备（W28）：一行读数、不是一发调用，所以不并进调用桶。轮转只发生一次，
            # 正常情况下这一天只有一行；真出现多行（重跑）时留最后一行的文件名、把裁掉的对数累加。
            if cold is None:
                cold = dict(r)
            else:
                cold["pruned"] = (cold.get("pruned") or 0) + (r.get("pruned") or 0)
            continue
        if kind in ("distill", "review", "episode", "anomaly", "backup"):
            key = (kind, r.get("pass", ""), r.get("family") or "")
            b = buckets.setdefault(key, {"calls": 0, "work": 0, "rc": [], "note": []})
            b["calls"] += 1
            if r.get("rc") != 0:
                b["rc"].append(r.get("rc"))
            out = r.get("out", "")
            if kind == "distill":
                doc = first_json(out)
                if doc is None:
                    b["note"].append(out.strip().split("\n")[-1][:60])
                else:
                    b["work"] += doc.get("turns") or 0
                    if doc.get("state") not in (None, "empty", "none"):
                        b["note"].append(f"{doc.get('state')}x1")
            elif kind == "episode":
                # episode 归组档（2026-11-04 I-6b）：`work` 记落库行数 `episodes_written`
                # （这一档零模型调用，`used` 恒 0——见 daily-drain.sh 的 run_episode 头①）。
                # 分诊读数只抄 merged_pairs / cards_written 两个非零格；逐发累计的 `work`
                # 与 `phase` 行的 `episodes_total` 同一口径，两边对得上。
                doc = first_json(out)
                if doc is None:
                    b["note"].append(out.strip().split("\n")[-1][:60])
                else:
                    b["work"] += doc.get("episodes_written") or 0
                    for cell in ("merged_pairs", "cards_written"):
                        if doc.get(cell):
                            b["note"].append(f"{cell}={doc[cell]}")
            elif kind == "backup":
                # 备份档（W28）：`work` 记这一组覆盖的记忆行数（`memory_rows`），
                # 文件名与随组走的 extras 份数抄进备注——它们不是量，抄成数会把整列读歪。
                doc = first_json(out)
                if doc is None:
                    b["note"].append(out.strip().split("\n")[-1][:60])
                else:
                    b["work"] += doc.get("memory_rows") or 0
                    for cell in ("name", "memory"):
                        if doc.get(cell):
                            b["note"].append(f"{cell}={doc[cell]}")
                    if doc.get("extras") is not None:
                        b["note"].append(f"extras={doc.get('extras')}")
            elif kind == "anomaly":
                # 告警复核（W26）：`work` 记这一扫**升起**的告警数（矛盾一族 + 重影一族）。
                # 解除数单独抄进备注：它与升起数不是同一件事，加在一起会把"复核在动"念成"告警更多"。
                doc = first_json(out)
                if doc is None:
                    b["note"].append(out.strip().split("\n")[-1][:60])
                else:
                    for fam, label in (("contradiction", "contradiction"), ("ghosts", "ghosts")):
                        cell = doc.get(fam) or {}
                        b["work"] += cell.get("raised") or 0
                        if cell.get("resolved"):
                            b["note"].append(f"{label}_resolved={cell.get('resolved')}")
            else:
                doc = review_json(out)
                if doc is None:
                    b["note"].append(out.strip().split("\n")[-1][:60])
                else:
                    b["work"] += doc.get("processed") or 0
                    for cell in ("applied", "rejected", "quarantined", "errors"):
                        if doc.get(cell):
                            b["note"].append(f"{cell}={doc[cell]}")
                    b["remaining"] = doc.get("remaining_pending_review")
        elif kind == "phase":
            # 收着最后一起印：`phase` 是在循环里直接 print 的话，它会跑到 `start=` 那行**前面**，
            # 汇总表读起来像"先有结论再有配置"。顺序应当是 start → 每族 phase → 逐发计数 → 终态 → end。
            # episode 档的行数格附在最后（2026-11-04 I-6b）：`episodes_total` 是 `episodes_written`
            # 的累计值（这一档零调用、不进 used），与逐发累计的 `work` 同口径。
            # W19c2b：只剩 `quarantined` 一格（只追加在最后，前列序一个不动）。
            # 它数的是**抽取档里因重试耗尽而整批隔离**的发数（脚本只认 hub 读数面的
            # `retry_exhausted=1`）；质量隔离（审核不确定、契约拒收）不计入，也不从 `state` 猜。
            # `retried` 已删：旧口径按 `state=pending` 数"重试中"，可那同一格也是
            # "入队待审核"的合法结果——它把每一发成功都记成了重试（W19c2 partial 实测 47 里有 45 是正常产出）。
            extra = ""
            if "quarantined" in r:
                extra += " quarantined={}".format(r["quarantined"])
            if "episodes_total" in r:
                extra += " episodes_total={} cursor_moved={}".format(
                    r["episodes_total"], r.get("cursor_moved"))
            # 告警复核（W26）：四格也附在最后（同一律：只追加）。`why=ok` 只说"这一发跑成了"，
            # 四格说"升了什么、解了什么"；读不出来时是 -1（`parse_anomaly`）。
            if "contradiction_raised" in r:
                extra += " contradiction={}/{} ghosts={}/{}".format(
                    r["contradiction_raised"], r["contradiction_resolved"],
                    r["ghosts_raised"], r["ghosts_resolved"])
            # 备份档（W28）：`hub backup` 的四格在 phase 行里同名同层，附在最后（同一律：只追加）。
            # 读不出来时是 -1（`parse_backup`），与 `why=ok` 说的"这一发跑成了"是两件事。
            if "memory_rows" in r:
                extra += " name={} memory={} memory_rows={} extras={}".format(
                    r.get("name"), r.get("memory"), r["memory_rows"], r.get("extras"))
            phase_lines.append("phase    {:7s} pass={:7s} family={:9s} calls={:>3} why={} used={}{}".format(
                r.get("kind") or "", r.get("pass") or "", r.get("family") or "",
                r.get("calls"), r.get("why"), r.get("used"), extra))
        elif kind == "fatal":
            print("FATAL", json.dumps(r, ensure_ascii=False)[:200])
        elif kind == "unparsed":
            print("UNPARSED", r["line"][:120])

    if start:
        # `episode_calls_cap` 是 episode 归组档的发数保险（不是额度），附在最后照印。
        print("start={} hub={} llm_timeout_ms={} episode_calls_cap={}".format(
            start.get("start"), start.get("hub"), start.get("llm_timeout_ms"),
            start.get("episode_calls_cap")))
    for line in phase_lines:
        print(line)
    for key, b in buckets.items():
        kind, pas, fam = key
        tail = " ".join(b["note"][-6:])
        print(f"{kind:8s} pass={pas:7s} family={fam:9s} calls={b['calls']:3d} "
              f"work={b['work']:4d} bad_rc={len(b['rc'])} {tail}")
        if "remaining" in b:
            print(f"          remaining_pending_review={b['remaining']}")
    if snapshot:
        for row in snapshot.get("rows", []):
            print("state  ", row)
    if cold:
        # 冷备（W28）：一行，放在 `end` 之前——它属于"这一夜留下的东西"，不是一次调用。
        # 失败那一行的两个文件名是 JSON `null`（脚本没交回名字），照印成 `null`，不念成空串。
        tail = ""
        if "error" in cold:
            tail += " error={}".format(cold.get("error"))
        print("cold_backup ledger={} core={} pruned={}{}".format(
            "null" if cold.get("ledger") is None else cold.get("ledger"),
            "null" if cold.get("core") is None else cold.get("core"),
            cold.get("pruned"), tail))
    if end:
        # 只追加一格：这一夜因重试耗尽隔离过几发（抽取各档的合计；口径见上面 `phase` 那一段）。
        tail = ""
        if "quarantined" in end:
            tail += " quarantined={}".format(end["quarantined"])
        print("end={} used={} fails={}{}".format(
            end.get("end"), end.get("used"), end.get("fails"), tail))
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
