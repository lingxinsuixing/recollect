#!/bin/bash
# recollect 每日抽干，实际顺序（与下面的调用序列一致）：
#   无章会话按环境声明补章（G13，W37-A2 落）→ naming 抽 → event 抽 → fact 抽（三族）
#   → naming 审 → state 抽 → event/fact 审 → 时间回填 → time 审 → state 审
#   → derive 推 → episode 归组 → lesson 记错（第 6 pass，波三刀 B，2026-11-09）
# state 排在 naming 审核之后是硬顺序（方案 design/2026-10-31-state-pass-p2.md §6）：
# 状态主体只能引用已过审落库的锚，排在前面时每批都会因为"候选池里没有那格"而整批拒收。
# derive 排在 state 审核之后也是硬顺序（L3 是组合阶段）：它读的是**已过审落库**的记忆行，
# 排在前面时推导的依据行还没定局，推出来的边会在下一轮审核里被成排地推翻。
# episode 归组（L2，2026-11-04 I-6b）插在 derive 之后：它归组的是**活 fact** 行（写
# `episode`/`episode_fact` 与镜像卡），与 derive 不构成第二条抽取入口；同为纯程序段、放队尾。
#
# **这里没有夜间预算**（2026-11-06 用户裁定："调用配额、夜间预算这两个是预算，其他不是"）：
# 没有总闸、没有按族切百分比、没有"到额度就停"那一档。每一档按下面调用序列的顺序，
# **跑到自己的队列为空**（连续两次读回空批）或**连续失败**（`FAIL_STREAK`）才进下一档。
# `used` 仍逐档累加，但它**只是成本计数**：不判哪一档能不能开跑，也不判什么时候停。
# 行为的字面后果（不粉饰）：排在前面的 naming 队列只要够长，就能占满整夜，后面几档当晚
# 一次都不开跑。这正是"砍掉预算"的字面后果；要公平得另做调度器（轮转、固定批次数），
# 不在这条脚本里用一个新额度顶替。
# 剩下的两个帽子**都不是预算**，别读成额度：`EPISODE_CALL_CAP` 数的是 `hub episode run`
# 的**发数**（防坏二进制原地空转），`run_time` 的 50 步同理。
# 逐档记账口径（2026-11-04 I-1b / P-1；`used` 里混着三种单位是已知债，与预算无关）：
#   derive  = **次**：一批（一次 `hub derive run --max-batches 1`）记 1；落库行数 `rows` 不进账，
#             只累计进 `phase` 行的 `rows_total` 供人读。
#   distill = **轮**：一次调用交回几轮 `turns` 就记几。抽取各档都带 `--batch-turns 1`，
#             1 轮 ≈ 1 次 ⇒ 账面与"次"同值，但这个同值靠的是旗标而不是闸。
#             （`run_time` 那一档带 `--batch-turns 8`，是纯程序回填，压根不记 `used`。）
#   review  = **条**：一次调用 `processed` 几条记几条；`--limit 40` ⇒ 一发最多能记 40。
#   episode = **0**（2026-11-04 I-6b）：日路径这一档**不传座位**（`--replies`/`--llm-config`
#             都不给）⇒ 中间带一律拒、真零调用（I-12 起给了座位会逐对裁决，
#             见 `crates/hub/src/main.rs` 的 `episode_run`），
#             ⇒ episode 档的 `used` 恒 0。落库行数（读数里的 `episodes_written`）**单独记**
#             进 `phase` 行的 `episodes_total`，不进 `used`。
# 混单位是历史遗留：没有总闸之后它不再影响任何一档的取舍，只影响人读"这一夜花掉多少"。
# 统一到"次"是另一轮的事（会同时动 `summarize_drain.py` 的列）。
#
# **墙（W19c2）：下面这四个数是"坏到什么程度就停"，不是"花多少就停"**——
#   ① 单发墙钟 `LLM_TIMEOUT`（默认 300s）：一次调用超过它就按失败收；
#   ② 3 次尝试：同一单元连续失败到 `MAX_PASS_ATTEMPTS`（rc-distill/rc-ledger 那一格）就不再重试；
#   ③ 3 次失败隔离：第 3 次失败的那一发把该单元落成隔离终态，hub 按 rc=0 交回
#      `state=quarantined`——那一格不再是失败；
#   ④ 连续 5 个隔离（`SEAT_WALL_STREAK`）：停这一族，`why=quarantine_streak`。
# **③（抽取档）只由 hub 的 `retry_exhausted` 那一格识别，不从 `state` 猜**（W19c2b）：`state` 有两个歧义——
#   `pending` 既是"入队待审核"（`--no-review` 的合法结果）又是"失败后在退避里"；
#   `quarantined` 既是"重试耗尽"又是"审核/契约判出的质量隔离"。按 `state` 数重试会把每一发
#   成功都记成重试，按 `state` 立隔离墙会把质量隔离当座位卡死而停掉一族本该继续的活。
#   因此脚本只认 `retry_exhausted=1`：其它（含 `state=quarantined` 而这一格为 0）把连隔离计数归零。
# 重试中的 `state=pending` **不计数、不算失败**，也不推进任何一条墙。
# **审核档也有同一条墙，但判据不同源**（W19c2c）：`hub review run` 的读数里没有
#   `retry_exhausted` 这一格，它给的是 `quarantined`（这一发被判隔离的待审项条数）与 `errors`；
#   这两格任一大于 0 就推进连隔离计数，连续 5 发 ⇒ `why=quarantine_streak` 停这一族。
#   审核档的隔离条数**只进它自己那一行 `phase`**，不进 `end` 里那一格总账（那一格只数抽取档
#   因 `retry_exhausted` 整批隔离的发数）：条与批单位不同，混了就是一本混单位的账。
# **rc=2 不是墙**（W31）：hub 的退出码契约是"0 全收 / 2 整批拒 / 3 运维失败"
# （`crates/hub/src/main.rs` 的"退出码即契约"；`finish_naming` 把 `NamingFault::Rejected` 映成 2）。
# 2 说的是这一批/单元被质量闸拒收、已经落了终态（读数行里是 `state:"rejected"`）——那是**推进**，
# 不是座位故障：`run_distill` / `run_review` 都把 rc=2 按推进收（连失败计数清零、不记 `failed`、
# 不睡退避），只有其余非零才走 `FAIL_STREAK`。旧版把任何 rc≠0 都记进连失败，连续 3 发就把整档停掉——
# 小程序/长会话里连续拒收是常态，夜档会因此提前停摆。判据**只认退出码**，不从 `state` 猜（同 ③ 那条歧义）。
# 这里没有次数或金额额度：任何一档都不会因为"今天已经用了多少"而收手，只因为队列空、
# 连失败或连续隔离收手。
# 只做加法：不删数据、不改 serve；所有输出按天落 JSONL。
#
# **两档（W25）**：
#   - 无参数 = 下面这一整串（抽取 → 审核 → 时间回填 → derive → episode → lesson）；
#   - `--full` = 同一串 + 队尾两档维护：`hub anomaly sweep`（W26 落）与 `run_backup`（W28 落：
#     `hub backup` 一组——memory + transcript + ledger（必备）+ core（agent 私房本，
#     懒库存在才备，缺席如实记 null）+ manifest，五件一套）。日保底档用它：水位闸判
#     "没有待办"的夜晚也要跑一次全量维护，而全量维护比常规抽干多出来的正是这两档。
#     普通触发档（无 `--full`）**不跑**备份：备份是"每天一次"的事，与"有待办才蒸"不是同一条闸。
#     其它参数一律用法错误、退出 2——两档之外没有第三档，静默忽略一个拼错的旗标等于让它白跑。
set -u

FULL=0
for arg in "$@"; do
  case $arg in
    --full) FULL=1 ;;
    *)
      printf 'usage: %s [--full]\n' "$(basename "$0")" >&2
      exit 2
      ;;
  esac
done

DATA=${RECOLLECT_DATA_DIR:-${HOME}/.recollect/data}
# 二进制从版本化路径里取「最新装的那一个」。
# **按 mtime 取，不按文件名排序**：注释里那句"最新的那一个"讲的是新旧，而 sha 的字典序与安装顺序
# 毫无关系。2026-11-01 中心现场证伪：装完 `hub-bf3eb8d` 之后，`ls -1 hub-* | sort | tail -1`
# 给出的仍是 `hub-fd8e22c`（b < f）——新构建被静默跳过一整夜的话，读到的会是旧判据的产物。
# 想要可复现的固定档位，用 `RECOLLECT_HUB=<路径>` 显式钉住，而不是靠文件名。
HUB=${RECOLLECT_HUB:-$(ls -1t "$DATA"/bin/hub-* 2>/dev/null | head -1)}
LLM=${RECOLLECT_LLM_CONFIG:-$DATA/llm.toml}
DEVICE=${RECOLLECT_DEVICE_ID:-local-qoder}
# 墙钟上限（单发）。默认从 120000 抬到 300000：2026-10-31 有一发 naming（8 轮英文 agent 叙述）
# 手工重发是 HTTP 200、`base_resp.status_code=0`、正文合法 JSON，但思考 15,561 token、全程 >120s
# ⇒ 被 120000 判成 `error: llm`，日志里与真故障不可分。
# 别把它读成"这类语料在这档座位上必然超时"：2026-11-01 把同一批按两档墙钟各跑 4 遍，8/8 全过
# （134–290s）⇒ 真实形状是**这一族思考长度方差大**，抬墙钟只是把方差里能救的那部分救回来。
LLM_TIMEOUT=${RECOLLECT_LLM_TIMEOUT_MS:-300000}
LOG=${RECOLLECT_LOG:-$DATA/logs/daily-$(date +%F).jsonl}

mkdir -p "$(dirname "$LOG")"
export RECOLLECT_DEVICE_ID=$DEVICE
used=0
# 真失败的那一发也要有账：整夜全失败时，`end` 行不许与"这一夜无事可做"同形
# （2026-10-31 之前中心的空库夜就是 `used:0` + SUCCESS，看不出区别）。
failed=0
# 这一档的发数保险：数的是 **`hub episode run` 的发数**（与 `run_time` 的 50 步帽同族，
# 防坏二进制在原地空转），不是产出额度。归组窗逐趟放大（扫描帽倍增），正常几趟就到窗尾，
# 20 发封顶只是保险；它不参与任何"配额/百分比"换算，也没法被别的档借用或吃光。
EPISODE_CALL_CAP=20
# 两次调用之间的喘息（供应商侧的礼貌）。离线排期闸把它压到 0，真跑保持 2。
SLEEP=${RECOLLECT_DRAIN_SLEEP:-2}
# 连续失败到几次就放弃这一族。同一批毒数据或同一个坏座位不会自己变好，而每一发都要一次真调用
# 加一段退避；没有额度以后它是这一族唯一的停手闸（队列空是另一条）。哪一族是被这一格拦下来的，
# 看 `phase.why=fail_streak`。
FAIL_STREAK=${RECOLLECT_DRAIN_FAIL_STREAK:-3}
# 连续多少个单元被隔离就放弃这一族（W19c2）。**这是墙，不是预算**：它数的是"连续坏了几个单元"，
# 不是"今天还能花几次"——座位死掉时不该把整夜单元逐个隔离成终态，而该停手去修座位。
# 只数 `retry_exhausted=1` 的发（见文件头）：质量隔离（审核不确定、契约拒收）不推进这一格。
SEAT_WALL_STREAK=5
# 只进读数的一个总计数：`quarantined` 数**因重试耗尽而整批隔离**的发数（`retry_exhausted=1`）。
# 它不参与任何取舍（与 `used` 同一条律），只让 `end` 行说得清这一夜隔离过几次；
# 质量隔离（审核不确定等）不计入——单位不同，混了就又是一本混单位的账。
quarantined_total=0

# 观测面（W33-A10）两格：`max_call_ms` = 整夜 distill 各发里最慢一发真实出网的耗时
# （transport 的单调钟，hub 每发读数面的 `max_call_ms` 键）；`timeouts` = 各发
# `LlmFailure::Timeout` 计数之和。两个数**只进台账，不参与任何取舍**（与 `used` 同一条律）。
# 评估闸门：出现 `timeouts>0` 或 `max_call_ms` 逼近 300s 才启 `timeout_global` 实施
# （design/2026-11-07-w33-a10-wall-assessment.md §3）。
MAX_CALL_MS=0
TIMEOUTS_TOTAL=0

# 一行一个事件，且必须是**一行**：读数块（state_snapshot / state_uids）在空库里跑时
# python 会把 traceback 打到 stderr、stdout 交回空串，`printf '%s\n' ""` 就会在日志里落一条
# 空行，把"一天一个 JSONL"这条形状破掉（下游按行解析的读者会当场炸）。空载荷不落。
log() {
  [ -n "$1" ] || return 0
  case $1 in
    # 带换行的载荷不落：它会把"一行一个事件"这个形状破掉，下游按行解析的读者当场炸。
    *$'\n'*) return 0 ;;
  esac
  printf '%s\n' "$1" >> "$LOG"
}
jstr() { python3 -c 'import sys,json;print(json.dumps(sys.stdin.read()))'; }

# 从 hub 的输出里读 `turns` 那一格。失败那一发交回来的是"读数一行 + `error:` 一行"两段
# （hub 的"两件事同时出去"那条律），整体 `json.loads` 会把一次真失败读成 `turns=-1`。
# **扫描全部行**（2026-11-07 W33 评审缺陷①的同根扩展）：run_distill 以 `2>&1` 收输出，
# rc-llm-http 的 ops 行打 stderr 且先于读数行——首行不是读数。旧版"只看第一行"在遮蔽场景
# 把 `turns` 读成 -1，而 run_distill 在 rc=0 时无条件清 `bad` ⇒ 连失败闸永远凑不满，
# **这一族原地空转到天亮**（每发 10 秒；观测臂④的端到端桩实证）。现在逐行挑**含 `turns`
# 键**的合法 JSON 行取最后一行的值（ops 行不带键、error 行不是 JSON，都不贡献）；
# 一行都没有 ⇒ 仍然 -1，"读不出"的语义不变。
parse_turns() { printf '%s' "$1" | python3 -c 'import sys,json
v = -1
for line in sys.stdin.read().strip().splitlines():
    try:
        d = json.loads(line)
    except Exception:
        continue
    if isinstance(d, dict) and "turns" in d:
        t = d.get("turns")
        if isinstance(t, int) and not isinstance(t, bool): v = t
print(v)'; }

# 读"这一发是不是重试耗尽"（W19c2b 的分流键）。与 `parse_turns` 同一条律：**扫描全部行**、
# 只认显式的 `retry_exhausted=1`（或 `true`）：读不出来、键缺席、0 一律交回 0 ——
# 这些情况**都不是**"重试耗尽"，不许把 `state` 或沉默读成隔离。键是 hub 读数面的**最后**一格。
parse_exhausted() { printf '%s' "$1" | python3 -c 'import sys,json
v = 0
for line in sys.stdin.read().strip().splitlines():
    try:
        d = json.loads(line)
    except Exception:
        continue
    if isinstance(d, dict) and "retry_exhausted" in d:
        x = d.get("retry_exhausted")
        if x is True or x == 1: v = 1
print(v)'; }

# 观测面（W33-A10）两格的聚合读法：交回 "max_call_ms timeouts" 两个整数（空格分隔，
# 调用方用 read 拆）。**扫描全部行**（2026-11-07 W33 评审缺陷①的修法）：run_distill
# 以 `2>&1` 收输出，rc-llm-http 的 ops 行打 stderr 且先于读数行（self-heal/provider
# failure 在 complete 期间即打）——首行可能是非 JSON 的 ops 行。旧版"只看第一行"
# 会把整发交回 0 0，真实 max_call_ms（恰是"自愈重发后的慢一发"）被静默抹零。
# 现在逐行挑**含 `max_call_ms` 键**的合法 JSON 行（可能多行）：`max_call_ms` 取 max、
# `timeouts` 求和；ops 行、error 行、不带键的行一律不贡献。与既有解析器的另一条
# **有意偏离**：键缺席（旧二进制、或不认识这一行的桩）交回 **0** 而不是 -1——
# 这一对是"最慢值/求和"的聚合料，缺席的一发对"最慢"贡献 0 是准确的（那一发确实
# 没有比 0 更慢的观测可交），-1 反而会让 end 行的 `max_call_ms` 变成"有观测但
# 读不出"的谎。time pass 恒交 0/0（它从不开 LLM 口，键恒在）。
# derive 一发可能跑多批、每批一行：批行交"这一批的增量"（timeouts）与进程级累计
# 的最慢值（max_call_ms），跨行 sum/max 之后与"每发一行"的档同口径（见 derive_run 的注释）。
parse_call_obs() { printf '%s' "$1" | python3 -c 'import sys,json
mx = 0; to = 0
for line in sys.stdin.read().strip().splitlines():
    try:
        d = json.loads(line)
    except Exception:
        continue
    if not isinstance(d, dict) or "max_call_ms" not in d:
        continue
    v = d.get("max_call_ms"); t = d.get("timeouts")
    if isinstance(v, int) and not isinstance(v, bool) and v > mx: mx = v
    if isinstance(t, int) and not isinstance(t, bool): to += t
print(mx, to)'; }

# `hub derive run` 的读数里那一格叫 `rows`，**不叫 `turns`**：复用 `parse_turns` 会恒得 `-1`，
# 于是 `used` 永远不涨、derive 那一档的额度闸变成一件装饰（本仓"惰性守卫"那一族点过名）。
# 与 `parse_turns` 同一条律：只看第一行——失败那一发交回的是"读数一行 + `error:` 一行"，
# 整体 `json.loads` 会把一次真失败读成解析异常。读不出来一律 -1，不替调用方猜一个 0。
parse_rows() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try: print(json.loads(head).get("rows", -1))
except Exception: print(-1)'; }

# 收批的判据必须落在**游标**上：`rows == 0` 同时是"选批为空"与"整批被 `INSERT OR IGNORE` 吞掉"
# （`rc-distill/src/derive.rs` 的 `DeriveExit::Ok` 注释明写这两者在退出码上同形），
# 只有 `cursor_moved` 能把它们分开。缺这一格（旧二进制）⇒ -1 = 读不出来，
# 不许把它读成"游标没动"——那会让一个说不出原因的读数冒充一次"跑空了"。
parse_cursor_moved() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try:
    v = json.loads(head).get("cursor_moved")
except Exception:
    print(-1); raise SystemExit
print(-1 if v is None else (1 if v else 0))'; }

# `hub episode run` 的读数里落库行数那一格叫 `episodes_written`（键面在
# `crates/hub/src/main.rs` 的 `episode_run`：schema_version/job/batch/candidates/merged_pairs/
# span_pruned/episodes_written/members_linked/cards_written/superseded/dropped_members/
# mid_band_deferred/cursor_moved/lag_rows；用例 `crates/hub/tests/episode_run.rs` 的
# EPISODE_RUN 键清单钉着）——**不叫 `rows`**：复用 `parse_rows` 恒得 -1，于是行数格永远
# 记成"读不出"。与 `parse_rows` / `parse_turns` 同一条律：只看第一行（失败那一发交回的是
# "读数一行 + `error:` 一行"），读不出来一律 -1，不替调用方猜一个 0。
# 游标格 `cursor_moved` 与 derive 同名同义，直接复用 `parse_cursor_moved`。
parse_episodes() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try: print(json.loads(head).get("episodes_written", -1))
except Exception: print(-1)'; }

# lesson 档（第 6 pass，波三刀 B）的读数：`state turns drafts cards` 四个词/整数，空格分隔。
# **为什么收手判据看 `state` 而不是 `turns`**：`lessons=[]` 是合法空产出，那一发的 `turns`
# 是**选中的轮数**（>0）而 `drafts=0`、`state="empty"`——按 `turns` 判"还在推进"会把同一条
# 候选组反复重选（空产出不落 `review_item`，`batch_key` 不进幂等集）。`state="none"` 是选批
# 返回 `Ok(None)`（队列空，这一发一次模型都不调）。与 `parse_turns` 同一条律：**扫描全部行**
# （失败那一发是"读数一行 + `error:` 一行"，rc-llm-http 的 ops 行也可能先于读数），只认
# 带 `pass="lesson"` 的合法 JSON 行；读不出来 `state=?`、三格 -1，调用方按"读不出"收手，
# 不替它猜 0。
parse_lesson_readout() { printf '%s' "$1" | python3 -c 'import sys,json
state = "?"; turns = drafts = cards = -1
for line in sys.stdin.read().strip().splitlines():
    try:
        d = json.loads(line)
    except Exception:
        continue
    if not isinstance(d, dict) or d.get("pass") != "lesson" or "state" not in d:
        continue
    s = d.get("state")
    state = s if isinstance(s, str) and s else "?"
    def num(k):
        v = d.get(k)
        return v if isinstance(v, int) and not isinstance(v, bool) else -1
    turns = num("turns"); drafts = num("drafts"); cards = num("cards")
print(state, turns, drafts, cards)'; }

# reuse 反馈审计档（波二刀 B 的入队面）的读数：`candidates queued deduped` 三格。
# 与 `parse_lesson_readout` 同一条律：扫描全部行、只认带 `candidates` 与 `queued` 两格的合法
# JSON 行（2>&1 会先带出 stderr 的 ops 行）；读不出三格 -1，不替调用方猜 0。
parse_reuse_ingest_readout() { printf '%s' "$1" | python3 -c 'import sys,json
c = q = d = -1
for line in sys.stdin.read().strip().splitlines():
    try:
        v = json.loads(line)
    except Exception:
        continue
    if not isinstance(v, dict) or "candidates" not in v or "queued" not in v:
        continue
    def num(k):
        x = v.get(k)
        return x if isinstance(x, int) and not isinstance(x, bool) else -1
    c = num("candidates"); q = num("queued"); d = num("deduped")
print(c, q, d)'; }

# 派发档 I-2b · B-1：derive 的**拒收分诊**三格（`rejected_no_mechanism` / `rejected_no_evidence`
# / `rejected_anchor_gate`）。与 `parse_rows` 同一条律：**只看第一行**、读不出来一律 -1，
# 不替调用方猜 0——0 说的是"这一族今天没拒过"，-1 说的是"这一格我不认识"（旧二进制、
# 或不认识这一族的桩）。既有的 `parse_turns` / `parse_rows` / `parse_cursor_moved` **一字未动**
# （派发档：解析器不许改；这一格是**另加**的一支，只服务 derive 那一档的读数行）。
parse_derive_rejects() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try:
    d = json.loads(head)
except Exception:
    print("-1 -1 -1"); raise SystemExit
def num(k):
    v = d.get(k, -1)
    return v if isinstance(v, int) and not isinstance(v, bool) else -1
print(" ".join(str(num(k)) for k in
      ("rejected_no_mechanism", "rejected_no_evidence", "rejected_anchor_gate")))'; }

# `hub anomaly sweep --json` 的四格（W26）：矛盾一族一对、重影一族一对。
# 与 `parse_turns` / `parse_rows` 同一条律：**只看第一行**（失败那一发交回的是 `error:` 一行，
# 整体 `json.loads` 会把一次真失败读成"扫过了、库是干净的"）。读不出来一律 **-1**，
# 不替调用方猜 0——0 说的是"这一轮没升也没解"，-1 说的是"这一格我不认识"（旧二进制、
# 或不认识这一族的桩）。
parse_anomaly() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try:
    d = json.loads(head)
except Exception:
    print("-1 -1 -1 -1"); raise SystemExit
def num(box, k):
    v = (box or {}).get(k, -1)
    return v if isinstance(v, int) and not isinstance(v, bool) else -1
c, g = d.get("contradiction"), d.get("ghosts")
print(num(c, "raised"), num(c, "resolved"), num(g, "raised"), num(g, "resolved"))'; }

# `hub backup` 的读数面（W28）：name / memory / ledger / core / memory_rows / extras 六格，
# **没有 `--json` 旗标**——`run_backup`（`crates/hub/src/main.rs`）无论成败都印这一行 JSON，
# 传 `--json` 只会被 clap 判成用法错误（退出 2），所以日路径就按原样发命令、按第一行读这几格。
# 与 `parse_anomaly` / `parse_turns` 同一条律：**只看第一行**（失败那一发交回的是 `error:` 一行，
# 整体 `json.loads` 会把一次真失败读成"备份好了、0 行"）。读不出来一律 **-1**，不替调用方猜 0——
# `extras:0` 说的是"这一组没有随组走的配置"，`-1` 说的是"这一格我不认识"（旧二进制、或桩不认这一族）。
# 文件名格只认非空字符串：空串与非字符串同样打回 -1（文件名是恢复时唯一的把手，不许被读成空）。
# `core` 一格的特殊形状（W28）：**null 是合法读数**——agent 私房本是懒库，`core.db` 没建过
# 就是缺席，清单/报告如实记 `core: null`（不造假文件）。所以 core 的分流是三态：
# 字符串（文件名）→ 原样、JSON null → `null` 原样落行、其余（旧二进制缺格等）→ -1。
# 输出的是**去掉外层花括号的键值片段**（`"name":…,"memory":…,…`），由调用方拼进 `phase` 行，
# 这样各格在日志里是同一层键、与 hub 读数面同名，读者不用记第二个键名。
parse_backup() { printf '%s' "$1" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try:
    d = json.loads(head)
except Exception:
    d = {}
def num(k):
    v = d.get(k, -1)
    return v if isinstance(v, int) and not isinstance(v, bool) else -1
def txt(k):
    v = d.get(k, -1)
    return v if isinstance(v, str) and v else -1
def core(k):
    v = d.get(k, -1)
    if v is None: return None
    return v if isinstance(v, str) and v else -1
out = {"name": txt("name"), "memory": txt("memory"), "ledger": txt("ledger"),
       "memory_rows": num("memory_rows"), "extras": num("extras"), "core": core("core")}
print(json.dumps(out)[1:-1])'; }

# 分诊三格的累计规则（起手 -1，另带一个独立状态位 `rej_poison`）：第一发**读得出来**的以它起账，
# 之后逐发相加；任何一发读不出来 ⇒ 三格一起变 -1、把 `rej_poison` 置 1，之后的读数不再把它复活。
# 理由与上面同一条："少算了一发的数"与"这一族今天一次都没拒"在运维面上是两件事，补 0 就是把前者念成后者；
# 而复活成"部分和"同样是谎，所以宁可整档标 -1（这一档今天的分诊读数不完整，
# 去 `hub derive run --json` 那一面逐批发看，那里有五格且恒等式被用例钉着）。
# **为什么要一个独立状态位，而不是复用 -1 的符号**（2026-11-04 I-2c / M-1）：
# 三格起手是 -1，被读不出打回时也是 -1，同一个 -1 因此同时说着两件事——①"今天还没有一发被读出来"、
# ②"今天有过一发读不出来、总账已经作废"。只读符号位分不出这两种，I-2b 那一版就把②当成了①，
# 于是坏一发之后的可读读数"从这一发重新起账"，交回一个**部分和**（排期闸实测 15/15/15：
# 坏那一发之后三发各 5/5/5 被加成 15，而第 1 发真实的 2/1/0 被丢掉）——正是本仓点名的
# "一个值承担两种语义"。`rej_poison` 只回答一件事：这一档今天有没有出现过读不出来的分诊读数；
# 一旦置 1 就锁死，三格永久停在 -1，之后的读数只记账不复活。
# 两种 -1 在 `phase` 行里**同形**（都写 -1），这是有意的：分诊读数的口径只有"完整"与"作废"两态，
# 作废的原因不进这一格。要查是哪一种，看同一轮里逐发的 `t:derive` 事件行——那一段带着原始 `out`，
# "整档一个都没读出来"（旧二进制）与"读到一半读不出来"在那几行里一眼可分。

if [ ! -x "$HUB" ]; then
  log "{\"fatal\":\"hub binary missing: $HUB\"}"
  exit 3
fi
if [ ! -f "$LLM" ]; then
  log "{\"fatal\":\"llm config missing: $LLM\"}"
  exit 3
fi

log "{\"start\":\"$(date -Is)\",\"data\":\"$DATA\",\"hub\":\"$HUB\",\"llm_timeout_ms\":\"$LLM_TIMEOUT\",\"episode_calls_cap\":$EPISODE_CALL_CAP}"

run_distill() {
  # 两个形参：pass 与族。这一档**没有额度**——它跑到自己的队列空（连续两次 `turns=0`）、
  # 连续失败（`FAIL_STREAK`）或连续隔离（`SEAT_WALL_STREAK`）才收手；`used` 只记账，不参与任何取舍。
  local pass=$1 fam=$2 zero=0 out rc turns exhausted n=0 bad=0 stuck=0 quarantined=0
  local args=(distill once --pass "$pass")
  if [ -n "$fam" ]; then args+=(--family "$fam"); fi
  while true; do
    out=$("$HUB" "${args[@]}" --batch-turns 1 --no-review --data-dir "$DATA" \
          --llm-config "$LLM" --llm-timeout-ms "$LLM_TIMEOUT" 2>&1); rc=$?
    log "{\"t\":\"distill\",\"pass\":\"$pass\",\"family\":\"$fam\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    # 观测面聚合（W33-A10）：失败那一发也要聚合——慢的/超时的那发恰恰是观测面
    # 要看见的（rc 只决定业务分流，不决定观测记账；观测记账不改下面任何业务结果）。
    read -r call_ms n_timeouts <<<"$(parse_call_obs "$out")"
    [ "$call_ms" -gt "$MAX_CALL_MS" ] && MAX_CALL_MS=$call_ms
    TIMEOUTS_TOTAL=$((TIMEOUTS_TOTAL + n_timeouts))
    turns=$(parse_turns "$out")
    exhausted=$(parse_exhausted "$out")
    # 与 derive 的 M-2 护栏同形：解析器自己没跑起来会交回空串或非数字，那不是 0 也不是负数，
    # 在 `[ -gt ]`/`[ -lt ]` 里都因"integer expression expected"为假 ⇒ 会掉进"还在推进"那一支。
    case $turns in ''|*[!0-9-]*) turns=-1 ;; esac
    case $exhausted in ''|*[!0-9]*) exhausted=0 ;; esac
    n=$((n + 1))
    # 整批/单元质量拒收（rc=2）是**推进**，不是座位故障（见文件头"rc=2 不是墙"那一段）：
    # 单元已落终态，把连失败计数清零后立刻进下一轮——不记 `failed`、不睡退避、不新增任何 `phase` 键。
    # 只认退出码：`state` 的歧义（W19c2b）不在这里猜。
    if [ "$rc" -eq 2 ]; then
      bad=0
      continue
    fi
    if [ "$rc" -ne 0 ]; then
      bad=$((bad + 1)); failed=$((failed + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        quarantined_total=$((quarantined_total + quarantined))
        log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"$fam\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"quarantined\":$quarantined}"
        return 0
      fi
      sleep 10
      continue
    fi
    bad=0
    # 隔离墙（W19c2b）：**只认 `retry_exhausted=1`**。它说的是"这一批是单元级失败试满上限、
    # 整批落成的隔离"；`state=quarantined` 另有来源（质量隔离），那种批在这里把连隔离计数
    # 归零、不推进墙——否则一位质量不达标的批会把整族当成座位卡死停掉。
    if [ "$exhausted" = 1 ]; then
      quarantined=$((quarantined + 1)); stuck=$((stuck + 1))
      if [ "$stuck" -ge "$SEAT_WALL_STREAK" ]; then
        # 连续这么多个单元都是重试耗尽 ⇒ 座位/族坏到不该再逐个判死：停这一族（rc=0，不是失败）。
        quarantined_total=$((quarantined_total + quarantined))
        log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"$fam\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"quarantine_streak\",\"used\":$used,\"quarantined\":$quarantined}"
        return 0
      fi
    else
      stuck=0
    fi
    if [ "$turns" = "0" ]; then
      zero=$((zero+1))
      if [ $zero -ge 2 ]; then
        quarantined_total=$((quarantined_total + quarantined))
        log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"$fam\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"queue_empty\",\"used\":$used,\"quarantined\":$quarantined}"
        return 0
      fi
      sleep "$SLEEP"
      continue
    fi
    zero=0
    # 只有正数才记账：`turns=-1` 是"连读数都没打出来"，别把账面写成负的。
    # 没有额度兜底之后，这一支必须能被同一个连失败闸收手，否则坏二进制会让它 10 秒一次原地转到天亮。
    if [ "$turns" -gt 0 ]; then
      used=$((used + turns))
    else
      bad=$((bad + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        quarantined_total=$((quarantined_total + quarantined))
        log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"$fam\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"quarantined\":$quarantined}"
        return 0
      fi
      sleep 10
      continue
    fi
    sleep "$SLEEP"
  done
}

run_review() {
  # 一个形参：pass。没有额度：跑到 `remaining_pending_review=0`（队列清空）、
  # `processed=0`（这一发一条都没处理 = 停住了/读数读不出）或连续隔离墙（W19c2c）才收手。
  # 隔离墙与抽取档同一条（`SEAT_WALL_STREAK`），但**判据不同源**（见文件头）：`hub review run`
  # 的读数里没有 `retry_exhausted`，它的两格是 `quarantined`（这一发被判隔离的待审项条数）
  # 与 `errors`（这一发的错）；任一大过 0 就把连隔离计数推进一格，否则归零。
  # 审核档的隔离条数只累计进这一族自己的 `phase` 行，**不进** `end` 的全局那一格。
  local pass=$1 out rc left done_n q_n err_n n=0 stuck=0 q_total=0
  # 观测面两格（W33 评审缺陷②补线）：与 run_distill 同一对手局——这一档真烧
  # `--llm-config`，一发撞 300s 墙在 end 行不许恒 0/0。
  local call_ms=0 n_timeouts=0
  while true; do
    out=$("$HUB" review run --pass "$pass" --limit 40 --data-dir "$DATA" \
          --llm-config "$LLM" --llm-timeout-ms "$LLM_TIMEOUT" --json 2>&1); rc=$?
    log "{\"t\":\"review\",\"pass\":\"$pass\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    # 观测面聚合（与 run_distill 同形）：失败那一发也要聚合——慢的/超时的那发恰恰是
    # 观测面要看见的（rc 只决定业务分流，不决定观测记账）。
    read -r call_ms n_timeouts <<<"$(parse_call_obs "$out")"
    [ "$call_ms" -gt "$MAX_CALL_MS" ] && MAX_CALL_MS=$call_ms
    TIMEOUTS_TOTAL=$((TIMEOUTS_TOTAL + n_timeouts))
    # 整批拒收（rc=2）同样按推进收（W31，与 `run_distill` 同一条律）：它是质量结果，不是座位故障，
    # 不记 `failed`，继续按本发读数走下面的队列/隔离判据。今天的 `review run` 函数头只列 0/3/4
    # （`crates/hub/src/main.rs` 的 `run_review_run`），这一支是给共享契约里"2 = 整批拒收"
    # 留的同一口径——免得一次质量拒收被记成一次运维失败、把 `end.fails` 打歪。
    if [ "$rc" -eq 2 ]; then
      :
    elif [ "$rc" -ne 0 ]; then
      failed=$((failed + 1))
    fi
    read -r left done_n q_n err_n <<<"$(printf '%s' "$out" | python3 -c 'import sys,json
s=sys.stdin.read()
i=s.find("{")
try:
    d=json.loads(s[i:]) if i>=0 else {}
except Exception:
    d={}
print(d.get("remaining_pending_review",0), d.get("processed",0), d.get("quarantined",0), d.get("errors",0))')"
    # 四格都必须能当整数用：读不出来（空串/非数字）一律按 0，与上面 `turns` 同一律。
    case ${left:-} in ''|*[!0-9-]*) left=0 ;; esac
    case ${done_n:-} in ''|*[!0-9-]*) done_n=0 ;; esac
    case ${q_n:-} in ''|*[!0-9-]*) q_n=0 ;; esac
    case ${err_n:-} in ''|*[!0-9-]*) err_n=0 ;; esac
    n=$((n + 1))
    used=$((used + done_n))
    q_total=$((q_total + q_n))
    # 隔离墙（W19c2c）：与抽取档那条墙同一格数（`SEAT_WALL_STREAK`），判据换成审核读数面自己的
    # 两格。**这是墙不是预算**：它数的是"连续几发都在隔离/报错"（座位或这一族坏到不该再逐个判死）。
    if [ "$q_n" -gt 0 ] || [ "$err_n" -gt 0 ]; then
      stuck=$((stuck + 1))
      if [ "$stuck" -ge "$SEAT_WALL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"\",\"kind\":\"review\",\"calls\":$n,\"why\":\"quarantine_streak\",\"used\":$used,\"quarantined\":$q_total}"
        return 0
      fi
    else
      stuck=0
    fi
    if [ "$left" = "0" ] || [ "$done_n" = "0" ]; then
      log "{\"t\":\"phase\",\"pass\":\"$pass\",\"family\":\"\",\"kind\":\"review\",\"calls\":$n,\"why\":\"queue_empty_or_stalled\",\"used\":$used,\"quarantined\":$q_total}"
      return 0
    fi
    sleep "$SLEEP"
  done
}

# 推导档（L3）：仿 `run_distill` 的形状，但有三处口径要单独交代。
# ① **`used` 记的是「次」，不是「行」**（2026-11-04 I-1b / P-1）：一次 `hub derive run
#    --max-batches 1` = 一批 = 一次模型调用 ⇒ 每一批发出去的调用记 1，落库行数 `rows` 不进账。
#    `rows` 只累计进 `phase` 行的 `rows_total`（P-2：让人一眼看清"这一档花了几发、落了几行"；
#    `hub derive run` 自己的读数行本轮不加键、不改键名）。
#    旧口径（`used += rows`）的害处已被排期闸实证（那时还有总闸可比）：
#    `DERIVE_MAX_ROWS_PER_BATCH` = 40（`crates/rc-distill/src/derive.rs:16`）数的是**选进来的输入行数**，
#    而 `rows` 交回的是**写入函数实际插入的行数**，含 mention 与 I6 的别名补铸
#    （`crates/rc-derive/src/derive.rs:160-163` 把补铸并进 `rows_written`）⇒ **一批落库可以超过批宽 40**。
#    副本库实测（V-3 评审 A3，`<data-dir>/derive-evidence-accept2`，只读 SQL）：
#    一批（输入 ≤40 行）落 **51 行**，`memory_mention` 48→99，新增 51 行全是 `source_agent='rc-derive'`
#    的别名补铸（29 个 `mem_id` × 单行至多 5 个锚），同轮 `card`=36 与 `fact_link`=0 一动没动；
#    那一轮边/卡/链/时间线各 0 行、`rows` 照样是 51 ⇒ **一批可以一条边都不落却铸 51 行 mention**。
#    上界因此是「批行数 × 该行命中的锚数」而不是 40：按行记账时一发调用能把 51 记进 `used`
#    （`used` 只数调用、`rows` 另记，关的就是这个口子）。
#    ——补桩后先红那一次：`calls=2` 时 `used` 从 190 跳到 206（那一夜的总闸是 200）。
#    另有一条读数面的不对称要知道（本轮不改，登记 I-3b）：`rows` 里"索引（补铸 mention）"与"产出"
#    拆不开——`hub derive run --json` **没有** `mentions_written` 那一格，只有 `hub cycle once` 面有
#    `derive_mentions_written`（`crates/rc-cycle/src/lib.rs:144`、累加在 `:339`），而 cycle 面不分批。
#    所以日路径的 `rows_total` 只说"落了几行"，不许被读成"产出了几条边/卡"。
#    绝不能复用 `parse_turns`：那一格在 derive 的读数里叫 `rows`，读 `turns` 恒得 -1 ⇒ used 不涨。
# ② **三态分诊**（P-5）：`parse_rows` 交回的三个区间各走各的支，两态不许折成一态——
#    `rows>0` 是有产出的轮（记次，`rows_total` 累加）；
#    `rows=0` 是合法空批（走③的游标判据）；
#    `rows<0` 是**读数里根本没有 `rows` 这一格**＝这一族压根没被解析出来（旧二进制、
#    或不认识这一族的桩）；解析器自己没跑起来、交回空串或非数字的，也并进这一态（见下面 `case` 那行）。
#    旧代码把第三态与 `rows=0` 合进同一条 `else sleep 10`，
#    于是"解析不出来"冒充"跑空了"，日志里一行都说不出这一族为什么没动，而每一发白等 10 秒
#    （控制器 2026-11-04 实跑：整轮被拖长约 5 分钟）。现在第三态走连失败闸（`bad`），
#    最多烧 `FAIL_STREAK` 发就收手，不再占着额度原地转。
# ③ **收批的判据落在游标上**：`rows=0` 且 `cursor_moved=false` 连续两次才叫"跑空"（`why=queue_empty`）；
#    `rows=0` 而游标动了的轮是真干过活的轮（同批重答、内容寻址全命中），不许读成跑空。
# ④ **拒收分诊三格**（派发档 I-2b · B-1，累计律与状态位见上面 :151 那段）：`phase` 行的三个新键
#    `rej_no_mechanism_total` / `rej_no_evidence_total` / `rej_anchor_gate_total` 按次累加
#    `hub derive run --json` 读数里的同名三格（证据闸两道 + I4 锚门槛）。
#    读不出来 = -1 并**永久停在** -1（`parse_derive_rejects` + `rej_poison` 那一格闩），
#    收批判据一律不看这三格，所以旧二进制（没有这三格）只会让这三格是 -1，
#    不会改变这一档的任何既有行为。
#    **这三格是子集，与总格 `rejected` 只有不等式关系**（口径见下面 B-1 那段注释）：
#    `rejected` 的完整恒等式（五格 = 总格）住在 `hub derive run` 那一面与
#    `crates/hub/tests/derive_run.rs` 的用例里；日路径只搬分诊用得上的三格。
# `why` 沿用既有词表（`queue_empty`/`fail_streak`/`queue_empty_or_stalled`/`step_cap`），
# 不新增值——跨天日志按 `why` 分过代。②里"读不出"那一支选 `fail_streak`，理由是：
# 这一族停手既不是因为队列空（读不出就无从知道队列），也不是因为哪一发被拦下，它是"连续几发
# 交不回能用的读数"，与 rc≠0 同一条闸、同一个词。两类失败在日志里
# 分得开：这一族的 `t:derive` 事件行有 `rc` 与原始 `out`，rc=0 而读数没有 `rows` 键的那种
# 形状在那一行里一眼可辨（本函数头②说的那件"说不出原因"的事，缺的就是这一格的对应关系）。
run_derive() {
  # 没有额度：跑到自己的队列空（空批 + 游标不动连续两次）或连续失败才收手。
  local out rc rows moved zero=0 n=0 bad=0 rows_total=0
  # 分诊三格的总账（起手 -1 = "今天还没有任何一发被读出来"；见上面那段累计规则）。
  # `rej_poison` 是**独立的状态位**，不复用 -1 的符号：-1 同时是"起手"与"被打回"两种状态的值，
  # 只读符号位分不出这两种（I-2b 就是在这里把"已作废"当成"还没起账"，交回了一个部分和）。
  # 它只回答一件事：这一档今天有没有出现过读不出来的分诊读数。一旦置 1 就锁死到这一档结束。
  local r3 q1 q2 q3 rej_nm=-1 rej_ne=-1 rej_ag=-1 rej_poison=0
  # 观测面两格（W33 评审缺陷②补线）：derive 档同样真烧 `--llm-config`，聚合与
  # run_distill / run_review 同形。
  local call_ms=0 n_timeouts=0
  while true; do
    out=$("$HUB" derive run --max-batches 1 --data-dir "$DATA" \
          --llm-config "$LLM" --llm-timeout-ms "$LLM_TIMEOUT" --json 2>&1); rc=$?
    log "{\"t\":\"derive\",\"pass\":\"derive\",\"family\":\"\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    # 观测面聚合（与 run_distill 同形）：失败那一发也要聚合。derive 一发可能跑多批、
    # 每批一行读数——parse_call_obs 跨行 sum/max（批行口径见 parse_call_obs 的注释）。
    read -r call_ms n_timeouts <<<"$(parse_call_obs "$out")"
    [ "$call_ms" -gt "$MAX_CALL_MS" ] && MAX_CALL_MS=$call_ms
    TIMEOUTS_TOTAL=$((TIMEOUTS_TOTAL + n_timeouts))
    rows=$(parse_rows "$out")
    moved=$(parse_cursor_moved "$out")
    # `parse_rows` 正常只交回 -1 / 0 / 正整数；交回**空串或非数字**只可能是解析器自己没跑起来
    # （本机 `python3` 是空壳时就是这个形状，2026-11-04 实测：整族 10 发被读成"合法空批"，
    # `rows_total=0` 却报了一个收手词，一行都说不出这一族不认识）。空串在 `[ -gt ]` 与
    # `[ -lt ]` 两道判断里都因"integer expression expected"为假 ⇒ 会掉进第三支冒充 `rows=0`。
    # 与 P-5 同一条律：那不是"跑空"，是"读不出"，统一并到 -1 那一态。
    case $rows in ''|*[!0-9-]*) rows=-1 ;; esac
    n=$((n + 1))
    if [ "$rc" -ne 0 ]; then
      bad=$((bad + 1)); failed=$((failed + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"derive\",\"family\":\"\",\"kind\":\"derive\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"rows_total\":$rows_total,\"rej_no_mechanism_total\":$rej_nm,\"rej_no_evidence_total\":$rej_ne,\"rej_anchor_gate_total\":$rej_ag}"
        return 0
      fi
      sleep 10
      continue
    fi
    # 一次 rc=0 的调用 = 一次模型调用，记 1；行数不进 `used`（口径见函数头①）。
    # 失败那一发（rc≠0）不记，与 `run_distill` 同一条律：真失败的账在 `end.fails` 那一格。
    used=$((used + 1))
    # B-1：拒收分诊三格在这一发上各加各的（只在 rc=0 的读数上加，与 `used` 同一条律：
    # 失败那一发交回的不是本轮的判定）。
    # **这三格是子集，不等于总账，也没有可核对的关系**（2026-11-04 I-2c / N-2 把口径写准）：
    # `hub derive run --json` 的恒等式是**五格**恰好拼成总格 `rejected`——
    # `rejected_no_mechanism + rejected_no_evidence + rejected_anchor_gate
    #  + rejected_parse_other + rejected_chain_merged == rejected`
    # （`crates/rc-derive/src/derive.rs:255` 的 `debug_assert_eq!` 加 `crates/hub/tests/derive_run.rs`
    # 的用例钉着；键面在 `crates/hub/src/main.rs:6598-6603`）。日路径只搬前三格 ⇒ 这里之和与总格之间**只有不等式**，差额是后两格：
    # "没有码的 parse 拒收"（枚举外 kind / 别名越界 / 环节数 < 2 / 链内重复句柄 / 时间线形态）与
    # "I5 链合并"，它们是"落库形态"的分诊而不是"这一族有没有干活"的分诊。
    # 所以**别把这三个数相加去对 `hub derive run` 各批发回的 `rejected` 之和**——必然对不上，
    # 而日路径既不落总格 `rejected`、也不落那两格残差，差额在这一档的日志里没有任何一格能说明。
    # 要核对恒等式就去 `hub derive run --json` 那一面逐发看五格。（残差要不要在 shell 侧补一格，
    # 与 `rows` 拆不开索引/产出同一批，登记 I-3b；本轮 crates 禁碰，也不给 `phase` 行加新键。）
    r3=$(parse_derive_rejects "$out")
    q1=${r3%% *}; q2=${r3#* }; q2=${q2%% *}; q3=${r3##* }
    # M-2：与上面 `case $rows in ''|*[!0-9-]*) rows=-1` 同形的护栏，I-2b 只给 `rows` 补过、
    # 这一支漏了。`parse_derive_rejects` 正常只交回"三个以单空格分隔的整数"；交回**空串或非数字**
    # 只可能是解析器自己没跑起来（本机 `python3` 是 Store 空壳时正是这个形状）。三个值是**拆完才判**的：
    # 空串在 `[ -lt ]` 里报"integer expression expected"并整体为假，于是三道判断全部落空，
    # 走到"以本发起账"那一支把 `rej_nm` 赋成**空串** ⇒ `phase` 行写成 `"rej_no_mechanism_total":,`，
    # 整行不可解析（破的就是 `log()` 之后"一行一个合法 JSON"那条形状）；再往下一发还会把空串
    # 带进算术式当 0 用，把"读不出"念成"没拒过"。与 ② 同一条律：那不是 0，是读不出 ⇒ 并进 -1 那一态。
    case $q1 in ''|*[!0-9-]*) q1=-1 ;; esac
    case $q2 in ''|*[!0-9-]*) q2=-1 ;; esac
    case $q3 in ''|*[!0-9-]*) q3=-1 ;; esac
    if [ "$q1" -lt 0 ] || [ "$q2" -lt 0 ] || [ "$q3" -lt 0 ]; then
      # 这一发读不出来 ⇒ 整档标 -1 并**置闩**：总账到此作废，之后的读数只记账不复活
      # （为什么必须是独立状态位而不是 -1 的符号，见上面 :151 那段与 `local … rej_poison=0`）。
      rej_nm=-1; rej_ne=-1; rej_ag=-1; rej_poison=1
    elif [ "$rej_poison" -ne 0 ]; then
      # 已经被打回过 -1：三格保持 -1，**不**以本发起账。这一支就是 M-1 要挡的那条复活路径——
      # 从坏那一发之后重新起账会交回"部分和"（实测 15/15/15），既与总账不符，也把第 1 发真读出的
      # 2/1/0 悄悄丢掉；宁可整档标 -1，让读的人知道"今天的分诊读数不完整，去 hub 面逐批发看"。
      :
    elif [ "$rej_nm" -lt 0 ]; then
      # 起手且从未被打回过 = 今天还没有过任何可读的一发 ⇒ 以本发起账，不从 0 累。
      rej_nm=$q1; rej_ne=$q2; rej_ag=$q3
    else
      rej_nm=$((rej_nm + q1)); rej_ne=$((rej_ne + q2)); rej_ag=$((rej_ag + q3))
    fi
    if [ "$rows" -gt 0 ]; then
      bad=0; zero=0
      rows_total=$((rows_total + rows))
    elif [ "$rows" -lt 0 ]; then
      # 三态里的第三态：读数里没有 `rows` 这一格。按连失败闸收手，别让它冒充"跑空"。
      bad=$((bad + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"derive\",\"family\":\"\",\"kind\":\"derive\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"rows_total\":$rows_total,\"rej_no_mechanism_total\":$rej_nm,\"rej_no_evidence_total\":$rej_ne,\"rej_anchor_gate_total\":$rej_ag}"
        return 0
      fi
      sleep 10
    else
      # rows=0：合法空批，判据落在游标上（函数头③）。
      bad=0
      if [ "$moved" = "0" ]; then
        zero=$((zero+1))
        if [ "$zero" -ge 2 ]; then
          log "{\"t\":\"phase\",\"pass\":\"derive\",\"family\":\"\",\"kind\":\"derive\",\"calls\":$n,\"why\":\"queue_empty\",\"used\":$used,\"rows_total\":$rows_total,\"rej_no_mechanism_total\":$rej_nm,\"rej_no_evidence_total\":$rej_ne,\"rej_anchor_gate_total\":$rej_ag}"
          return 0
        fi
      else
        zero=0
      fi
    fi
    sleep "$SLEEP"
  done
}

# episode 归组档（L2，2026-11-04 I-6b）：仿 derive 档形状，四处口径单独交代。
# ① **`used` 恒 0**：归组内核是纯程序——`run_episode_pass(m, t, budget, adjudicator, now_ms)`
#    的签名里没有 `&dyn LlmBackend` 形参；hub 显式丢弃座位（`episode_run` 里座位"装而不叫"，
#    递进来只为验配置，一次也不会被调用；crates/hub/src/main.rs 的 episode_run 注释明写
#    "零 LLM 调用也要 rc=0 是结构性的"）。与 derive 档修正后的口径同源（`used` 只数模型调用）：
#    episode 每一发记 0，`used` 在这一档纹丝不动，`end.used` 里看不到它。
#    落库行数**单独记一格**：读数里的 `episodes_written` 逐发累加进 `phase` 行的
#    `episodes_total`，不进 `used`（文件顶部 episode = **0** 那条）。
#    本档的帽子（`EPISODE_CALL_CAP`）数的是 `hub episode run` 的**发数**，与产出无关，
#    也不是预算：它防的是坏二进制在原地空转。
#    座位照递（`--llm-config` + `--llm-timeout-ms`）：配坏了当场 rc≠0 走 FAIL_STREAK，
#    "配了但配坏了"与"没配"两格不许塌成一种（hub 侧同一条律）。
# ② **三态分诊与 derive 同律**：`episodes_written>0` 是有产出的轮（`episodes_total` 累加）；
#    `=0` 是合法空批（走③的游标判据）；`<0` 是读数里根本没有那一格 = 解析不出来
#    （旧二进制、或不认识这一族的桩），空串/非数字同归这一态（下面 `case` 那行护栏，
#    与 `parse_rows` 的 M-2 同一条律：空串在 `[ -gt ]`/`[ -lt ]` 里都因
#    "integer expression expected"为假，会掉进空批那支冒充"跑空"）。第三态走连失败闸
#    （`bad` / FAIL_STREAK），不许它冒充"跑空"，也不许它白烧发数。
# ③ **收批判据落在游标上**（与 derive ③ 同一条律）：`episodes_written=0` 同时是"没新对"
#    与"整段已在库里"（episode_run 的 doc ②明写），只有 `cursor_moved` 能分开——
#    空批 + 游标不动连续两次才叫"跑空"（`why=queue_empty`）。缺这一格（旧二进制）⇒ -1，
#    不许读成"游标没动"。
# ④ **`why` 沿用既有词表**（queue_empty/fail_streak/queue_empty_or_stalled/step_cap），
#    不新增值——跨天日志按 `why` 分过代。发数帽到了写 `step_cap`（与 `run_time` 的 50 步帽
#    同一个词：都是"帽子到了"而不是"队列空了"）。
# `phase` 行除既有各格外带三格新键：`llm_calls:0`（模型调用数，恒 0——把它写出来是让
# "这一档没花成本"在日志里可核对，而不是只活在注释里）、`episodes_total`（行数）、
# `cursor_moved`（最后一发的游标是否动：1 动 / 0 没动 / -1 读不出）。
# ⑤ **循环条件只有发数帽**：episode 零调用、零成本（①），没有理由让它被任何别的档的账面
#    牵连。旧版这里还挂着一个总额闸，那一夜前面各档把它顶到顶时这一档就被静默饿死
#    （calls=0、日志里一行都说不出为什么，桩测红态实证）。现在那个闸连同整条预算都不在了。
run_episode() {
  local cap=$1 out rc written moved zero=0 n=0 bad=0 episodes_total=0 moved_last=-1
  while [ "$n" -lt "$cap" ]; do
    # 控制器拍板（2026-11-05）：不递座位。I-12 解禁裁决后，递 --llm-config 会让夜档真烧
    # 裁决调用而账面 used:0（账实不符），且裁决缓存不落库 ⇒ 每晚对同样的对重问重烧
    # （实测同 prompt_sha 2 对翻面）。开闸判据=裁决缓存落库 + usage 闭集加 episode 格（归档位制）。
    out=$("$HUB" episode run --max-batches 1 --data-dir "$DATA" --json 2>&1); rc=$?
    log "{\"t\":\"episode\",\"pass\":\"episode\",\"family\":\"\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    written=$(parse_episodes "$out")
    moved=$(parse_cursor_moved "$out")
    # 与 `parse_rows` 的 M-2 护栏同形：解析器交回**空串或非数字**只可能是它自己没跑起来，
    # 并进 -1 那一态，不许冒充"合法空批"。
    case $written in ''|*[!0-9-]*) written=-1 ;; esac
    n=$((n + 1)); moved_last=$moved
    if [ "$rc" -ne 0 ]; then
      bad=$((bad + 1)); failed=$((failed + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"episode\",\"family\":\"\",\"kind\":\"episode\",\"calls\":$n,\"llm_calls\":0,\"why\":\"fail_streak\",\"used\":$used,\"episodes_total\":$episodes_total,\"cursor_moved\":$moved_last}"
        return 0
      fi
      sleep 10
      continue
    fi
    # 纯程序：这一发**不记 `used`**（恒 0 的口径见函数头①）；行数只进 `episodes_total`。
    if [ "$written" -gt 0 ]; then
      bad=0; zero=0
      episodes_total=$((episodes_total + written))
    elif [ "$written" -lt 0 ]; then
      # 三态里的第三态：读数里没有 `episodes_written` 这一格。按连失败闸收手，别让它冒充"跑空"。
      bad=$((bad + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"episode\",\"family\":\"\",\"kind\":\"episode\",\"calls\":$n,\"llm_calls\":0,\"why\":\"fail_streak\",\"used\":$used,\"episodes_total\":$episodes_total,\"cursor_moved\":$moved_last}"
        return 0
      fi
      sleep 10
    else
      # episodes_written=0：合法空批，判据落在游标上（函数头③）。
      bad=0
      if [ "$moved" = "0" ]; then
        zero=$((zero+1))
        if [ "$zero" -ge 2 ]; then
          log "{\"t\":\"phase\",\"pass\":\"episode\",\"family\":\"\",\"kind\":\"episode\",\"calls\":$n,\"llm_calls\":0,\"why\":\"queue_empty\",\"used\":$used,\"episodes_total\":$episodes_total,\"cursor_moved\":$moved_last}"
          return 0
        fi
      else
        zero=0
      fi
    fi
    sleep "$SLEEP"
  done
  # 发数帽到了：正常是窗还没收敛（活 fact 很多），要么二进制坏了在原地转。留一行分得清。
  log "{\"t\":\"phase\",\"pass\":\"episode\",\"family\":\"\",\"kind\":\"episode\",\"calls\":$n,\"llm_calls\":0,\"why\":\"step_cap\",\"used\":$used,\"episodes_total\":$episodes_total,\"cursor_moved\":$moved_last}"
}

# lesson 档（第 6 pass，波三刀 B，2026-11-09）：放在 episode 归组之后。
# 形状照 distill 抽取档（同一个 `hub distill once` 面），但**收批判据落在 `state` 上**，
# 理由见 `parse_lesson_readout`：`lessons=[]` 是合法空产出，`turns>0` 不代表队列在推进。
# 四条收手面：
#   state="none"          —— 选批返回 `Ok(None)`，一次模型都不调（队列空，零成本跳过）；
#   state="empty" 连续两次 —— 模型交了空数组，没写 `review_item`（`batch_key` 不落），
#                            再发一次问的还是同一组：continue 就是原地重烧，收手；
#   rc=2 连续两次          —— 抽取契约拒收同样不入队（同上，不许把"拒收"读成"推进"）；
#   rc≠0 到 `FAIL_STREAK`  —— 座位/运维失败，与 `run_distill` 同一条墙。
# 有进展的发（rc=0 且 state∈done/rejected/quarantined/pending）把两道"没留下队列行"的计数
# 清零，并把 `turns` 记进 `used`（与 distill 档同一单位：轮）。`cards`/`drafts` 只进
# `phase` 行的 `cards_total`/`drafts_total`，不进 `used`（与 derive 的 `rows_total` 同律）。
# 读不出（`state=?` 或 `turns<0`，旧二进制/坏读数）走连失败闸，不许冒充 `empty` 或 `none`。
# 已知债（不修，登记 design/2026-11-09-lesson-live-readings.md §缺陷）：空产出与抽取契约
# 拒收的那一组不落终态，**下一夜还会被重选一次**；本档只保证"不在同一夜原地重烧"，
# 不保证一次跑完整个候选队列（队头卡住时，后面的组这一夜不跑）。
run_lesson() {
  local out rc state turns drafts cards zero=0 bad=0 n=0 cards_total=0 drafts_total=0
  local call_ms=0 n_timeouts=0
  while true; do
    out=$("$HUB" distill once --pass lesson --batch-turns 10 --data-dir "$DATA" \
          --llm-config "$LLM" --llm-timeout-ms "$LLM_TIMEOUT" 2>&1); rc=$?
    log "{\"t\":\"distill\",\"pass\":\"lesson\",\"family\":\"\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    read -r call_ms n_timeouts <<<"$(parse_call_obs "$out")"
    [ "$call_ms" -gt "$MAX_CALL_MS" ] && MAX_CALL_MS=$call_ms
    TIMEOUTS_TOTAL=$((TIMEOUTS_TOTAL + n_timeouts))
    read -r state turns drafts cards <<<"$(parse_lesson_readout "$out")"
    # 与 `run_derive` 的 M-2 护栏同律：解析器没跑起来交回空串/非数字，不是 0 也不是负数。
    case ${turns:-} in ''|*[!0-9-]*) turns=-1 ;; esac
    case ${drafts:-} in ''|*[!0-9-]*) drafts=-1 ;; esac
    case ${cards:-} in ''|*[!0-9-]*) cards=-1 ;; esac
    case $state in ''|none|empty|done|rejected|quarantined|pending) ;; *) state=unknown ;; esac
    n=$((n + 1))
    if [ "$cards" -gt 0 ]; then cards_total=$((cards_total + cards)); fi
    if [ "$drafts" -gt 0 ]; then drafts_total=$((drafts_total + drafts)); fi
    if [ "$rc" -eq 2 ]; then
      # 契约拒收不是座位故障（与 `run_distill` 同一条：不记 `failed`），但它**没有队列行进账**
      # （抽取契约拒收在 `enqueue` 之前就返回）：按"没进展"计数，连续两次收手。
      bad=0; zero=$((zero + 1))
      if [ "$zero" -ge 2 ]; then
        log "{\"t\":\"phase\",\"pass\":\"lesson\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"queue_empty_or_stalled\",\"used\":$used,\"cards_total\":$cards_total,\"drafts_total\":$drafts_total}"
        return 0
      fi
      sleep "$SLEEP"
      continue
    fi
    if [ "$rc" -ne 0 ]; then
      bad=$((bad + 1)); failed=$((failed + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"lesson\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"cards_total\":$cards_total,\"drafts_total\":$drafts_total}"
        return 0
      fi
      sleep 10
      continue
    fi
    if [ "$state" = "unknown" ] || [ "$turns" -lt 0 ] || [ "$cards" -lt 0 ] || [ "$drafts" -lt 0 ]; then
      # rc=0 而读数读不出（旧二进制/坏读数）：读不出 ≠ 跑空（与 derive 的第三态同律），
      # 走连失败闸，最多烧 FAIL_STREAK 发。
      bad=$((bad + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"lesson\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"fail_streak\",\"used\":$used,\"cards_total\":$cards_total,\"drafts_total\":$drafts_total}"
        return 0
      fi
      sleep 10
      continue
    fi
    bad=0
    if [ "$state" = "none" ]; then
      log "{\"t\":\"phase\",\"pass\":\"lesson\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"queue_empty\",\"used\":$used,\"cards_total\":$cards_total,\"drafts_total\":$drafts_total}"
      return 0
    fi
    if [ "$state" = "empty" ]; then
      # 合法空产出：没有队列行进账（`drafts=0` 时不入队），再发一次问的是同一组。
      zero=$((zero + 1))
      if [ "$zero" -ge 2 ]; then
        log "{\"t\":\"phase\",\"pass\":\"lesson\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$n,\"why\":\"queue_empty_or_stalled\",\"used\":$used,\"cards_total\":$cards_total,\"drafts_total\":$drafts_total}"
        return 0
      fi
      sleep "$SLEEP"
      continue
    fi
    zero=0
    used=$((used + turns))
    sleep "$SLEEP"
  done
}

# reuse 反馈审计档（波三修 3，波二刀 B 的入队面进日路径）：把 `card_reuse_event` 里
# `failed`/`mismatch` 的卡入复核队列（`source='reuse'`，只走人工判定链 `hub review decide`）。
# 零候选零成本（不调模型、不写水位）；与 anomaly/backup 同一条 best-effort：失败只记账
# （`failed` → `end.fails`）、不止损——反馈入队是维护项，失败不该让这一夜的数据面读数作废。
# phase 行带三格（`candidates`/`queued`/`deduped`），读不出 = -1（`parse_reuse_ingest_readout`）。
run_reuse_ingest() {
  local out rc candidates queued deduped
  out=$("$HUB" review reuse-ingest --data-dir "$DATA" --json 2>&1); rc=$?
  log "{\"t\":\"reuse\",\"kind\":\"ingest\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
  read -r candidates queued deduped <<<"$(parse_reuse_ingest_readout "$out")"
  case ${candidates:-} in ''|*[!0-9-]*) candidates=-1 ;; esac
  case ${queued:-} in ''|*[!0-9-]*) queued=-1 ;; esac
  case ${deduped:-} in ''|*[!0-9-]*) deduped=-1 ;; esac
  if [ "$rc" -ne 0 ]; then
    failed=$((failed + 1))
    log "{\"t\":\"phase\",\"pass\":\"reuse\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"fail_streak\",\"used\":$used,\"candidates\":$candidates,\"queued\":$queued,\"deduped\":$deduped}"
    return 0
  fi
  log "{\"t\":\"phase\",\"pass\":\"reuse\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"ok\",\"used\":$used,\"candidates\":$candidates,\"queued\":$queued,\"deduped\":$deduped}"
}

# 全量维护档（`--full`，W26）：把 `hub anomaly sweep` 跑一次。
#
# **为什么它必须挂在日路径上**：`rc_memory::anomaly::sweep` / `sweep_ghosts` 此前只被 legacy
# `cycle once` 调用，而日路径从不跑 cycle ⇒ `attention` 只升不解除（2026-11-03 起 5 条
# `alias_ambiguity` 一直挂着，W15 身份合流之后歧义其实已经不存在，却没有一次扫描去把它解掉，
# doctor 的 `recheck_pending.open=5` 因此永久红）。
#
# **W25 的探测钩子已撤**（`anomaly sweep --help` 探不到就记 `skipped`）：命令现在真的存在，
# 探测只会把"二进制是旧的/坏了"也念成"命令还没落"——两种坏形状混成一种，而它们的修法完全不同。
# `skipped` 这个词随探测一起退休，这一族（`kind=maintenance`）只剩两个词：`ok` / `fail_streak`。
#
# 它是**一次全量扫描**，不是队列：发一次、按 rc 收。失败只记账（`failed` → `end.fails`），
# **不止损整条链**——告警复核是维护项，不是数据面；它失败不该让这一夜的抽取/审核读数作废。
# `phase` 行带四格（`contradiction_raised` / `contradiction_resolved` / `ghosts_raised` /
# `ghosts_resolved`），读者是 `summarize_drain.py` 之后的人，也是"这一轮到底升了什么、解了什么"
# 唯一可核对的形状。四格读不出来时是 -1（`parse_anomaly`），而 `why=ok` 说的是"这一发跑成了没有"：
# 两件事各有一格，混成一格就是把"成功但读数不认识"（旧二进制）念成"库是干净的"。
run_anomaly() {
  local out rc c_r c_s g_r g_s
  out=$("$HUB" anomaly sweep --data-dir "$DATA" --json 2>&1); rc=$?
  log "{\"t\":\"anomaly\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
  if [ "$rc" -ne 0 ]; then
    failed=$((failed + 1))
    log "{\"t\":\"phase\",\"pass\":\"anomaly\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"fail_streak\",\"used\":$used,\"contradiction_raised\":-1,\"contradiction_resolved\":-1,\"ghosts_raised\":-1,\"ghosts_resolved\":-1}"
    return 0
  fi
  read -r c_r c_s g_r g_s <<<"$(parse_anomaly "$out")"
  # 四格都必须能当整数用：解析器交回空串/非数字（本机 python3 是商店假身时正是这个形状）
  # 一律并进 -1 那一态，不许冒充 0（与 `run_derive` 的 M-2 护栏同一条律）。
  case ${c_r:-} in ''|*[!0-9-]*) c_r=-1 ;; esac
  case ${c_s:-} in ''|*[!0-9-]*) c_s=-1 ;; esac
  case ${g_r:-} in ''|*[!0-9-]*) g_r=-1 ;; esac
  case ${g_s:-} in ''|*[!0-9-]*) g_s=-1 ;; esac
  log "{\"t\":\"phase\",\"pass\":\"anomaly\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"ok\",\"used\":$used,\"contradiction_raised\":$c_r,\"contradiction_resolved\":$c_s,\"ghosts_raised\":$g_r,\"ghosts_resolved\":$g_s}"
}

# 备份档（W28，只在 `--full` 跑）：`hub backup` 一组五件——memory + transcript + ledger
# （必备段）+ core（agent 私房本，懒库存在才备）+ manifest。
#
# **为什么它必须挂在日路径上**：日路径此前从不调用 `hub backup`（脚本里 grep 不到），
# `usage_counter` 的 `maintenance:backup:backup_ok_at_ms` 停在 2026-11-06 10:58——那一格靠的是
# 手工/旧入口，不是保底档。保底档的职责正是"没人看着的夜晚也把可恢复性往前推一格"。
#
# **ledger/core 为什么在 hub 组里而不在脚本侧**：本函数旧版（a1c270c）在脚本里用 python
# 的 sqlite3 backup API 给 ledger/core 单独冷备（`ledger-<ts>.db`/`core-<ts>.db`，留 14 对）。
# W28 本体落地后 `hub backup` 自己就把这两库收进组（带清单读数、可校验、可轮转），脚本侧
# 的冷备整段撤除——不是"换地方干同一件事"，是它有两个必须撤的理由：
#   ① 文件名同形（`ledger-<ms>.db`/`core-<ms>.db`）：脚本冷备的 14 对轮转会**裁掉 hub 组里
#      同名的段**，manifest 说有而文件没了，restore 当场拒——两套轮转互踩；
#   ② 脚本冷备没有清单也没有校验，它有而 hub 组没有的能力为零（加密变体反而是 hub 独有）。
# 旧版产出的散对（无 manifest）hub 的轮转认不出、不会动，会在 backups/ 里自然残留，手工清。
#
# 读数面：`hub backup` 印 `name` / `memory` / `ledger` / `core` / `memory_rows` / `extras`
# 六格（成功那一发；`core` 为 null 是合法读数——私房本懒库缺席，见 `parse_backup`），
# phase 行（kind=maintenance、pass=backup）把这六格**同层照抄**；`why` 回答"这一发跑成了没有"
# （`ok` / `fail_streak`），六格回答"读数读出来了没有"（读不出 = -1，`parse_backup`）。
# 失败只记账（`failed` → `end.fails`），不止损整条链——与 anomaly 同档：备份是维护项，
# 不是数据面。
run_backup() {
  local out rc cells
  out=$("$HUB" backup --data-dir "$DATA" 2>&1); rc=$?
  log "{\"t\":\"backup\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
  if [ "$rc" -ne 0 ]; then
    failed=$((failed + 1))
    log "{\"t\":\"phase\",\"pass\":\"backup\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"fail_streak\",\"used\":$used,\"name\":-1,\"memory\":-1,\"ledger\":-1,\"core\":-1,\"memory_rows\":-1,\"extras\":-1}"
  else
    cells=$(parse_backup "$out")
    # 解析器自己没跑起来（本机 python3 是商店假身时正是这个形状）会交回空串：那不是 0，是读不出，
    # 与 `parse_anomaly` 的四格护栏同一条律——不许把空串拼进 JSON（那一行会当场非法）。
    [ -n "$cells" ] || cells='"name":-1,"memory":-1,"ledger":-1,"core":-1,"memory_rows":-1,"extras":-1'
    log "{\"t\":\"phase\",\"pass\":\"backup\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"ok\",\"used\":$used,$cells}"
  fi
}

# 时间回填：纯程序、不调模型；必须在 fact 审核落库之后。
# 它把 time_expr 归一成 fact 的时间列（event_time/validity/deadline → occurred/valid/due）；
# 解析不出的落 stale(unresolved)，可被解析器升级后重新选中。
run_time() {
  # 时间回填不调模型，但它是**二进制**在读：坏库/坏二进制会在 50 步里每步等 10 秒（500 秒空转）。
  # 所以同一道连失败闸也给它——失败原因与抽取不同族，`why` 用的是同一个词，日志里一眼可分。
  local out rc turns zero=0 step=0 bad=0
  while [ "$step" -lt 50 ]; do
    step=$((step + 1))
    out=$("$HUB" distill once --pass time --batch-turns 8 --data-dir "$DATA" 2>&1); rc=$?
    log "{\"t\":\"distill\",\"pass\":\"time\",\"family\":\"\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
    turns=$(parse_turns "$out")
    if [ "$rc" -ne 0 ]; then
      bad=$((bad + 1)); failed=$((failed + 1))
      if [ "$bad" -ge "$FAIL_STREAK" ]; then
        log "{\"t\":\"phase\",\"pass\":\"time\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$step,\"why\":\"fail_streak\",\"used\":$used}"
        return 0
      fi
      sleep 10
      continue
    fi
    bad=0
    if [ "$turns" = "0" ]; then
      zero=$((zero+1)); [ $zero -ge 2 ] && return 0
    else
      zero=0
    fi
    if [ "$turns" -lt 0 ]; then sleep 10; fi
    sleep "$SLEEP"
  done
  # 50 次还没跑空：要么每批都有新东西（正常推进），要么二进制坏了在原地转。留一行分得清。
  log "{\"t\":\"phase\",\"pass\":\"time\",\"family\":\"\",\"kind\":\"distill\",\"calls\":$step,\"why\":\"step_cap\",\"used\":$used}"
}

# G13 盖章链修复（W37-A2）：蒸馏**前置**对无章会话按环境声明补章——
# `hub principal claim-env`（入口与导入路径同一份 `PrincipalDecl`，章 `source='agent_runtime'`，
# 幂等：已有有效章的会话一格不动，重跑同一库 written=0）。缺章会话蒸出的行会永久缺
# principal（章后补的会话列保持 NULL，"同主人互读"对老记忆失效），所以这一枪必须排在
# 第一族抽取（run_distill naming）之前。声明缺席（`RECOLLECT_PRINCIPAL_NAME` 没设）⇒
# 命令 rc=0、全零读数，**不是故障**；命令失败只记账（`failed` → `end.fails`）、不止损：
# 补不上章的夜晚抽取照旧跑（蒸出的行缺章是老行为，停一整夜抽取更坏）。
# 回执两格（`sessions_scanned` / `claims_written`）落 phase 行；读不出 = -1（与
# `parse_anomaly` 的四格同一条律：不替坏读数冒充 0）。
run_preclaim() {
  local out rc scanned claimed
  out=$("$HUB" principal claim-env --data-dir "$DATA" --json 2>&1); rc=$?
  log "{\"t\":\"preclaim\",\"rc\":$rc,\"used\":$used,\"out\":$(printf '%s' "$out" | jstr)}"
  if [ "$rc" -ne 0 ]; then
    failed=$((failed + 1))
    log "{\"t\":\"phase\",\"pass\":\"preclaim\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"fail_streak\",\"used\":$used,\"sessions_scanned\":-1,\"claims_written\":-1}"
    return 0
  fi
  read -r scanned claimed <<<"$(printf '%s' "$out" | python3 -c 'import sys,json
head = sys.stdin.read().strip().split("\n", 1)[0]
try:
    d = json.loads(head)
except Exception:
    d = {}
def num(k):
    v = d.get(k, -1)
    return v if isinstance(v, int) and not isinstance(v, bool) else -1
print(num("sessions_scanned"), num("claims_written"))')"
  # 解析器交回空串/非数字（本机 python3 是商店假身时正是这个形状）一律并进 -1 那一态，
  # 不许把空串拼进 JSON（那一行会当场非法）——与 run_backup 的 cells 护栏同一条律。
  case ${scanned:-} in ''|*[!0-9-]*) scanned=-1 ;; esac
  case ${claimed:-} in ''|*[!0-9-]*) claimed=-1 ;; esac
  log "{\"t\":\"phase\",\"pass\":\"preclaim\",\"family\":\"\",\"kind\":\"maintenance\",\"calls\":1,\"why\":\"ok\",\"used\":$used,\"sessions_scanned\":$scanned,\"claims_written\":$claimed}"
}

# 抽取：各 pass 各自跑到自己的队列空（turns=0 连续两次）或连续失败。**没有额度**：
# 排在前面的族队列还没空，后面的族这一夜就不开跑——顺序本身就是取舍（见文件顶部那条后果）。
# G13 盖章链修复（W37-A2）：蒸馏**前置**对无章会话按环境声明补章（`run_preclaim`，
# 在第一族抽取之前）——缺章会话蒸出的行会永久缺 principal，所以这一枪必须排最前。
run_preclaim
run_distill naming ""
run_distill event ""
run_distill fact decision
run_distill fact promise
run_distill fact plan

# naming 审核必须在 state 抽取之前：锚只有过审才存在（state 抽取读的是已过审的锚，
# 排在前面的档没跑完时它照样会按自己的队列空收手，并在 `phase` 行留下读数，不静默）。
run_review naming
run_distill state ""

# 其余各族审核：把抽出来的待审项落地
run_review event
run_review fact
run_time
run_review time
# state 的审核放队尾（方案 §6：与抽取分成两次调用，队列里等的是上一轮的草稿）。
run_review state

# 推导档放全部审核之后（L3 是组合阶段：依据行必须已经定局，见文件顶部那条硬顺序）。
run_derive
# episode 归组放 derive 之后（读活 fact 行；同为纯程序段、0 调用，见文件顶部）。
run_episode "$EPISODE_CALL_CAP"
# lesson（第 6 pass，波三刀 B）：在 episode 之后；只吃"已判错"的轮，队列空零成本跳过
# （`state=none` 那一发在选批处返回，一次模型都不调）。收手面见 `run_lesson` 函数头。
run_lesson
# reuse 反馈审计（波三修 3）：failed/mismatch 入复核队列；零候选零成本。
run_reuse_ingest
# 日保底档（`--full`）比常规抽干多出来的两档：anomaly sweep（W26 落）与 backups（W28 落，
# 钩子见两个函数的函数头）。顺序照派发档：sweep 在前、备份在后；两者都只记账、不止损。
if [ "$FULL" -eq 1 ]; then
  run_anomaly
  run_backup
fi

# §11.7：把本轮的终态分布（含 reason）落一行读数——`uncovered`/`not_utterance` 等逐条可查，
# 不再只有计数。只读，不改库。
log "$(python3 - "$DATA" <<'PY'
import json, sqlite3, sys
root = sys.argv[1]
c = sqlite3.connect(root + "/ledger.db")
rows = c.execute(
    "select pass_id, family, state, coalesce(reason,'') as reason, count(*) "
    "from extract_pass_state group by 1,2,3,4 order by 1,2,3,4").fetchall()
print(json.dumps({"t": "state_snapshot",
                  "rows": [[p, f, s, r, n] for p, f, s, r, n in rows]}, ensure_ascii=False))
PY
)"

# §11.7：同一份终态再落「逐轮 uid」，让 uncovered / not_utterance / dropped 能按 uid 回查原文；
# 每组最多 2000 个 uid（超出记 truncated）。只读，不改库。
log "$(python3 - "$DATA" <<'PY'
import json, sqlite3, sys
from collections import defaultdict
root = sys.argv[1]
c = sqlite3.connect(root + "/ledger.db")
groups = defaultdict(list)
for p, f, s, r, u in c.execute(
        "select pass_id, family, state, coalesce(reason,''), turn_uid "
        "from extract_pass_state order by pass_id, family, state, reason, turn_uid"):
    groups[(p, f, s, r)].append(u)
out = [{"pass": p, "family": f, "state": s, "reason": r,
        "n": len(v), "truncated": len(v) > 2000, "turn_uids": v[:2000]}
       for (p, f, s, r), v in sorted(groups.items())]
print(json.dumps({"t": "state_uids", "groups": out}, ensure_ascii=False))
PY
)"

log "{\"end\":\"$(date -Is)\",\"used\":$used,\"fails\":$failed,\"quarantined\":$quarantined_total,\"max_call_ms\":$MAX_CALL_MS,\"timeouts\":$TIMEOUTS_TOTAL}"
exit 0
