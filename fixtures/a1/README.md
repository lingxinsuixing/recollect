# A1 锚质量门夹具边界（gold v2）

本目录的 dev / holdout **按 session 切分**：同一 session 不跨两边；实体 id 与主要名字也不跨两边。
`dev.json` / `dev_gold.json` 是调参集；`holdout.json` / `holdout_gold.json` 封存，只在 dev 连续两轮达标后跑一次。

## 1. 什么算锚

一个锚必须是**有名、可复指、跨轮稳定**的东西，且表面名逐字出现在某一轮原文里：

- `person` / `agent` / `org` / `place` / `object` / `animal` / `work` / `event` / `concept`。
- 原文点名的交付物（时间表、报告、清单、方案）算 `work`。
- 具体型号/品牌与旁边的泛称是**同一个锚**：`惠普 M126a` 为 canonical，`打印机` 为 alias；
  不许拆成两个同 kind 锚。
- 同名不同人：两个锚，canonical 用原文可区分的逐字短语，`mode="all"`，tokens 同时包含共同名与区分词。
- AI/工具算 `agent`，人算 `person`；有名字的团队/组织算 `org`。

## 2. 什么不建锚

| 层 | 含义 | 例子 |
|---|---|---|
| `forbidden` | 硬禁名：命中即失败 | 注入块 `[recollect] ...` 里的名字、压缩摘要 `[[compact_summary]]` 里的名字、纯代词 |
| `unresolved` | 无名泛称/无法落地的实例，应进 `unresolved_surfaces` | `一个朋友`、`那台设备`、`一个同事`、`报告模板` |
| `no_anchor` | 不该建锚、也不要求 unresolved 的串 | `10 月 29 日`、`半小时`、`1TB`、`docs/atlas`、`彩色` |

注意：
- 泛称只有在确实指向已命名实体时才作 alias（`小张` → 张三）；否则不建锚。
- 代词永不进 canonical/alias；能归因就靠 `speaker_ref`，不能就 `unresolved`。
- 注入块/压缩摘要**不是命名来源**；那两轮 `mentions` 必须为空，里面的名字进 `forbidden`。
- 路径/文件名不是锚，除非原文把它当作有名字的交付物来引用。

## 3. gold 形状

```json
{
  "format_version": 2,
  "entities": [
    {"id":"dev_s02_shenzhixia","kind":"person","tokens":["沈知夏"],
     "aliases":["知夏"],"core":true,"sessions":["ses-a1-dev-s02"]}
  ],
  "forbidden": ["某老板"],
  "allowed_extra": ["打包清单"],
  "turns": [
    {"session_key":"ses-a1-dev-s02","turn_index":0,
     "mentions":[{"surface":"沈知夏","entity":"dev_s02_shenzhixia","speaker":null,"role":"mentioned"}],
     "unresolved":["客户"],"no_anchor":["10 月 30 日"]}
  ]
}
```

- `sessions`：实体所属 session；空表示不限（旧 gold）。有了它，两个 session 可以各自用“打印机”这类语域别名。
- `turns`：exchange 每个 turn 必须恰好一条，空轮也要有，防止“整轮漏标”被当成“本来没有”。
- mention 的 `surface` 必须逐字出现在该轮；同一轮同一 surface 只记一条。
- 同一轮里互相嵌套的表面只记**最大短语**（`老电脑` + `ThinkPad`，不再单独记 `2019 年的 ThinkPad`）；
  alias 只需逐字出现在原文，不强制单独作一次 mention。
- `speaker` 只在“这一轮本身就是该实体在说话”时填实体 id；转述（`张三说……`）用 `role="quoted"`、speaker 为 null。
  `role` ∈ `speaker|addressee|mentioned|quoted`；speaker/role 只做诊断，不进硬门。
- `allowed_extra`：允许存在但不进 gold 的具名额外锚；不填的额外锚算膨胀。

## 4. 跑法

```bash
# 一臂：所有 session 双跑，落逐 session plan + 合并 plan
cargo run -q -p rc-distill --example a1_probe -- \
  --plan-dir target/a1/dev --plan-out target/a1/dev/merged.json \
  fixtures/a1/dev.json deepseek deepseek-chat

# 评测：合并 plan + gold v2 + 原文
cargo run -q -p rc-distill --example a1_eval -- \
  target/a1/dev/merged.json fixtures/a1/dev_gold.json fixtures/a1/dev.json
```
