# 中心运维流水线

## `daily-drain.sh`

每日抽干（**十七档**，实际顺序以脚本里的调用序列为准）：
`preclaim 补章（G13）→ naming 抽 → event 抽 → fact 抽（decision/promise/plan 三族）→ naming 审 → state 抽 → event 审 → fact 审 → time 回填 → time 审 → state 审 → derive 推 → episode 归组 → lesson 记错（第 6 pass，2026-11-09）→ reuse-ingest 复用反馈入队`。
**保底档（`--full`）**在这一串之后再追加两档维护：`anomaly sweep`（W26）与 `backup`（W28，见下面「备份」一节）；
普通触发档（无 `--full`）不跑这两档。
episode 归组（L2，2026-11-04 I-6b 加档）排在 derive 之后：它归组的是**活 fact** 行（写
`episode`/`episode_fact` 与镜像卡），与 derive 不构成第二条抽取入口；lesson 与 reuse-ingest 再排其后。

- **没有夜间预算**（2026-11-06 用户裁定："调用配额、夜间预算这两个是预算，其他不是"）：脚本里既没有
  总额闸，也没有按族切百分比的那张表（旧的总闸变量与九个按族百分比一张不剩）。各档按脚本里的
  顺序**跑到自己的队列空**（连续两次空批）或**连续失败**（`FAIL_STREAK`）才进下一档。
  字面后果：排在前面的 naming 队列够长就占满整夜，后面几档当晚一次都不开跑；要公平得另做调度器
  （轮转、固定批次数），脚本不拿一个新额度参数顶替。
  `used` 仍逐档累加，但**只记账**：不判哪一档能不能开跑，也不判何时停（成本读数，不是闸）。
- 剩下的两个帽子都**不是预算**：`EPISODE_CALL_CAP` = 20 数的是 `hub episode run` 的**发数**
  （防坏二进制原地空转，与 `run_time` 的 50 步帽同族），不是产出额度。
- **`used` 的单位仍混着三种**（已知债，见脚本顶部）：derive 记「次」（一次 `hub derive run --max-batches 1` = 1，
  落库行数不进账、只累计进 `phase` 行的 `rows_total`）、抽取记「轮」、审核记「条」；
  **episode 记 0**（纯程序零调用，`used` 在这一档纹丝不动）。
  ⇒ derive 那一档 `calls=10` 就是 `used +10`，与 `rows_total` 是几无关；episode 那一档
  `calls` 是几，`used` 一格都不动。
- **derive 与 episode 各按自己的队列收手**：episode 的循环条件只有发数帽（零模型调用 ⇒ `used` 不动）；
  derive 收在"空批 + 游标不动"连续两次或连失败闸上。两档都不看别的档花了多少——那个总额闸已经不在了。
- `rows` 含 mention 与 I6 的别名补铸，**一批落库可以超过批宽 40**（副本库实测一批 51 行、一条边没落）：
  批宽 `DERIVE_MAX_ROWS_PER_BATCH` = 40 数的是**选进来的输入行数**，不是落库行数。
  读数拆不开"索引 vs 产出"（`hub derive run --json` 没有 `mentions_written` 那一格），
  所以 `rows_total` 只说"落了几行"，不许读成"产出了几条边/卡"。
- **time 回填是纯程序，不调模型**；顺序不能换：锚只在 naming 审核通过时落库，event/fact 需要已审锚，
  time 只回填已落库的 fact，state 只能引用已过审落库的锚，derive 读的是**已定局**的组合行（所以放队尾）。
- `time` 的 `review` 是空操作（time 没有审核面），保留是为了终态读数一致。
- derive 的 `phase` 行另带三格拒收分诊（`rej_no_mechanism_total` / `rej_no_evidence_total` /
  `rej_anchor_gate_total`）：**这三格是子集**，与总格 `rejected` 只有不等式关系（恒等式在五格上，住在
  `hub derive run --json` 那一面）；任何一发读不出来 ⇒ 三格永久标 -1，不复活成部分和。
- episode 归组的读数面（`hub episode run --json`，键面在 `crates/hub/src/main.rs` 的 `episode_run`）：
  `schema_version / job / batch / candidates / merged_pairs / span_pruned / episodes_written /
  members_linked / cards_written / superseded / dropped_members / mid_band_deferred / cursor_moved / lag_rows`。
  日路径的 `phase` 行（kind=episode）带 `llm_calls:0`（模型调用数，恒 0——写出来是让"这一档没花成本"
  可核对）、`episodes_total`（`episodes_written` 的累计值）、`cursor_moved`（最后一发的游标是否动：
  1 动 / 0 没动 / -1 读不出）。**收批判据落在游标上**：`episodes_written=0` 同时是"没新对"与
  "整段已在库里"，只有 `cursor_moved` 能分开——空批 + 游标不动连续两次才叫跑空（与 derive 同律）。
  归组的窗是"从库头逐趟放大整段重算"，旧段按内容寻址命中 `INSERT OR IGNORE`；**游标不落库**
  （`job_watermark.family` 的 CHECK 是闭集，新增一族是一次 schema 变更，本轮不做）⇒ 跨运行的
  收敛靠幂等，不靠持久水位。
- 人读面（2026-11-04 I-6b 更新）：`summarize_drain.py` 现在认得 **episode 档**——`t:"episode"`
  的事件行归堆（`work` = `episodes_written` 累计，`merged_pairs` / `cards_written` 抄进备注）、
  `phase` 行补印 `episodes_total` 与 `cursor_moved`。保底档的两档维护也认（W26/W28 补齐）：
  `t:"anomaly"` 归堆（`work` = 升起的告警数，解除数进备注）、`phase` 行（kind=`maintenance`）
  补印四格；`t:"backup"` 归堆（`work` = `memory_rows`，`name`/`memory`/`extras` 进备注；
  W28 本体落地后事件行多出 `ledger`/`core` 两格，人读面暂不补印——`phase` 行里有）。
  脚本侧的 `t:"cold_backup"` 行已随 W28 本体退休（ledger/core 改由 `hub backup` 组内产出），
  `summarize_drain.py` 的同名分支从此不命中，无害。**derive 档的人读面仍欠**（登记在 I-3b：
  `kind":"derive"` 的事件行不归堆、`rows_total` / `rej_*_total` 不补印）。
  仓库里的规矩是"加一档须同批补三处：脚本档 + 回归桩臂 + 人读面"——episode / anomaly / backup 三处已齐；
  **lesson 与 reuse-ingest 两档未兑现**（`test_drain_schedule.sh` 没有这两族的桩臂，`summarize_drain.py`
  也不归堆这两族），是已知债。

### 备份（`--full` 的 `run_backup`，W28）

一件事，只记账、不止损整条链（与 `anomaly sweep` 同档：备份是维护项，不是数据面）：

1. **`hub backup --data-dir $DATA`** —— 一组五件：memory + transcript + **ledger（必备段）** +
   **core（agent 私房本，懒库存在才备；缺席时清单/报告如实记 `core: null`）** + manifest，
   文件名只有时间戳，留最近 7 组由 hub 自己轮转（W28 本体落地前脚本侧的 python 冷备
   `run_cold_backup` 已撤——它的 14 对轮转与 hub 组同抢 `ledger-<ts>.db`/`core-<ts>.db`
   文件名会互踩，且它没有清单也没有校验；旧版产出的散对 hub 轮转认不出，会在
   `backups/` 里自然残留，手工清）。六格读数 `name` / `memory` / `ledger` / `core` /
   `memory_rows` / `extras` 照抄进 `phase` 行（`kind=maintenance`、`pass=backup`）；
   `why` 只有 `ok` / `fail_streak` 两词，读不出来时各格是 `-1`（`parse_backup`，与
   anomaly 的四格同律；`core: null` 是合法读数——懒库缺席，原样落行）。
   注意 `hub backup` **没有 `--json` 旗标**：它总是印这一行，传 `--json` 只会被判成用法错误。
   **`keys/` 目录一个字节都不进组**：身份类密钥（`core_<h>.key` 等）不下发的既有裁定，
   多一份静态拷贝 = 多一类出机器面。

**恢复走 `hub restore`**（带清单校验：账本的版本门/行数/摘要、core 的 agents 数/摘要、
记忆库的 digest，坏备份在校验步就被拒）。旧清单（无 ledger/core 段）照旧只恢复
transcript/memory 两件，活库的账本/私房本不被碰。

### 部署

```bash
cp tools/pipeline/daily-drain.sh ${HOME}/.recollect/data/bin/daily-drain.sh.new
bash -n ${HOME}/.recollect/data/bin/daily-drain.sh.new
cp ${HOME}/.recollect/data/bin/daily-drain.sh ${HOME}/.recollect/data/bin/daily-drain.sh.bak-$(date +%Y%m%d-%H%M)
install -m 755 ${HOME}/.recollect/data/bin/daily-drain.sh.new ${HOME}/.recollect/data/bin/daily-drain.sh
```

### 环境变量

`RECOLLECT_DATA_DIR`（默认 `${HOME}/.recollect/data`）、`RECOLLECT_HUB`、`RECOLLECT_LLM_CONFIG`、
`RECOLLECT_DEVICE_ID`、`RECOLLECT_LOG`（另有两个墙/退避格：`RECOLLECT_LLM_TIMEOUT_MS`、
`RECOLLECT_DRAIN_SLEEP`、`RECOLLECT_DRAIN_FAIL_STREAK`——都不是额度）。

### 已知语义缺口（2026-10-31）

`time_kind_for` 只把带「之前/以前/截止」的承诺/计划判成 `deadline`；普通的
「明天交报告」会落成 `validity`（`valid_from`），**不是 `due_at`**。
所以 `agenda` 目前只对显式截止语有效。要不要把"带日期的承诺默认当截止"是单独一次判定，
不要在脚本层猜。

## `collect-qoder.sh`（会话采集，W27）

把 Qoder 工作区里的会话文件采进中心：`*.jsonl` → `tools/converters/qoder_v1.py`（v1）→
`hub import --data-dir $DATA --json`。**只写 transcript**（不启 LLM、不碰别的表），一次运行
最多碰几个文件由帽决定，所以它适合每 10 分钟被叫一次。

### 顺序

```
collect-qoder.sh           ← 这一条：把新会话变成 transcript 行
drain-if-pending.sh        ← 有待办才抽干（W25 的闸，只蒸**已入库**的轮次）
```

采集与抽干**分离**：本脚本不调 `daily-drain.sh`。顺序由控制器的计划任务决定（建议先采后蒸）。
先蒸后采的话，新采进来的轮要等到下一次触发才有机会被蒸。

### 状态文件

`$DATA/ingest/qoder-state.json`（`RECOLLECT_INGEST_STATE` 可换路径）：

```json
{"files": {"<绝对路径>": {"mtime_ms": 1793993605485, "size": 82832, "last_turns": 2, "offset": 82832}},
 "daily": {"2026-11-07": {"cap": 300, "used": 2}}}
```

- `files` 是**逐文件水位**：只有 `mtime_ms` **与** `size` 都和记录一致才跳过；
  文件被追加（size 变了）或重写（mtime 变了）都会再处理一次。
- `offset`（A6）= 上次成功入库消费到的**字节偏移**，永远落在完整行边界（段内最后一个 `\n` 之后；
  末行没有 `\n` 时仅当它整体是合法 JSON 才越过）：增量路径**只把 `[offset, EOF)` 的段**喂转换器，
  不再整份重转。偏移越界（文件变小/截断）归 0 ⇒ 整文件重转一次，旧轮靠 `turn_uid` 幂等
  （实测同一份重导 `skipped_duplicate` = `turns`）。
- `last_turns` = 到这个文件为止**累计**入库的 **human+agent 轮数**。两条路径的"新增"各算各的：
  增量路径 = 增量段里的 human+agent 轮数；全文件路径（首采/截断回退）= 总数 − `last_turns`（负数按 0 记）。
  **日轮帽数的就是这个"新增"**，不是 `turns`：
  一个 13MB 的活动会话是 39 human + 199 agent + 2404 system，按总轮数计帽会一步打满。
- `daily` 按日期记帽值与本日已用，跨多次运行累计（10 分钟一次的调度不会把帽重置）。
- 路径统一成 `abspath` + 正斜杠。Windows 上这一步是必须的：git bash 会把 `/c/…` 参数转成
  `C:/…`，状态键、`basename`、python 的 `stat` 必须落在同一个字符串上，否则水位永远对不上。

### 两道帽（都不是预算）

| 变量 | 默认 | 含义 |
|---|---|---|
| `RECOLLECT_INGEST_DAILY_TURNS` | 300 | 本日**新增 human+agent 轮**的上限（`used >= cap` 就不再碰后面的文件） |
| `RECOLLECT_INGEST_FILES_PER_RUN` | 3 | 一轮最多碰几个文件（含失败的尝试） |

撞上任何一道帽，`ingest_summary` 的 `capped` 就是 `true`；已处理的文件正常记。
帽只决定"这一轮还采不采下一个"，不决定"采进来的轮怎么蒸"。

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `RECOLLECT_DATA_DIR` | `${HOME}/.recollect/data` | 中心库目录 |
| `RECOLLECT_HUB` | `$DATA/bin/hub-*` 里 mtime 最新的那个 | 二进制 |
| `RECOLLECT_INGEST_DIRS` | `$HOME/.qoder-cn/projects/D--recollect` | 冒号分隔多目录 |
| `RECOLLECT_INGEST_STATE` | `$DATA/ingest/qoder-state.json` | 水位文件 |
| `RECOLLECT_INGEST_CONVERTER` | `tools/converters/qoder_v1.py` | 仅测试用（桩转换器） |
| `RECOLLECT_LOG` | `$DATA/logs/ingest-<date>.jsonl` | 事件行（同时也打到 stdout） |

### 输出与退出码

每处理一个文件一行，跑完一行 summary（未变化的文件**一行都不打**——10 分钟一次的调用若
每次都留行，真正的采集行会被噪声淹掉）：

```
{"t":"ingest","file":"<basename>","turns":N,"accepted":N,"dup":N,"rejected":N,"new_turns":N,"used":N}
{"t":"ingest_summary","files":N,"accepted":N,"dup":N,"capped":false,"new_turns":N,"used":N}
{"t":"ingest_error","file":"<basename>","stage":"convert|hub|hub_parse","rc":N}
```

- 转换失败 / 交不出 v1 / `hub import` 非零 / 读数解析不出来 ⇒ 一行 `ingest_error`，
  **不更新那个文件的水位**（下一轮重试）。整轮照常跑完，退出 0。
- 退出码只有两种：`0` = 跑完（哪怕每个文件都失败）；`3` = **状态文件读不出/写不进**。
  水位不可信时不猜：把"读不出来"念成"全部没采过"会把同一批会话重采一遍并打乱日帽。
- 用法错误（帽值不是非负整数、找不到 hub/转换器）⇒ 退出 2。

### 已知边界

- 只扫 `RECOLLECT_INGEST_DIRS` 的**顶层** `*.jsonl`。子 agent 的 `<id>/subagents/*.jsonl` 不单独采
  ——它们的内容本来就在主会话文件的 `isSidechain` 记录里（转换器按 sidechain 归组、不重复计人）。
- 会话文件正在被写时读到的最后半行会记成 `unparsable` 跳掉；下一轮 size 变了会整份重转，
  重导靠 `turn_uid` 去重，不会重复落行。
- 转换器对"只有 meta 行、没有 user/assistant"的文件会非零退出 ⇒ 这类文件每轮都会落一行
  `ingest_error`（按派发档的纪律：不推进水位、下轮重试），不是水位失效。
- 事件行里的 `file` 是 basename，直接内插、不再二次转义：假定会话文件是 Qoder 的 uuid 命名
  （不含引号与反斜杠）。真出现怪名字时那一行会变成非法 JSON，采集本身不受影响（水位照记）。
- `hub import` 自己会顺手 bump ledger 的 `ingest_turns_*` 设备计数（导入面既有行为）：
  本脚本不额外写任何表，但"采一次"在 ledger 上会留下计数，别把它读成脚本写了别的表。
- `hub import` 的退出码 2 是整批拒收（`batch_reject` 有 slug），这里按失败处理：不推进水位，
  让下一轮重试，避免一次坏转换把水位推到一个再也不会重试的位置。

### 测试

```bash
bash -n tools/pipeline/collect-qoder.sh
PATH=/tmp/py3shim:$PATH bash tools/pipeline/test_collect_qoder.sh
```

桩闸钉十三件事：新文件采一次 / 同 mtime+size 不重复 / 增长再采且 `new_turns` 只数新增的
human+agent / 日帽=1 时第二个文件连 hub 都不碰 / 文件数帽 / hub 非零与读数坏都不推进水位 /
转换失败不推进水位 / 多目录（冒号分隔）/ 状态文件不可用退出 3 + 日志每行合法 JSON /
A6 增量三臂：同会话追加只喂增量段且偏移推到文件尾 / 文件变小偏移归 0 整文件重转 /
半行不越过（偏移停在行边界，行写全后下一轮恰好补采）。

