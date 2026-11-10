# 会话转换器（adapter 侧）

把宿主原生会话文件转成 `ExchangeFile v1`，**按段保真**：一段话一轮、工具调用/结果各成 system 轮、
两类"伪装成 user 的系统内容"改判（压缩摘要、子 agent 完成回执）。

## 用法

```bash
python3 tools/converters/qoder_v1.py <session.jsonl> --out-dir <dir> \
  [--device local-qoder] [--source-agent qoder] [--machine local] [--project recollect] \
  [--project-mode fixed|dir]
```

`--project-mode dir`：按会话记录里的 `directories[0]` 取工作区 basename 作为 `project_hint`
（非法字符换 `-`，空则回落 `qoder`）。采集器配置默认用它，19 个 Qoder 工作区各自成 scope。

输出 `<stem>.v1.json` + `<stem>.manifest.json`（逐类计数 + sha256）。

## 元素闭集（与台账 §10.84 一致）

| 源 | 落成 |
|---|---|
| `type=user/assistant` 的 `text` | `human` / `agent` |
| `content[].tool_use` | `system`，`[tool_call name=… id=…]` |
| `content[].tool_result`（可 `is_error`） | `system`，`[tool_result[ error] for=…]` |
| `isCompactSummary`（源里是 user 轮） | `system`，正文以 `[[compact_summary]]` 开头 |
| `[SYSTEM NOTIFICATION…]` / `<task-notification>`（源里是 user 轮） | `agent` + `speaker="subagent:<编码>"` |
| `isSidechain` 的 agent 文本 | `agent` + `speaker="subagent:sidechain-<n>"` |
| `type=system` | `system`，`[system:<subtype>]`（压缩边界带 token 读数） |
| `thinking` | 丢弃（计数） |
| 其余元数据（attachment/active-leaf/…） | 不产轮（计数） |

## 纪律

- 不摘要、不改写、不合并、不按长度截断、不抽样；确定性（同一输入逐字节相同）。
- 脱敏不在这里做——核心 `hub import` 已实现值级脱敏（身份用原文算）。
- 本目录属于 adapter 侧，**不依赖核心库**；契约变化时同步更新本 README 与台账。

## 其他来源

### DSH（小鲸鱼）

```bash
python3 tools/converters/dsh_v1.py <session.v4.jsonl.zstd> --out-dir <dir> \
  [--project personal] [--project-mode fixed|dir] [--domain life] \
  [--device local-dsh] [--source-agent dsh-jingyu] [--machine local] \
  [--principal-id p-xxx] [--principal-name NAME]
```

- `user/message` → `human`；`assistant/message` 的 `text` → `agent`（`reasoning` 丢弃）；
  `tool/call` / `tool/result` → `system`（结果超过 8000 字截断并标 `[truncated]`）；
  `system/message`（人格/系统提示）丢弃并计数；图片只留 `[image]`，不留路径。
- 输出按 session id 命名（DSH 源文件名都叫 `session.v4.jsonl`，不能按文件名）。
- `--project-mode dir`：按会话头里的 `cwd` basename 推逻辑项目名（采集器的 DSH 源用这个）。
- **整文件源（采集器 `kind = "whole"`）**：DSH 的会话是 zstd 压缩的**整份**事件流，
  字节偏移增量对它没有意义（不能切压缩流）。采集器因此把**源文件原始路径**交给转换器
  （`{segment}` = 源文件路径，不复制、不切段），转换器自己读它、写 v1 到 `{out}`；
  水位记 `{mtime_ms, size, head_sha256}`，文件变了就整份重转，旧轮由 hub 的内容哈希
  幂等键吸收成 `duplicate`（不会重复入库）。
- 换行只在 `\n` 处切（不按 `splitlines()`）：JSONL 的行分隔符只有换行，而 `splitlines()`
  还会在 U+0085 / U+2028 等字符处断行——这些字符能合法出现在 JSON 字符串里，断出来的
  碎片不是合法 JSON，整份会话就废了（本机 125 个会话文件里就有 2 个含 U+0085）。
  单行解码失败只丢这一行（计数），不拖垮整份会话。

### Hermes（会话库 sqlite，仓库内 `hermes_v1.py`）

```bash
python3 tools/converters/hermes_v1.py <out.json|out_dir> --db <state.db> \
  [--since-ms N] [--device local-hermes] [--source-agent hermes-agent] \
  [--machine local] [--project hermes] [--sources cli,feishu] \
  [--include-archived] [--principal-id p-xxx] [--principal-name NAME] [--limit N]
```

- 源：`~/.hermes/state.db`（只读 URI）；`sessions` + `messages` 两张表。
- **role 决定身份**：`user` → `human`；`assistant` → `agent`；`tool` / `session_meta` →
  不产轮（只计数）。`display_kind='internal_notification'` / `'process_complete'` 的 user 轮
  其实是子 agent 的结论 ⇒ 落成 `agent` 轮并带 `speaker='subagent:<deleg_id>'`。
- **`--since-ms` 按消息时间过滤**（不是会话时间）：只保留 `created_at_ms > N` 的消息；
  一个会话若一条这样的消息都没有，就**不产出**该会话。这样水位与转换器同源，增量不漏不重。
- 毫秒换算用 `int(ts*1000+0.5)`：与 SQL 侧的游标
  `CAST(timestamp*1000+0.5 AS INTEGER)` 同一个公式（两边差 1 毫秒，边界消息会忽进忽出）。
- `--principal-id` 不传 ⇒ 身份三格缺席（不盖章）。
- 确定性：会话按 id 排、消息按 (timestamp, id) 排 ⇒ 同一输入两次输出逐字节相同。

### LCCC（中文对话，MIT）

```bash
python3 tools/converters/lccc_v1.py <lccc.jsonl.gz> --out <file.v1.json> [--limit 100] [--seed 7]
```

- LCCC 文本是字符级分词（"我 饿 了 。"），转换器去掉字符间空格。
- 每段对话按奇偶交替映射成 `human` / `agent`；一段一个 session，全部写进一个 v1 文件。

### Tieba（Hanversion/Tieba-SomeInteresting，Apache-2.0）

```bash
python3 tools/converters/tieba_v1.py <file.json...> --out <file.v1.json> \
  [--limit 100] [--per-session 3]
```

- `question` → `human`，`answer` → `agent`，`think`（LLM 生成的思考链）整段丢弃。
- 源没有会话边界；默认把同吧连续 3 帖合成一个 session（`--per-session 1` 退回一帖一会话）。
- 低质集中在孙笑川吧（辱骂/广告），按吧抽样时注意权重；PII 密度很低但仍交给 import 再扫。

### MiniMax Code

```bash
python3 tools/converters/minimax_v1.py <sessions 根目录 | messages.jsonl> \
  --out-dir <dir> [--limit 30]
```

- `user` text → `human`（先剥 `<system-reminder>`/`<agent-context>`/任务通知）；
  `assistant` text → `agent`；`toolCall`/`toolResult` → `system`；`thinking`/custom 丢弃。
- **转换器层脱敏**：邮箱/手机/sk-/ghp_/AKIA/bearer/kv/中文口令/URL userinfo，替换为占位符。
  核心 `hub import` 不覆盖邮箱与手机号，所以真实语料必须在这一层先挡。



### MiniMax Code（v2 sqlite，仓库内 `mcode_v1.py`）

```bash
python3 tools/converters/mcode_v1.py <out.json|out_dir> --db <runtime-state.sqlite> \
  [--since-ms N] [--agent mavis] [--device local-mcode] [--source-agent mcode] \
  [--principal-id p-xxx] [--no-subagents] [--include-archived]
```

- 源：`~/.minimax/v2/sqlite/runtime-state.sqlite`（只读 URI）；`local_runtime_sessions` +
  `local_runtime_message_rows`。**`role` 决定身份，`msg_type` 不决定**（`assistant` 即使
  `msg_type=1` 也是 agent）。
- 子代理（`session_kind='task'`）独立成 session_key，派发提示记 `role='system'`，每轮带
  `speaker='subagent:<session_id>'`；压缩事件与 todo 事件跳过。
- `--since-ms` 按**消息**时间过滤（不是会话时间）；首跑由采集器把水位 seed 到当前最大值。
- `--principal-id` 不传 ⇒ 身份三格缺席（不盖章）。
- 输送前脱敏：邮箱/手机/sk-/ghp_/AKIA/bearer/kv/中文口令/URL userinfo。
