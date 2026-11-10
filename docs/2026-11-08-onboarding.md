# recollect 接入指南（外部 agent 版）

写给要通过 `recollect-mcp` 接入记忆库的 agent（或它的宿主）。只讲**工具面做什么、
自查怎么走**；部署细节、密钥与数据目录布局不在本文——那些是操作员的活。

工具名以 `tools/list` 返回的清单为准；本文写的工具名与参数名都是字面值。

## 1. 接入前置：先把主语定下来

core 层工具（`core_*`）要求当前 agent 在 `core.db` 里有注册行。**首次调 `core_*` 会自动注册**
（幂等，不产内容）：分派口先补注册行再执行，所以有主语时不会因为"没有注册行"回
`validation:unregistered`。只有诊断面的 `recollect_whoami` 如实报 `registered=false`——
它刻意不注册，也不替你把"未注册"抹掉。

手动注册仍是入口（换主语、预置身份时用），命令：

```
recollect-core --data-dir <dir> register <agent_id>
```

注意两点：
- `<agent_id>` / `<dir>` 是**占位符**，换成你的主语与数据目录；命令原文不回显任何你的输入。
- `--data-dir` 是 `recollect-core` 的**父级参数**，必须写在 `register` 之前——
  顺序反了 clap 在解析期直接拒，照抄才会成功。

注册是幂等的；重跑不会改坏任何东西。`mcp` 的启动参数 `--agent-id` 决定 core 层
以谁的身份干活，没给且宿主也报不出名字时，core 工具会要求
`restart mcp with --agent-id`——先把主语定下来再调。

## 2. 工具发现

- 清单唯一来源是 `tools/list`。每个工具都带 `title` / `description` / `inputSchema`。
- **宿主通常在会话开始时缓存工具清单**：中途换装新二进制后，新工具（G15 的现场）
  往往要**开一个新会话**才会出现在 `tools/list` 里。看不到新工具先开新会话，不要反复重试旧会话。
- 参数里**永远没有 `agent_id`**：身份由运行时绑定，参数里给了 `agent_id` 会被
  `agent_id_forbidden` 拒掉并留痕。

## 3. 三个自查入口

| 问题 | 工具 |
|---|---|
| 我是谁、core_brief 该传哪个 user_id？ | `recollect_whoami`（无参数） |
| 库现在什么状态（计数、滞后、attention）？ | `recollect_status`（`view="counts"` 拿轻量计数页） |
| 我导入的会话到了哪一步？ | `recollect_ingest_status`（`session_key` 必填） |

### 3.1 whoami：身份四格

`recollect_whoami` 回 `agent_id` / `device_id` / `principal_id` / `primary_user`。

- `primary_user` 是 core.db 里 `is_primary=true` 的主用户画像，形状
  `{"user_id": …, "profile_text": …}`——**这个 `user_id` 就是 `core_brief`
  必填参数要的值**。core_brief 的首参没有别的发现途径，先 whoami。
- `registered=false`：这个 agent 还没注册。回执**不是故障**（isError=false），
  会带 `register_hint` 字段——就是 §1 那条可粘贴命令。
- `registered=true` 而 `primary_user=null`：注册过但还没写主用户画像。
  用 `core_subject_update`（`{"subject":{"subject_id":…,"is_primary":true,…}}`）补一个。

### 3.2 status：`view=counts` 是计数页

`recollect_status` 缺省返回完整诊断对象（含工具能力声明与实体目录）。
只要计数就传 `view="counts"`：各表行数、滞后（`distill_lag_turns`）、
`attention_open`、freshness、索引状态等**计数格**照旧，
不带工具名清单、不带实体目录（`entities*` 三格不出现）。
可选 `turn=<64 hex turn_uid>` 反查单轮抽取态（挂在 `extraction` 键下）；
`entity=<名字或别名>` 筛实体目录（与 `turn` 互斥、与 `view=counts` 也互斥，
越界组合就地拒，不静默忽略）。

### 3.3 ingest_status：导入→可查的两阶段读数

`recollect_ingest_status(session_key)` 回四格：

| 格 | 含义 |
|---|---|
| `turns_total` | 可抽取轮数（`role != 'system'`；system 轮永不参与蒸馏） |
| `turns_distilled` | 已到终态（蒸馏已跑完）的轮数 |
| `turns_pending` | 还没蒸的轮数 |
| `last_arrival_ms` | 最近一轮的到达时刻（含 system 轮） |

会话不存在 = `empty:no_rows`，**isError=false**——轮询一个还没落的会话键不是故障。
空 `session_key` 是输入错误（`validation:missing_argument`）。

## 4. 导入的两阶段（hermes G14 同一痛点）

给记忆库灌历史对话是一条 `recollect-hub import <FILE>...` 命令（可一次给多个文件，
每个文件独立一份报告、退出码取最坏；`--json` 时每个文件一行 JSON，回执带
`turn_uids`——接受轮的 uid 列表，拿它们去 poll ingest_status 或 vote 都有效）。

导入之后发生的事分两段，**急的那段不用等**：

1. **导入落地 → 转录立即可查**。轮一落转录层，`recollect_search`（原话检索）、
   `recollect_session`（整会话回放）、`recollect_ingest_status`（计数）立刻能读到它。
   不需要等蒸馏、不需要重试。
2. **蒸馏 → 记忆召回**。导入的轮由中心的抽取流程异步蒸馏成记忆行，之后才会出现在
   `recollect_recall` 的结果里。想知道蒸到哪了，别瞎等——`recollect_ingest_status`
   的 `turns_distilled` / `turns_pending` 就是答案。

两段之间"search 查得到、recall 查不到"是**预期状态**，不是数据丢了。

## 5. 常见错误与可粘贴的修正

| 读数（`kind:code`） | 意思 | 下一步 |
|---|---|---|
| `validation:unregistered` | core.db 里没有这个 agent 的注册行 | `recollect-core --data-dir <dir> register <agent_id>`（顺序见 §1） |
| `validation:agent_id_forbidden` | 参数里带了 `agent_id` | 删掉参数——身份由运行时绑定 |
| `unavailable:no_memory` | 这台还没有可用副本 | 节点先同步（`node sync`），或到中心面上操作 |
| `unavailable:index_not_built` | 召回赖着的索引层没建 | `recollect-hub index rebuild --data-dir <dir>` |
| `unavailable:write_refused` / `authorization:replica_read_only` | 节点持有只读副本，写要落中心 | 把 vote / report 放到中心会话里做 |
| `empty:no_rows` | 库是好的，这次查询没命中 | 正常读数（isError=false），换词或换句柄重试 |

通用口径：
- **空读数不是故障**。`no_rows`（及 recall 的 `no_episode` / `no_edges`）都是
  isError=false 的正常状态；宿主不必把它们当 transport 问题重发成洪水。
- 记一条话用 `recollect_remember`（写入即返回 `turn_uid`），抽取同样异步——
  想知道那一轮的状态，`recollect_status` 带 `turn=<uid>`。
- 召回里**用了某张卡就报一句** `recollect_reuse`（`handle` 就是召回行里的句柄，
  `outcome` 五档 `worked|failed|partial|mismatch|stale`）。报 `worked` 两次这张卡会
  在召回里升一档，报 `stale` 它当场按失效处理，报 `failed`/`mismatch` 它进人工复核队列
  ——不报等于"用量读数为零"，好卡坏卡都升不了也降不了。注入面已经带着句柄，顺手就能报。

## 6. 边界

- agent 面没有删除：退休记忆（`retired`）是 user/operator 的动作，工具会显式拒绝。
- 全库导出（`recollect_export`）默认关闭，同属 user/operator。
- 本文不覆盖部署、密钥与数据迁移；那些看操作员文档。
- 要**自己装、自己写导入器、自己接宿主配置**：看 `2026-11-09-agent-install-and-import.md`。
