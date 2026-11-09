#!/bin/bash
# 水位闸（主触发，W25）：**有待办才跑 `daily-drain.sh`；没有就零输出退出 0。**
#
# 形状：每 10 分钟被计划任务叫一次（任务本身不在本仓，控制器部署），问一次 `hub pending`，
# 只读四组水位格。待办 = 下面五个数之和 > 0：
#   extract.pending_passes（排队的 pass 格）
#   extract.lag_turns     （还没蒸完的可抽取轮）
#   review.pending        （等审的条数）
#   review.quarantined    （被隔离的条数）
#   derive.lag_rows       （推导池里游标之后的余量）
# `extract.running_passes` **不进这个和**：它是"已经有进程在跑"，不是"还有活没人干"；
# 真要有活，同一时刻的 `lag_turns` 本来就不为 0。
#
# 三条纪律（它们比这份脚本短，别删）：
# 1. **读不出来不等于没有待办**：`hub pending` 非零、或读数解析不出来 ⇒ 退出码 3，**不**跑抽干，
#    也**不**静默退出 0。把"读不出来"念成"没事可做"就是这道闸最坏的失败形状——它会让整夜
#    一次都不跑，而日志里只有一个 `exit 0`。
# 2. **没有待办就零输出**：一行都不写（不写日志、不写 stdout）。10 分钟一次的闸若每次都留一行，
#    一天 144 行噪声，真正的触发行就淹了。
# 3. **「有待办」时写一行 `{"t":"trigger","pending":{...}}` 到 `$LOG`**，再 `exec` 日脚本
#    （`exec` 而不是子进程：抽干要独占这一段，闸本身没有收尾动作）。
set -u

DATA=${RECOLLECT_DATA_DIR:-${HOME}/.recollect/data}
# 二进制来源与 `daily-drain.sh` 逐字同一条（按 mtime 取最新装的那一个；要钉死用 RECOLLECT_HUB）。
HUB=${RECOLLECT_HUB:-$(ls -1t "$DATA"/bin/hub-* 2>/dev/null | head -1)}
LOG=${RECOLLECT_LOG:-$DATA/logs/daily-$(date +%F).jsonl}

# Python 解释器：优先 `RECOLLECT_PYTHON`，否则在 PATH 里找可用的 python3 / python。
# 不写死安装路径——那是某台机器的状态，不是项目的依赖。
if [ -n "${RECOLLECT_PYTHON:-}" ]; then
  PY="$RECOLLECT_PYTHON"
else
  PY=""
  for cand in python3 python; do
    if "$cand" -c 'pass' >/dev/null 2>&1; then PY="$cand"; break; fi
  done
fi
if [ -z "$PY" ] || ! "$PY" -c 'pass' >/dev/null 2>&1; then
  printf 'error: no usable python (set RECOLLECT_PYTHON or put python3 on PATH)\n' >&2
  exit 2
fi

# 一行一个事件，且必须是**一行**（与 `daily-drain.sh` 的 `log()` 同一条守卫：带换行的载荷不落）。
log() {
  [ -n "$1" ] || return 0
  case $1 in
    *$'\n'*) return 0 ;;
  esac
  mkdir -p "$(dirname "$LOG")"
  printf '%s\n' "$1" >> "$LOG"
}

if [ $# -gt 0 ]; then
  printf 'usage: %s   （没有参数：闸的输入全来自 RECOLLECT_DATA_DIR / RECOLLECT_HUB / RECOLLECT_LOG）\n' \
    "$(basename "$0")" >&2
  exit 2
fi

# 读数：stdout 只取第一行（`hub` 的"读数一行 + error 一行"形状下，整体解析会把失败读成噪声）。
# stdout 单独收集、stderr 原样流过（运维看得见 hub 的 `error:` 行），两者不混。
out=$("$HUB" pending --json --data-dir "$DATA")
rc=$?
if [ "$rc" -ne 0 ]; then
  printf 'error: hub pending failed (rc=%s) — not treating a failed read as "nothing to do"\n' "$rc" >&2
  exit 3
fi

# 五格求和 + 一份给日志的紧凑 JSON，一行打出两个字段：`<total> <json>`。
parsed=$(printf '%s' "$out" | "$PY" -c 'import sys, json
try:
    head = sys.stdin.read().strip().split("\n", 1)[0]
    d = json.loads(head)
    e, r, dv = d["extract"], d["review"], d["derive"]
    keys = {
        "pending_passes": e["pending_passes"],
        "lag_turns": e["lag_turns"],
        "review_pending": r["pending"],
        "quarantined": r["quarantined"],
        "derive_lag_rows": dv["lag_rows"],
    }
    for k, v in keys.items():
        # bool 是 int 的子类：真值不许冒充计数。
        if not isinstance(v, int) or isinstance(v, bool):
            raise ValueError(k)
    print(sum(keys.values()), json.dumps(keys, separators=(",", ":"), sort_keys=True))
except Exception:
    pass' 2>/dev/null)

total=${parsed%% *}
payload=${parsed#* }
# 解析器没跑起来 / 交回空串 / 交回非数字：全都是"读不出来"，一律退出 3（见文件头纪律 1）。
case ${total:-} in
  ''|*[!0-9]*) printf 'error: hub pending output is not readable — refusing to guess\n' >&2; exit 3 ;;
esac

if [ "$total" -le 0 ]; then
  exit 0
fi

log "{\"t\":\"trigger\",\"pending\":$payload}"

# A1（W32 止血）：五个 pass 各 reclaim 一次，把卡死的 `running` 残行退回 `pending`，
# 再 exec 日脚本。为什么在这里：选批对 `running` 不重入，`reclaim_running` 此前只有手动
# CLI（`hub distill reclaim`），进程在 `mark_running` 之后死掉的那一行会永久卡死——
# 日路径是唯一无人值守的常开入口，闸放行（有待办）之后、抽干开跑之前是它唯一该出现的时刻。
#
# 参数与 `crates/hub/src/main.rs` 的 `DistillReclaimArgs` 逐字对上：`--pass` 只收五个
# pass 名（`rc_ledger::pass_state::PASS_IDS` 同一份名单，这里是它的第三处镜像），
# `--older-than-ms 1800000` = 30 分钟闸——比单发墙钟 300s 大一个量级，活进程还在跑的
# `running` 行不会被误收。`--json` 与本脚本读 `hub pending` 同一形状（失败时那行原文
# 交回 stderr，由操作员直接看，不进日志——`out` 里可能带任意字符，不经转义拼进 JSON
# 会把"一行一个合法 JSON"那条形状破掉）。
#
# 三个"不"：**不**让 reclaim 的失败拦住抽干（它是兜底，收不回来这一轮照跑，残行下一轮
# 再收）；**不**在成功时写日志（五个 pass 全 0 回收也是常态，逐 pass 记一行会把触发行
# 淹掉，与文件头纪律 2 同一条取向）；**不**手改 ledger（改行的唯一正道是 `reclaim_running`）。
RECLAIM_OLDER_MS=${RECOLLECT_RECLAIM_OLDER_MS:-1800000}
for pass in naming fact event state time; do
  out=$("$HUB" distill reclaim --pass "$pass" --older-than-ms "$RECLAIM_OLDER_MS" \
        --data-dir "$DATA" --json 2>&1); rc=$?
  if [ "$rc" -ne 0 ]; then
    # 失败的原文先原样流给 stderr（hub 的 `error:` 行），再落一行只有 rc 的事件。
    printf '%s\n' "$out" >&2
    log "{\"t\":\"reclaim\",\"pass\":\"$pass\",\"rc\":$rc}"
  fi
done

# 环境交给日脚本：闸读的那一套必须与真正干活的子进程一致（否则闸与抽干各看各的库）。
export RECOLLECT_DATA_DIR=$DATA
export RECOLLECT_HUB=$HUB
export RECOLLECT_LOG=$LOG
exec "$(dirname "$0")/daily-drain.sh"
