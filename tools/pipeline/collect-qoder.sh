#!/bin/bash
# W27 本地会话采集：Qoder 工作区 `*.jsonl` → v1 → `hub import`（**只写 transcript**）。
#
# 为什么要有这一条：`hub import` 只吃已转换的 v1，Qoder 的会话文件没有采集器，
# 于是现网 transcript 里只有 4 个手工导入的会话，新会话一轮都进不来——
# `hub pending` 恒 0，水位闸没活可干（W25 的闸只蒸**已入库**的轮次）。
#
# 顺序（由控制器的计划任务决定，本脚本不调 `daily-drain.sh`）：
#   collect-qoder.sh（本脚本）→ drain-if-pending.sh（有待办才抽干）
# 采集与抽干分离：采集是 I/O 活（零模型调用），抽干是模型活；混在一条脚本里会让
# "今天采了几轮"与"今天蒸了几轮"变成同一本账。
#
# 形状（每一步都可单独核对）：
#   ① 扫 `RECOLLECT_INGEST_DIRS`（冒号分隔多目录，默认 `$HOME/.qoder-cn/projects/D--recollect`）
#      顶层的 `*.jsonl`，只取 **mtime_ms 或 size 变了** 的文件（其余的文件这一轮一个字都不碰）；
#      按 mtime 升序（旧→新）处理——旧会话先入库，滞后窗口先收窄。
#   ② 每个文件（A6 增量导入）：只把 `[offset, EOF)` 的新增段切出来喂
#      `python3 tools/converters/qoder_v1.py <段文件> --out-dir <tmp>`（确定性转换，
#      同一输入逐字节同输出）→ `hub import <v1> --data-dir $DATA --json`。
#      转换器逐行无状态：session_key 取段内首条 user/assistant/system 记录的
#      sessionId（缺了才回落 basename；段文件沿用源 stem ⇒ 同键），轮次正文全部
#      内联在行内 ⇒ 增量段转出的 v1 不带前文也能独立过 import。
#   ③ 读 `turns_accepted` / `skipped_duplicate` / `turns_rejected` 落一行 `{"t":"ingest",…}`。
#
# 水位（`$DATA/ingest/qoder-state.json`，可用 `RECOLLECT_INGEST_STATE` 换路径）：
#   {"files": {"<绝对路径>": {"mtime_ms": N, "size": N, "last_turns": N, "offset": N}},
#    "daily": {"YYYY-MM-DD": {"cap": N, "used": N}}}
#   - `files` 是**逐文件水位**：只有 mtime_ms 与 size 都和记录一致才跳过。
#   - `offset`（A6）= 上次成功入库消费到的**字节偏移**，永远落在完整行边界：
#      段内最后一个 `\n` 之后；末行没有 `\n` 时仅当它整体是合法 JSON（完整但
#      未终止的行）才越过，半行留给下一轮（转换器按行跳坏行，半行本轮不产轮）。
#      推进只依据喂给转换器的段文件算（转了多少推进多少），不重读源文件 ⇒
#      转换与推进之间没有夹带窗口。旧状态文件没有这一格 ⇒ 按 0 处理，先整
#      文件转一次再记偏移（旧轮由 turn_uid 幂等吸收）。
#   - 文件变小（截断/重写）⇒ 偏移越界（> 当前 size）在扫描时归 0，整文件重转。
#      同长/变长的整文件重写探测不到（需要内容锚），已知边界。
#   - `last_turns` = 那个文件**累计**入库的 **human+agent 轮数**（不含 system）：
#      全文件路径（首采/截断回退）= 本次 v1 的 human+agent 轮数；增量路径 =
#      上次 + 增量段的。增量路径的"新增"= 增量段里的 human+agent 轮数；全
#      文件路径退化为 总数 − `last_turns`（负数按 0 记）。
#     这正是日轮帽的计量单位——帽数的是**新进来的对话轮**，不是 tool_call/tool_result 那种
#     system 轮（一个 13MB 的活动会话有 2404 个 system 轮，按总轮数计帽会一步打满）。
#   - `daily` 按日期记帽值与本日已用，跨多次运行累计（10 分钟一次的调度不会把帽重置）。
#
# 两道帽（都**不是预算**，是"一轮别把配额吃光、别把一天吃光"的界）：
#   - 日轮帽 `RECOLLECT_INGEST_DAILY_TURNS`（默认 300），按上面的"新增 human+agent 轮"计；
#     达到（`used >= cap`）就**不再处理更多文件**，已处理的正常记。`cap` 记进 `daily`。
#   - 单轮文件数帽 `RECOLLECT_INGEST_FILES_PER_RUN`（默认 3）：一轮最多碰几个文件（含失败的尝试）。
#
# 失败面（逐文件记，不拖垮整轮）：
#   转换失败 / 交不出 v1 / `hub import` 非零 / 读数解析不出来 ⇒ 落一行 `{"t":"ingest_error",…}`，
#   **不更新那个文件的水位**（下一轮重试），整轮照常跑完并退出 0。
#   退出码只有两种：0 = 跑完（哪怕每个文件都失败）；3 = **状态文件读不出/写不进**
#   （水位不可信时不猜：把"读不出来"念成"全部没采过"会把同一批会话重采一遍并打乱日帽；
#   这时也不更新任何水位，宁可这一轮什么都不做）。
set -u

DATA=${RECOLLECT_DATA_DIR:-${HOME}/.recollect/data}
# 与 `daily-drain.sh` / `drain-if-pending.sh` 逐字同一条取值：按 mtime 取最新装的那一个；
# 要钉死用 `RECOLLECT_HUB`。
HUB=${RECOLLECT_HUB:-$(ls -1t "$DATA"/bin/hub-* 2>/dev/null | head -1)}
WATCH=${RECOLLECT_INGEST_DIRS:-$HOME/.qoder-cn/projects/D--recollect}
STATE=${RECOLLECT_INGEST_STATE:-$DATA/ingest/qoder-state.json}
CAP=${RECOLLECT_INGEST_DAILY_TURNS:-300}
MAXFILES=${RECOLLECT_INGEST_FILES_PER_RUN:-3}
HERE=$(cd "$(dirname "$0")" && pwd)
CONVERTER=${RECOLLECT_INGEST_CONVERTER:-$HERE/../converters/qoder_v1.py}
LOG=${RECOLLECT_LOG:-$DATA/logs/ingest-$(date +%F).jsonl}
TODAY=$(date +%F)

# Python 解释器：优先 `RECOLLECT_PYTHON`，否则在 PATH 里找可用的 python3 / python。
# 不要写死安装路径（那是某台机器的状态，不是项目的依赖）。
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

# 用法错误（不是运行期失败）：退出 2，与 `daily-drain.sh` 的拼错旗标同一档。
case $CAP in ''|*[!0-9]*) printf 'error: RECOLLECT_INGEST_DAILY_TURNS is not a non-negative integer\n' >&2; exit 2 ;; esac
case $MAXFILES in ''|*[!0-9]*) printf 'error: RECOLLECT_INGEST_FILES_PER_RUN is not a non-negative integer\n' >&2; exit 2 ;; esac
if [ -z "${HUB:-}" ] || [ ! -e "$HUB" ]; then
  printf 'error: hub binary not found (RECOLLECT_HUB / %s/bin/hub-*)\n' "$DATA" >&2
  exit 2
fi
if [ ! -e "$CONVERTER" ]; then
  printf 'error: converter not found: %s\n' "$CONVERTER" >&2
  exit 2
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# 一行一个事件，且必须是**一行**（与 `daily-drain.sh` 的 `log()` 同一条守卫：带换行的载荷不落）。
# 同时打到 stdout 与按天的 JSONL：人读面看终端，机器读面按天归档（与抽干日志同一形状）。
emit() {
  [ -n "$1" ] || return 0
  case $1 in
    *$'\n'*) return 0 ;;
  esac
  printf '%s\n' "$1"
  mkdir -p "$(dirname "$LOG")" 2>/dev/null
  printf '%s\n' "$1" >> "$LOG"
}

# ---- ① 扫候选（mtime/size 变了的文件，旧→新）-----------------------------------------------
# 扫描与水位比较放在同一个 python 进程里：`stat` 的毫秒精度与"记录里是什么"必须对得上，
# 分两步做就会在两次 stat 之间出现"看到的 mtime 与比对用的 mtime 不是同一个"的空档。
scan=$("$PY" - "$STATE" "$WATCH" <<'PY'
import json, os, sys

state_path, watch = sys.argv[1], sys.argv[2]
doc = {}
if os.path.exists(state_path):
    try:
        with open(state_path, encoding="utf-8") as f:
            raw = f.read().strip()
        doc = json.loads(raw) if raw else {}
        if not isinstance(doc, dict):
            raise ValueError("state is not an object")
    except Exception as exc:
        print("error: state unreadable: %s" % (exc,), file=sys.stderr)
        sys.exit(3)
files_state = doc.get("files") or {}
if not isinstance(files_state, dict):
    print("error: state.files is not an object", file=sys.stderr)
    sys.exit(3)


def split_dirs(spec):
    """冒号分隔的多目录；**盘符里的冒号不是分隔符**。

    git bash 有两层改写（本机实测）：单个 `/c/…` 参数会变成 `C:/…`（盘符冒号出现在字符串里），
    而 `a:b` 这种冒号列表会变成 `C:\\…\\a;C:\\…\\b`（冒号变分号）。所以两种分隔符都认，
    并且把"单字母段 + 以 / 或 \\ 开头的下一段"拼回同一个路径。
    第一版只按 `:` 切，一个文件都采不到，日志里只有 summary——这就是这条守卫的由来。
    """
    if ";" in spec:
        return [p for p in spec.split(";") if p]
    parts = [p for p in spec.split(":") if p]
    out, i = [], 0
    while i < len(parts):
        p = parts[i]
        if i + 1 < len(parts) and len(p) == 1 and parts[i + 1][:1] in ("/", "\\"):
            out.append(p + ":" + parts[i + 1])
            i += 2
            continue
        out.append(p)
        i += 1
    return out


def norm(p):
    # 统一成正斜杠：`os.path.abspath` 在 Windows 交回 `C:\\Users\\…`，而下游的
    # `basename`/`stem` 与状态键都按 `/` 切；同一条路径两种写法会让水位永远对不上。
    return os.path.abspath(p).replace(os.sep, "/")


rows, seen = [], set()
for d in split_dirs(watch):
    if not os.path.isdir(d):
        continue
    try:
        names = sorted(os.listdir(d))
    except OSError:
        continue
    for name in names:
        if not name.endswith(".jsonl"):
            continue
        p = norm(os.path.join(d, name))
        if p in seen:
            continue
        try:
            st = os.stat(p)
        except OSError:
            continue
        if not os.path.isfile(p):
            continue
        seen.add(p)
        mt = st.st_mtime_ns // 1_000_000
        prev = files_state.get(p) or {}
        if not isinstance(prev, dict):
            prev = {}
        last = prev.get("last_turns")
        if not isinstance(last, int) or isinstance(last, bool) or last < 0:
            last = 0
        # A6：字节偏移。缺格 = 旧状态文件，按 0 处理（先整文件转一次再记偏移）；
        # 越界（> 当前 size）= 文件被截断/重写 ⇒ 归 0，整文件重转。
        off = prev.get("offset")
        if not isinstance(off, int) or isinstance(off, bool) or off < 0 or off > st.st_size:
            off = 0
        # 水位命中（mtime 与 size 都没变）⇒ 这一轮不碰它，也不打任何行。
        if prev.get("mtime_ms") == mt and prev.get("size") == st.st_size:
            continue
        rows.append((mt, st.st_size, p, last, off))
rows.sort()
for mt, size, p, last, off in rows:
    print("%d\t%d\t%s\t%d\t%d" % (mt, size, p, last, off))
PY
)
if [ $? -ne 0 ]; then
  printf 'error: ingest state is not usable (%s) — refusing to guess watermarks\n' "$STATE" >&2
  exit 3
fi
# Windows 上的 python 把 `\n` 写成 `\r\n`（文本模式 stdout）：不清掉的话最后一个字段会带着 `\r`
# 进算术式（`$((ha - last))` 报 invalid arithmetic operator）。整段清掉最省事：路径里不会有 `\r`。
scan=${scan//$'\r'/}

# 本日已用（跨运行累计）：只读，不在这里写回（写回只发生在成功入库之后）。
used_today=$("$PY" - "$STATE" "$TODAY" <<'PY'
import json, os, sys
p, today = sys.argv[1], sys.argv[2]
try:
    with open(p, encoding="utf-8") as f:
        raw = f.read().strip()
    doc = json.loads(raw) if raw else {}
except FileNotFoundError:
    doc = {}
except Exception:
    sys.exit(3)
daily = doc.get("daily") or {}
row = daily.get(today) or {}
used = row.get("used") if isinstance(row, dict) else 0
print(used if isinstance(used, int) and not isinstance(used, bool) and used >= 0 else 0)
PY
)
if [ $? -ne 0 ]; then
  printf 'error: ingest state is not usable (%s) — refusing to guess the daily cap\n' "$STATE" >&2
  exit 3
fi
used_today=${used_today//$'\r'/}

# ---- ② 逐文件：转换 → 导入 → 记水位 ---------------------------------------------------------
files=0
accepted_total=0
dup_total=0
new_turns_total=0
capped=0

# 水位写不进 = 这一轮的水位不可信 ⇒ 退出 3（这是唯一用 3 的一格）。
# 九个值全走 **argv**（不用环境变量）：git bash 只对命令行参数做 POSIX→Windows 路径转换，
# 环境变量原样透传 ⇒ 同一条路径在"读"与"写"两处会变成两个不同的文件（本机实测的形状）。
save_state() { # <path> <mt> <size> <last> <offset> <used>
  "$PY" -c 'import json, os, sys
sp, key, mt, size, last, off, today, cap, used = sys.argv[1:10]
try:
    with open(sp, encoding="utf-8") as f:
        raw = f.read().strip()
    doc = json.loads(raw) if raw else {}
    if not isinstance(doc, dict):
        raise ValueError
except FileNotFoundError:
    doc = {}
except Exception:
    sys.exit(3)
files = doc.get("files")
if not isinstance(files, dict):
    files = {}
daily = doc.get("daily")
if not isinstance(daily, dict):
    daily = {}
files[key] = {
    "mtime_ms": int(mt),
    "size": int(size),
    "last_turns": int(last),
    "offset": int(off),
}
daily[today] = {
    "cap": int(cap),
    "used": int(used),
}
doc = {"files": files, "daily": daily}
d = os.path.dirname(sp)
if d:
    os.makedirs(d, exist_ok=True)
tmp = sp + ".tmp-%d" % os.getpid()
try:
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1, sort_keys=True)
        f.write("\n")
    os.replace(tmp, sp)
except Exception:
    try:
        os.unlink(tmp)
    except OSError:
        pass
    sys.exit(3)' "$STATE" "$1" "$2" "$3" "$4" "$5" "$TODAY" "$CAP" "$6" 2>/dev/null
  if [ $? -ne 0 ]; then
    printf 'error: cannot write ingest state (%s) — watermarks are not trustworthy, stop here\n' "$STATE" >&2
    exit 3
  fi
}

while IFS=$'\t' read -r mt size path last off; do
  [ -n "${path:-}" ] || continue

  # 两道帽都在"要开始下一个文件"之前判：已处理的文件正常记完，帽一开就不碰后面任何一个。
  if [ "$files" -ge "$MAXFILES" ] || [ "$used_today" -ge "$CAP" ]; then
    capped=1
    break
  fi
  files=$((files + 1))

  base=$(basename "$path")
  stem=${base%.jsonl}
  conv_dir="$TMP/v1-$files"
  mkdir -p "$conv_dir"
  # A6 增量：只取 `[offset, EOF)` 的新增段喂转换器（`tail -c +N` 是 1 起，+offset+1
  # 恰好从 0 起的第 offset 字节开始）。段文件沿用源 stem：转换器在记录缺 sessionId
  # 时用 basename 推 session_key，同 stem ⇒ 增量段与全文件落在同一个 session_key 上。
  seg="$conv_dir/$stem.jsonl"
  tail -c +"$((off + 1))" "$path" > "$seg" 2>/dev/null
  if [ ! -s "$seg" ]; then
    # mtime 变了但字节没长（touch / 同长重写）：没有新增段，不转不导；
    # 只把 mtime/size 对齐进水位，否则这个文件每轮都被白扫一遍。
    save_state "$path" "$mt" "$size" "$last" "$off" "$used_today"
    emit "{\"t\":\"ingest\",\"file\":\"$base\",\"turns\":0,\"accepted\":0,\"dup\":0,\"rejected\":0,\"new_turns\":0,\"used\":$used_today,\"why\":\"no_new_bytes\"}"
    continue
  fi
  "$PY" "$CONVERTER" "$seg" --out-dir "$conv_dir" >"$TMP/conv.out" 2>"$TMP/conv.err"
  conv_rc=$?
  v1="$conv_dir/$stem.v1.json"
  if [ "$conv_rc" -ne 0 ] || [ ! -s "$v1" ]; then
    # 不更新水位：下一轮重试（会话文件在写时可能出现半行，见 README「会话采集」）。
    emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"convert\",\"rc\":$conv_rc}"
    continue
  fi

  # v1 里的真实轮数：`turns` = 全部轮，`ha` = human+agent 轮（日帽的计量单位）。
  counts=$("$PY" - "$v1" <<'PY' 2>/dev/null
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        doc = json.load(f)
    turns = doc["sessions"][0]["turns"]
    ha = sum(1 for t in turns if isinstance(t, dict) and t.get("role") in ("human", "agent"))
    print("%d %d" % (len(turns), ha))
except Exception:
    sys.exit(3)
PY
)
  if [ $? -ne 0 ] || [ -z "${counts:-}" ]; then
    emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"convert\",\"rc\":0}"
    continue
  fi
  counts=${counts//$'\r'/}
  total=${counts%% *}
  ha=${counts#* }
  # 两个数都必须是十进制计数（空串/带别的字符都不许进算术式猜一个 0 出来）。
  case ${total:-x}${ha:-x} in
    *[!0-9]*) emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"convert\",\"rc\":0}"; continue ;;
  esac

  hub_out=$("$HUB" import "$v1" --data-dir "$DATA" --json 2>"$TMP/hub.err")
  hub_rc=$?
  if [ "$hub_rc" -ne 0 ]; then
    emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"hub\",\"rc\":$hub_rc}"
    continue
  fi
  parsed=$(printf '%s' "$hub_out" | "$PY" -c 'import json, sys
raw = sys.stdin.read().strip().split("\n", 1)[0]
try:
    d = json.loads(raw)
    vals = [d["turns_accepted"], d["skipped_duplicate"], d["turns_rejected"]]
    for v in vals:
        # bool 是 int 的子类：真值不许冒充计数。
        if not isinstance(v, int) or isinstance(v, bool):
            raise ValueError
    print(vals[0], vals[1], vals[2])
except Exception:
    sys.exit(3)' 2>/dev/null)
  if [ $? -ne 0 ] || [ -z "${parsed:-}" ]; then
    emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"hub_parse\",\"rc\":0}"
    continue
  fi
  parsed=${parsed//$'\r'/}
  read -r accepted dup rejected <<<"$parsed"
  case ${accepted:-x}${dup:-x}${rejected:-x} in
    *[!0-9]*) emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"hub_parse\",\"rc\":0}"; continue ;;
  esac

  # A6：新增 human+agent 轮与 last_turns 推进。
  # 增量路径（off>0）：段 v1 里的 human+agent 轮就是全部新增，last_turns 累加；
  # 全文件路径（off=0，首采或截断回退）：沿用"总数 − last"（负数按 0 记，不倒退）。
  if [ "$off" -gt 0 ]; then
    delta=$ha
    new_last=$((last + ha))
  else
    delta=$((ha - last))
    [ "$delta" -lt 0 ] && delta=0
    new_last=$ha
  fi
  used_today=$((used_today + delta))

  # A6：新偏移只认**段文件**里的完整行——最后一个 `\n` 之后；末行没有 `\n` 时仅当
  # 它整体是合法 JSON（完整但未终止的行）才越过，半行留给下一轮。只依据段文件推进，
  # 不重读源文件 ⇒ "转了多少推进多少"，转换与推进之间没有夹带的窗口。
  # 顺带回读源文件当前 size 一起记：中途又追加了内容时 size 变化会让下一轮重扫，
  # 而偏移已经盖住已转过的字节；源文件中途被截断时记录的 size < 新偏移，下一轮
  # 扫描按越界归 0，整文件重转。
  mark=$("$PY" - "$seg" "$path" "$off" <<'PY'
import json, os, sys
seg_path, src, off = sys.argv[1], sys.argv[2], int(sys.argv[3])
with open(seg_path, "rb") as f:
    seg = f.read()
new = off
i = seg.rfind(b"\n")
if i >= 0:
    new = off + i + 1
tail = seg[new - off:]
if tail.strip():
    try:
        json.loads(tail.decode("utf-8"))
        new = off + len(seg)
    except Exception:
        pass
print(new, os.path.getsize(src))
PY
)
  mark_rc=$?
  if [ "$mark_rc" -ne 0 ] || [ -z "${mark:-}" ]; then
    emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"mark\",\"rc\":$mark_rc}"
    continue
  fi
  mark=${mark//$'\r'/}
  new_off=${mark%% *}
  size_now=${mark#* }
  case ${new_off:-x}${size_now:-x} in
    *[!0-9]*) emit "{\"t\":\"ingest_error\",\"file\":\"$base\",\"stage\":\"mark\",\"rc\":0}"; continue ;;
  esac

  save_state "$path" "$mt" "$size_now" "$new_last" "$new_off" "$used_today"

  accepted_total=$((accepted_total + accepted))
  dup_total=$((dup_total + dup))
  new_turns_total=$((new_turns_total + delta))
  emit "{\"t\":\"ingest\",\"file\":\"$base\",\"turns\":$total,\"accepted\":$accepted,\"dup\":$dup,\"rejected\":$rejected,\"new_turns\":$delta,\"used\":$used_today}"
done <<<"$scan"

if [ "$capped" -eq 1 ]; then capped_json=true; else capped_json=false; fi
emit "{\"t\":\"ingest_summary\",\"files\":$files,\"accepted\":$accepted_total,\"dup\":$dup_total,\"capped\":$capped_json,\"new_turns\":$new_turns_total,\"used\":$used_today}"

exit 0
