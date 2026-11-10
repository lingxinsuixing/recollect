#!/bin/bash
# `collect-qoder.sh` 的离线桩闸（W27）：不碰真库、不跑真转换器、不启模型。
#
# 钉的是五件事（派发档 §2-3）：
#   ① **新文件 ⇒ 采一次**：一行 `{"t":"ingest",…}`，hub 被叫一次，状态文件里出现该文件的水位；
#   ② **同 mtime/size 再跑 ⇒ 不处理**：一个 ingest 行都没有，hub 一次都不再叫；
#   ③ **文件增长 ⇒ 再处理一次**：水位里的 `size` 变了，`new_turns` 只数新增的 human+agent 轮；
#   ④ **日帽=1 ⇒ 第二个文件不再处理**：`capped:true`，第二个文件连 hub 都没碰；
#   ⑤ **hub 非零 ⇒ 水位不更新**：只落 `{"t":"ingest_error",…}`，下一轮重试（hub 再被叫一次）。
# 另加四条本闸自己要的：单轮文件数帽、多目录（冒号分隔）、状态文件不可用 ⇒ 退出 3、
# 日志每行合法 JSON 且没有空行。
# A6 增量导入再钉三条：
#   ⑥ **同会话追加 ⇒ import 输入只含增量段**：v1 里恰好是追加的那几轮，dup=0，偏移推进到文件尾；
#   ⑦ **文件变小（截断/重写）⇒ 偏移归 0 整文件重转**：import 输入=重写后的整个文件；
#   ⑧ **半行不越过**：无换行的半行不产轮也不推进偏移，行写全后下一轮恰好补采那一轮。
#
# 用法：bash tools/pipeline/test_collect_qoder.sh
#   本机 `python3` 是 Microsoft Store 空壳时，先加桩目录：
#   PATH=/tmp/py3shim:$PATH bash tools/pipeline/test_collect_qoder.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
bash -n "$HERE/collect-qoder.sh" || { echo "红：collect-qoder.sh 语法不过"; exit 1; }

REAL_PYTHON=$(command -v python3 || true)
if [ -z "$REAL_PYTHON" ] || ! "$REAL_PYTHON" -c 'pass' >/dev/null 2>&1; then
  echo "红：本机 python3 不存在或不能跑（商店假身？），桩臂无从执行。修法：PATH 前置指向真 python 的 python3 shim"
  exit 1
fi
export REAL_PYTHON

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
STUB="$WORK/stub"
mkdir -p "$STUB" "$WORK/data" "$WORK/logs"

# ---- 桩转换器：源行 `{"role":…,"body":…}` 原样变 v1 的 turns -------------------------------
# 真实转换器把 Qoder 的 content 块摊成 human/agent/system 三种 role；这一支只保留这个形状，
# 轮数的来源就是源文件的行数 ⇒ 增长/不增长可以逐轮数出来。
cat > "$STUB/conv_stub.py" <<'CONV'
import json, os, sys
args = sys.argv[1:]
path = args[0]
out_dir = args[args.index("--out-dir") + 1]
if os.environ.get("STUB_CONV_FAIL") == "1":
    print("stub converter: boom", file=sys.stderr)
    sys.exit(1)
turns = []
with open(path, encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            d = json.loads(line)
        except Exception:
            # 与真转换器同一条守卫：坏行（写一半的 JSONL）按行跳过，不产轮。
            continue
        turns.append({"role": d["role"], "body": d.get("body", "x"), "created_at_ms": 0})
doc = {"format_version": 1, "sessions": [{"session_key": "stub", "device_id": "stub",
                                          "length": len(turns), "turns": turns}]}
os.makedirs(out_dir, exist_ok=True)
stem = os.path.splitext(os.path.basename(path))[0]
with open(os.path.join(out_dir, stem + ".v1.json"), "w", encoding="utf-8") as f:
    json.dump(doc, f)
print(json.dumps({"turns_out": len(turns)}))
CONV

# ---- 桩 hub：记录每次调用的参数，按固定读数回 ----------------------------------------------
# 四个档：正常 / 非零 / 退出 0 但交回非 JSON / 读数缺格。后两档钉的是"解析不出来也算失败"，
# 它们与 `hub 非零` 走同一条"不更新水位"的路（水位不可信时不许推进）。
cat > "$STUB/hub" <<'HUB'
#!/bin/bash
printf '%s\n' "$*" >> "$STUB_HUB_CALLS"
# A6：留一份本次 import 输入（v1）的副本，臂 9/10/11 靠它断言"输入只含增量段/整文件/0 轮"。
if [ -n "${STUB_HUB_LAST:-}" ] && [ -n "${2:-}" ]; then
  cp "$2" "$STUB_HUB_LAST"
fi
case ${STUB_HUB_MODE:-ok} in
  fail) printf 'error: store unreachable\n' >&2; exit 3 ;;
  garbage) printf 'Python was not found; run without arguments to install from the Microsoft Store\n'; exit 0 ;;
  short) printf '{"schema_version":1,"turns_accepted":4}\n'; exit 0 ;;
esac
printf '{"schema_version":1,"turns_accepted":%s,"turns_rejected":0,"skipped_duplicate":%s,"tombstoned":0,"batch_reject":"","rejects":[],"redacted_spans":0}\n' \
  "${STUB_HUB_ACCEPTED:-4}" "${STUB_HUB_DUP:-0}"
HUB
chmod +x "$STUB/hub"

fail=0
red() { echo "红：$*"; fail=1; }

# 造一个会话文件：参数是 human/agent/system 三种 role 的个数。
mkfile() { # <path> <human> <agent> <system> [mtime]
  local path=$1 h=$2 a=$3 s=$4 mtime=${5:-}
  : > "$path"
  local i
  for ((i = 0; i < h; i++)); do printf '{"role":"human","body":"h%s"}\n' "$i" >> "$path"; done
  for ((i = 0; i < a; i++)); do printf '{"role":"agent","body":"a%s"}\n' "$i" >> "$path"; done
  for ((i = 0; i < s; i++)); do printf '{"role":"system","body":"s%s"}\n' "$i" >> "$path"; done
  [ -n "$mtime" ] && touch -d "$mtime" "$path"
  return 0
}

# 跑一次采集；环境逐臂给（状态/日志/帽/桩全隔离，臂与臂之间不串味）。
run_collect() { # <watch> <state> <log> <cap> <maxfiles> [hub 模式] [转换器失败] [额外目录]
  local watch=$1 state=$2 log=$3 cap=$4 maxf=$5 mode=${6:-ok} convfail=${7:-0}
  STUB_HUB_CALLS="$WORK/hub-calls" STUB_HUB_MODE=$mode STUB_CONV_FAIL=$convfail \
  STUB_HUB_LAST="$WORK/hub-last.json" \
  RECOLLECT_DATA_DIR="$WORK/data" RECOLLECT_HUB="$STUB/hub" RECOLLECT_INGEST_DIRS="$watch" \
  RECOLLECT_INGEST_STATE="$state" RECOLLECT_LOG="$log" \
  RECOLLECT_INGEST_CONVERTER="$STUB/conv_stub.py" \
  RECOLLECT_INGEST_DAILY_TURNS="$cap" RECOLLECT_INGEST_FILES_PER_RUN="$maxf" \
    bash "$HERE/collect-qoder.sh"
}

count_hub_calls() { [ -f "$WORK/hub-calls" ] && wc -l < "$WORK/hub-calls" | tr -d ' ' || echo 0; }
# 日志里某个 t 的行数（`grep -c` 零命中时自己就打了 0，别再补一个）。
count_t() {
  [ -f "$2" ] || { echo 0; return; }
  grep -c "\"t\":\"$1\"" "$2"
}
# 从日志里取某一行里的一个数字格。
field() { sed -n "s/.*\"$2\":\([0-9]*\).*/\1/p" "$1" | head -1; }
# 状态文件里某文件的水位（取 last_turns）；没有就交回空。
# 键按脚本同一条规范化算（abspath + 正斜杠）：git bash 会把 `/tmp/…` 转成 `C:/…`，
# 两边必须落在同一个字符串上，否则这一格永远读空。
state_last() { # <state> <path>
  "$REAL_PYTHON" - "$1" "$2" <<'PY'
import json, os, sys
try:
    key = os.path.abspath(sys.argv[2]).replace(os.sep, "/")
    with open(sys.argv[1], encoding="utf-8") as f:
        doc = json.load(f)
    row = (doc.get("files") or {}).get(key)
    print("" if not isinstance(row, dict) else row.get("last_turns"))
except Exception:
    print("")
PY
}
state_used() { # <state> <today>
  "$REAL_PYTHON" - "$1" "$(date +%F)" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        doc = json.load(f)
    print((doc.get("daily") or {}).get(sys.argv[2], {}).get("used"))
except Exception:
    print("")
PY
}
state_off() { # <state> <path> —— 水位里的字节偏移（A6）；没有就交回空。
  "$REAL_PYTHON" - "$1" "$2" <<'PY'
import json, os, sys
try:
    key = os.path.abspath(sys.argv[2]).replace(os.sep, "/")
    with open(sys.argv[1], encoding="utf-8") as f:
        doc = json.load(f)
    row = (doc.get("files") or {}).get(key)
    print("" if not isinstance(row, dict) else row.get("offset"))
except Exception:
    print("")
PY
}
# v1 里的轮次串（role:body 逗号相连；空会话交回空串）——臂 9/10/11 断言 import 输入形状用。
v1_turns() { # <v1json>
  "$REAL_PYTHON" - "$1" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        doc = json.load(f)
    turns = doc["sessions"][0]["turns"]
    print(",".join("%s:%s" % (t.get("role"), t.get("body")) for t in turns))
except Exception:
    print("")
PY
}

# ---- 臂 1/2/3：新文件 → 不重复 → 增长 -------------------------------------------------------
echo "== 臂 1：新文件 ⇒ 采一次（一行 ingest + hub 叫一次 + 水位落盘）=="
W1="$WORK/s1"; S1="$WORK/state1.json"; L1="$WORK/log1.jsonl"
mkdir -p "$W1"; mkfile "$W1/s1.jsonl" 1 1 2 '2026-11-01 10:00'
: > "$WORK/hub-calls"
out=$(run_collect "$W1" "$S1" "$L1" 300 3)
printf '%s\n' "$out"
[ "$(count_t ingest "$L1")" = 1 ] || red "臂1：应恰好一行 ingest，实测 $(count_t ingest "$L1")"
[ "$(count_t ingest_error "$L1")" = 0 ] || red "臂1：不该有 ingest_error"
[ "$(count_hub_calls)" = 1 ] || red "臂1：hub 应被叫 1 次，实测 $(count_hub_calls)"
line=$(grep '"t":"ingest"' "$L1" | head -1)
for k in file turns accepted dup rejected new_turns used; do
  printf '%s' "$line" | grep -q "\"$k\":" || red "臂1：ingest 行缺 $k 那一格：$line"
done
printf '%s' "$line" | grep -q '"file":"s1.jsonl"' || red "臂1：file 应是 basename：$line"
[ "$(field "$L1" turns)" = 4 ] || red "臂1：turns 应是 4（1h+1a+2s），实测 $(field "$L1" turns)"
[ "$(field "$L1" new_turns)" = 2 ] || red "臂1：new_turns 应是 2（human+agent），实测 $(field "$L1" new_turns)"
[ "$(state_last "$S1" "$W1/s1.jsonl")" = 2 ] || red "臂1：水位 last_turns 应是 2，实测 $(state_last "$S1" "$W1/s1.jsonl")"
[ "$(state_used "$S1")" = 2 ] || red "臂1：daily.used 应是 2，实测 $(state_used "$S1")"
grep -q '"t":"ingest_summary"' "$L1" || red "臂1：缺 ingest_summary 行"
grep -q '"capped":false' "$L1" || red "臂1：没到帽时 capped 应是 false"

echo "== 臂 2：同 mtime/size 再跑 ⇒ 不处理（零 ingest、hub 不再叫）=="
before=$(count_hub_calls)
out=$(run_collect "$W1" "$S1" "$L1" 300 3)
printf '%s\n' "$out"
# 这一臂只判"新跑出来的那一段"：日志是追加的，数总行会把臂 1 的行也算进来。
[ "$(count_hub_calls)" = "$before" ] || red "臂2：水位没变却又叫了 hub（$(count_hub_calls) > $before）"
printf '%s\n' "$out" | grep -q '"t":"ingest"' && red "臂2：水位没变却落了 ingest 行"
printf '%s\n' "$out" | grep -q '"files":0' || red "臂2：summary 的 files 应是 0"
[ "$(state_last "$S1" "$W1/s1.jsonl")" = 2 ] || red "臂2：水位被改动了"

echo "== 臂 3：文件增长 ⇒ 再处理一次（new_turns 只数新增的 2 轮）=="
printf '{"role":"human","body":"h2"}\n{"role":"agent","body":"a2"}\n' >> "$W1/s1.jsonl"
before=$(count_hub_calls)
out=$(run_collect "$W1" "$S1" "$L1" 300 3)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = $((before + 1)) ] || red "臂3：增长后应再叫一次 hub"
printf '%s\n' "$out" | grep -q '"new_turns":2' || red "臂3：new_turns 应是 2（新增 1h+1a），实测：$out"
[ "$(state_last "$S1" "$W1/s1.jsonl")" = 4 ] || red "臂3：水位 last_turns 应推进到 4，实测 $(state_last "$S1" "$W1/s1.jsonl")"
[ "$(state_used "$S1")" = 4 ] || red "臂3：daily.used 应累计到 4，实测 $(state_used "$S1")"

# ---- 臂 4：日帽=1 ⇒ 第二个文件不再处理 ------------------------------------------------------
echo "== 臂 4：日帽=1 ⇒ 只处理第一个文件，第二个连 hub 都不碰（capped:true）=="
W4="$WORK/s4"; S4="$WORK/state4.json"; L4="$WORK/log4.jsonl"
mkdir -p "$W4"
mkfile "$W4/old.jsonl" 1 1 0 '2026-11-01 09:00'
mkfile "$W4/new.jsonl" 1 1 0 '2026-11-01 11:00'
: > "$WORK/hub-calls"
out=$(run_collect "$W4" "$S4" "$L4" 1 3)
printf '%s\n' "$out"
[ "$(count_t ingest "$L4")" = 1 ] || red "臂4：日帽=1 时应只处理 1 个文件，实测 $(count_t ingest "$L4")"
[ "$(count_hub_calls)" = 1 ] || red "臂4：第二个文件不该碰 hub，实测 hub 叫了 $(count_hub_calls) 次"
grep -q '"capped":true' "$L4" || red "臂4：summary 里 capped 应是 true"
[ "$(state_last "$S4" "$W4/new.jsonl")" = "" ] || red "臂4：第二个文件不该有水位"
[ "$(state_last "$S4" "$W4/old.jsonl")" = 2 ] || red "臂4：第一个文件的水位应正常落盘"
# 帽值按日期记：state.daily[today].cap 必须是这一轮给的那个数（1），不是默认 300。
"$REAL_PYTHON" - "$S4" <<'PY' || red "臂4：daily 段里的 cap 不是本轮生效的帽值"
import json, sys
doc = json.load(open(sys.argv[1], encoding="utf-8"))
row = list((doc.get("daily") or {}).values())[0]
sys.exit(0 if row.get("cap") == 1 and row.get("used") == 2 else 1)
PY

# ---- 臂 5：hub 非零 / 读数坏 ⇒ 不更新水位 ---------------------------------------------------
echo "== 臂 5：hub 非零 ⇒ 只落 ingest_error、水位不更新；下一轮重试成功 =="
W5="$WORK/s5"; S5="$WORK/state5.json"; L5="$WORK/log5.jsonl"
mkdir -p "$W5"; mkfile "$W5/f.jsonl" 1 1 0 '2026-11-01 10:00'
: > "$WORK/hub-calls"
out=$(run_collect "$W5" "$S5" "$L5" 300 3 fail)
printf '%s\n' "$out"
[ "$(count_t ingest "$L5")" = 0 ] || red "臂5：hub 失败时不该落 ingest 行"
[ "$(count_t ingest_error "$L5")" = 1 ] || red "臂5：应落一行 ingest_error"
grep -q '"stage":"hub"' "$L5" || red "臂5：error 行的 stage 应是 hub"
[ "$(state_last "$S5" "$W5/f.jsonl")" = "" ] || red "臂5：hub 失败时不许更新水位"
out=$(run_collect "$W5" "$S5" "$L5" 300 3 ok)
printf '%s\n' "$out"
[ "$(count_t ingest "$L5")" = 1 ] || red "臂5：失败过的文件下一轮必须重试（没有 ingest 行就是水位被推进了）"
[ "$(count_hub_calls)" = 2 ] || red "臂5：hub 应被叫 2 次（失败 1 + 重试 1），实测 $(count_hub_calls)"

echo "== 臂 5b：读数解析不出来（非 JSON / 缺格）⇒ 同样不更新水位 =="
for mode in garbage short; do
  S5b="$WORK/state5b-$mode.json"; L5b="$WORK/log5b-$mode.jsonl"
  : > "$WORK/hub-calls"
  out=$(run_collect "$W5" "$S5b" "$L5b" 300 3 "$mode")
  [ "$(count_t ingest_error "$L5b")" = 1 ] || red "臂5b($mode)：读数坏时应落 ingest_error"
  grep -q '"stage":"hub_parse"' "$L5b" || red "臂5b($mode)：stage 应是 hub_parse"
  [ "$(state_last "$S5b" "$W5/f.jsonl")" = "" ] || red "臂5b($mode)：读数坏时不许更新水位"
done

echo "== 臂 5c：转换失败 ⇒ 不更新水位、错误行 stage=convert =="
S5c="$WORK/state5c.json"; L5c="$WORK/log5c.jsonl"
out=$(run_collect "$W5" "$S5c" "$L5c" 300 3 ok 1)
printf '%s\n' "$out"
[ "$(count_t ingest_error "$L5c")" = 1 ] || red "臂5c：转换失败应落 ingest_error"
grep -q '"stage":"convert"' "$L5c" || red "臂5c：stage 应是 convert"
[ "$(state_last "$S5c" "$W5/f.jsonl")" = "" ] || red "臂5c：转换失败时不许更新水位"

# ---- 臂 6：单轮文件数帽 --------------------------------------------------------------------
echo "== 臂 6：单轮文件数帽=2 ⇒ 只碰 2 个（capped:true），下一轮接着采第 3 个 =="
W6="$WORK/s6"; S6="$WORK/state6.json"; L6="$WORK/log6.jsonl"
mkdir -p "$W6"
mkfile "$W6/a.jsonl" 1 0 0 '2026-11-01 09:00'
mkfile "$W6/b.jsonl" 1 0 0 '2026-11-01 09:30'
mkfile "$W6/c.jsonl" 1 0 0 '2026-11-01 10:00'
: > "$WORK/hub-calls"
out=$(run_collect "$W6" "$S6" "$L6" 300 2)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = 2 ] || red "臂6：文件数帽=2 时 hub 应只被叫 2 次，实测 $(count_hub_calls)"
grep -q '"capped":true' "$L6" || red "臂6：撞上文件数帽时 capped 应是 true"
[ "$(state_last "$S6" "$W6/c.jsonl")" = "" ] || red "臂6：第 3 个文件不该被采"
out=$(run_collect "$W6" "$S6" "$L6" 300 2)
printf '%s\n' "$out"
[ "$(state_last "$S6" "$W6/c.jsonl")" = 1 ] || red "臂6：下一轮必须接着采第 3 个文件"

# ---- 臂 7：多目录（冒号分隔）--------------------------------------------------------------
echo "== 臂 7：RECOLLECT_INGEST_DIRS 冒号分隔两目录 ⇒ 两个都采 =="
W7a="$WORK/s7a"; W7b="$WORK/s7b"; S7="$WORK/state7.json"; L7="$WORK/log7.jsonl"
mkdir -p "$W7a" "$W7b"
mkfile "$W7a/x.jsonl" 1 1 0 '2026-11-01 08:00'
mkfile "$W7b/y.jsonl" 1 1 0 '2026-11-01 08:30'
: > "$WORK/hub-calls"
out=$(run_collect "$W7a:$W7b" "$S7" "$L7" 300 5)
printf '%s\n' "$out"
[ "$(count_t ingest "$L7")" = 2 ] || red "臂7：两个目录应各采一个，实测 $(count_t ingest "$L7")"
grep -q '"file":"x.jsonl"' "$L7" || red "臂7：缺 x.jsonl"
grep -q '"file":"y.jsonl"' "$L7" || red "臂7：缺 y.jsonl"

# ---- 臂 8：状态文件不可用 ⇒ 退出 3（不猜水位）-----------------------------------------------
echo "== 臂 8：状态文件是个目录 ⇒ 退出 3、不采任何文件 =="
S8="$WORK/state8-dir"; mkdir -p "$S8"; L8="$WORK/log8.jsonl"
: > "$WORK/hub-calls"
run_collect "$W1" "$S8" "$L8" 300 3 > "$WORK/out8" 2> "$WORK/err8"
rc=$?
printf 'rc=%s stderr=%s\n' "$rc" "$(cat "$WORK/err8")"
[ "$rc" = 3 ] || red "臂8：状态文件不可用应退出 3，实测 $rc"
[ "$(count_hub_calls)" = 0 ] || red "臂8：状态不可用时不许采任何文件"
[ -s "$L8" ] && red "臂8：状态不可用时不该落事件行"

# ---- 臂 9/10/11：A6 增量导入 ----------------------------------------------------------------
echo "== 臂 9：同会话追加 2 行 ⇒ import 输入只含增量段、dup=0、偏移推进到文件尾 =="
W9="$WORK/s9"; S9="$WORK/state9.json"; L9="$WORK/log9.jsonl"
mkdir -p "$W9"; mkfile "$W9/f.jsonl" 1 1 1 '2026-11-01 10:00'
: > "$WORK/hub-calls"
out=$(run_collect "$W9" "$S9" "$L9" 300 3)
printf '%s\n' "$out"
[ "$(count_t ingest "$L9")" = 1 ] || red "臂9：首采应一行 ingest，实测 $(count_t ingest "$L9")"
[ "$(field "$L9" new_turns)" = 2 ] || red "臂9：首采 new_turns 应是 2，实测 $(field "$L9" new_turns)"
sz1=$(wc -c < "$W9/f.jsonl" | tr -d ' ')
[ "$(state_off "$S9" "$W9/f.jsonl")" = "$sz1" ] || red "臂9：首采后 offset 应等于文件字节数（$sz1），实测 $(state_off "$S9" "$W9/f.jsonl")"
rm -f "$WORK/hub-last.json"
printf '{"role":"human","body":"h2"}\n{"role":"agent","body":"a2"}\n' >> "$W9/f.jsonl"
touch -d '2026-11-01 10:05' "$W9/f.jsonl"
before=$(count_hub_calls)
out=$(run_collect "$W9" "$S9" "$L9" 300 3)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = $((before + 1)) ] || red "臂9：追加后应再叫一次 hub"
[ -f "$WORK/hub-last.json" ] || red "臂9：桩 hub 没留下 import 输入副本"
seg_v1=$(v1_turns "$WORK/hub-last.json")
[ "$seg_v1" = "human:h2,agent:a2" ] || red "臂9：import 输入应只含增量段（human:h2,agent:a2），实测：$seg_v1"
printf '%s\n' "$out" | grep -q '"dup":0' || red "臂9：增量采的 ingest 行 dup 应是 0"
printf '%s\n' "$out" | grep -q '"new_turns":2' || red "臂9：增量采 new_turns 应是 2，实测：$out"
[ "$(state_last "$S9" "$W9/f.jsonl")" = 4 ] || red "臂9：last_turns 应累计到 4，实测 $(state_last "$S9" "$W9/f.jsonl")"
sz2=$(wc -c < "$W9/f.jsonl" | tr -d ' ')
[ "$(state_off "$S9" "$W9/f.jsonl")" = "$sz2" ] || red "臂9：offset 应推进到新文件尾（$sz2），实测 $(state_off "$S9" "$W9/f.jsonl")"

echo "== 臂 10：文件变小（截断/重写）⇒ 偏移归 0 整文件重转 =="
mkfile "$W9/f.jsonl" 1 0 0 '2026-11-01 11:00'
rm -f "$WORK/hub-last.json"
before=$(count_hub_calls)
out=$(run_collect "$W9" "$S9" "$L9" 300 3)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = $((before + 1)) ] || red "臂10：截断后应整文件重转（hub 再叫一次）"
seg_v1=$(v1_turns "$WORK/hub-last.json")
[ "$seg_v1" = "human:h0" ] || red "臂10：重转输入应是重写后的整个文件（human:h0），实测：$seg_v1"
[ "$(state_last "$S9" "$W9/f.jsonl")" = 1 ] || red "臂10：last_turns 应重置为重写后的 1，实测 $(state_last "$S9" "$W9/f.jsonl")"
sz3=$(wc -c < "$W9/f.jsonl" | tr -d ' ')
[ "$(state_off "$S9" "$W9/f.jsonl")" = "$sz3" ] || red "臂10：重转后 offset 应等于新文件字节数（$sz3），实测 $(state_off "$S9" "$W9/f.jsonl")"
printf '%s\n' "$out" | grep -q '"new_turns":0' || red "臂10：重写后 human+agent 轮数少于旧水位，new_turns 应按 0 记，实测：$out"

echo "== 臂 11：半行不越过 ⇒ 偏移停在行边界，行写全后下一轮恰好补采那一轮 =="
W11="$WORK/s11"; S11="$WORK/state11.json"; L11="$WORK/log11.jsonl"
mkdir -p "$W11"; mkfile "$W11/g.jsonl" 1 1 0 '2026-11-01 10:00'
: > "$WORK/hub-calls"
run_collect "$W11" "$S11" "$L11" 300 3 > /dev/null
sz0=$(wc -c < "$W11/g.jsonl" | tr -d ' ')
[ "$(state_off "$S11" "$W11/g.jsonl")" = "$sz0" ] || red "臂11：首采后 offset 应在文件尾（$sz0），实测 $(state_off "$S11" "$W11/g.jsonl")"
printf '{"role":"human","body":"h9"' >> "$W11/g.jsonl"
touch -d '2026-11-01 10:05' "$W11/g.jsonl"
rm -f "$WORK/hub-last.json"
before=$(count_hub_calls)
out=$(run_collect "$W11" "$S11" "$L11" 300 3)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = $((before + 1)) ] || red "臂11：半行段应照常走一次 import（0 轮 v1）"
[ "$(v1_turns "$WORK/hub-last.json")" = "" ] || red "臂11：半行不该产轮，实测：$(v1_turns "$WORK/hub-last.json")"
[ "$(state_off "$S11" "$W11/g.jsonl")" = "$sz0" ] || red "臂11：偏移不得越过半行，实测 $(state_off "$S11" "$W11/g.jsonl")（应仍为 $sz0）"
[ "$(state_last "$S11" "$W11/g.jsonl")" = 2 ] || red "臂11：半行不产轮，last_turns 应仍是 2"
printf '}\n' >> "$W11/g.jsonl"
touch -d '2026-11-01 10:10' "$W11/g.jsonl"
rm -f "$WORK/hub-last.json"
before=$(count_hub_calls)
out=$(run_collect "$W11" "$S11" "$L11" 300 3)
printf '%s\n' "$out"
[ "$(count_hub_calls)" = $((before + 1)) ] || red "臂11：行写全后应再采一次"
[ "$(v1_turns "$WORK/hub-last.json")" = "human:h9" ] || red "臂11：行写全后增量段应恰好是补上的那一轮（human:h9），实测：$(v1_turns "$WORK/hub-last.json")"
[ "$(state_last "$S11" "$W11/g.jsonl")" = 3 ] || red "臂11：last_turns 应累计到 3，实测 $(state_last "$S11" "$W11/g.jsonl")"
sz4=$(wc -c < "$W11/g.jsonl" | tr -d ' ')
[ "$(state_off "$S11" "$W11/g.jsonl")" = "$sz4" ] || red "臂11：offset 应推进到文件尾（$sz4），实测 $(state_off "$S11" "$W11/g.jsonl")"

# ---- 日志形状：每行合法 JSON、无空行 -------------------------------------------------------
echo "== 日志形状：每行合法 JSON、无空行 =="
for lf in "$WORK"/log*.jsonl; do
  [ -f "$lf" ] || continue
  "$REAL_PYTHON" - "$lf" <<'PY' || fail=1
import json, sys
bad = 0
for i, raw in enumerate(open(sys.argv[1], encoding="utf-8"), 1):
    line = raw.rstrip("\n")
    if not line.strip():
        print("红：%s:%d 是空行" % (sys.argv[1], i)); bad += 1; continue
    try:
        json.loads(line)
    except Exception:
        print("红：%s:%d 不是合法 JSON：%s" % (sys.argv[1], i, line[:80])); bad += 1
sys.exit(1 if bad else 0)
PY
done

if [ "$fail" = 0 ]; then
  echo "采集闸：通过（新文件采一次 / 水位不变不重复 / 增长再采且只数新增 human+agent / 日帽与文件数帽各自收手 / hub 非零与转换失败都不推进水位 / 多目录 / 状态不可用退出 3 / 增量只送增量段且 dup=0 / 截断回退整文件 / 半行不越过行边界 / 日志每行合法 JSON）"
fi
exit $fail
