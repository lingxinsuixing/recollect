# 中心 LLM 怎么配（2026-11-09）

一句话：**模型写在 `llm.toml`，密钥放在 `hub secret` 的加密箱里，跑抽取/推导时用
`--llm-config` 指过去。** 没有"一键生成 LLM 配置"的命令；有命令可以建箱、写密钥、列格。

## 1. 五步

```bash
DATA=/path/to/data

# 1) 复制形状（改 provider / model / secret_ref）
cp llm.toml.example "$DATA/llm.toml"

# 2) 建一个空箱（默认箱名 secrets；已存在不会覆盖）
recollect-hub secret init --data-dir "$DATA" --box secrets

# 3) 把密钥写进某个格（值只走 stdin，不回显、不进参数表）
printf '%s' "$DEEPSEEK_KEY" | \
  recollect-hub secret put --value-stdin --data-dir "$DATA" secrets:deepseek_key

# 4) 这台机器的名字（轮转态与座位主语都靠它）
export RECOLLECT_DEVICE_ID=dev-laptop

# 5) 试跑一条 pass（真出网，失败会响亮退出）
recollect-hub distill once --pass naming \
  --data-dir "$DATA" --llm-config "$DATA/llm.toml" --llm-timeout-ms 300000
```

> `llm.toml.example` 里的 `secret_ref = "kms:deepseek_key"` 只是一个**箱名示例**。
> 你用默认箱 `secrets`，就把它改成 `"secrets:deepseek_key"`；或者第 2 步用 `--box kms`。

## 2. `llm.toml` 的形状

- `provider` + `model` 必须是 `rc-llm-http` 预设表里的**复合键**，逐字抄，大小写也算；
- 自建网关/内网代理走 `[providers.endpoint]`（`base_url` + `protocol` 二选一），不要硬塞 provider；
- `secret_ref = "箱:格"`，两侧都只许 safe token（不带斜杠）；
- `[[providers]]` 的顺序 = 换座顺序：第一条主座，后面备座；
- `temperature`、`budget`、`derive_budget` 可选；不写就走厂家默认；
- **这个文件里不允许出现任何密钥值。**

## 3. 有哪些命令

| 目的 | 命令 |
|---|---|
| 建箱 | `recollect-hub secret init --data-dir DIR --box 名称` |
| 写一格 | `recollect-hub secret put --value-stdin --data-dir DIR 箱:格` |
| 看有哪些格 | `recollect-hub secret list --data-dir DIR 箱名`（**箱名是位置参数**，不是 `--box`） |
| 删一格 | `recollect-hub secret rm --data-dir DIR 箱:格` |
| 跑抽取 | `distill once --pass naming|fact|event|state|lesson|time --llm-config FILE` |
| 跑推导 | `derive run --llm-config FILE` |
| 旧管线 | `cycle once --llm-config FILE` |

注意 CLI 形状不完全对称：`init` 用 `--box`，`list` 把箱名当位置参数，`put/rm` 收 `箱:格`。
这是历史形状，2026-10-27 在现网踩过一次，照上表抄。

**没有** `hub llm init` / `hub llm check` 这种一键命令。配置是否正确，由
`distill/derive/cycle` 在启动和运行时校验：读不到箱、读不到格、模型不在表上、没设
`RECOLLECT_DEVICE_ID`，都会响亮失败并退出 3，而不是悄悄回落到默认模型。

## 4. 常见错误

| 现象 | 原因 | 修正 |
|---|---|---|
| `provider/model` 不在预设表 | 模型名抄错或用了表外组合 | 按 `llm.toml.example` 的注释逐字抄；自定义走 `endpoint` |
| 读不到密钥 | `secret_ref` 的箱/格与 `secret put` 不一致 | `secret list 箱名` 对一遍；`--box` 与 `secret_ref` 前缀要一致 |
| 退出 3 且提到设备名 | 没设 `RECOLLECT_DEVICE_ID` 或形状不对 | `export RECOLLECT_DEVICE_ID=dev-xxx` |
| 一直超时 | 单发有 300 秒墙钟；`--llm-timeout-ms` 只能调小 | 换近一点的网关或减小批 |
| 想先用假座位跑通 | 不需要密钥 | 用 `--replies FILE`（本地脚本回复），零出网 |

## 5. 实现位置（为什么是这套形状）

| 层 | 位置 | 做什么 |
|---|---|---|
| 配置解析 | `crates/rc-llm-cfg/src/lib.rs` | 读 `llm.toml`；`Ok(None)` = 没配，`Err` = 配了但坏了；不给默认值 |
| 引用形状 | `rc-llm-cfg/src/keys.rs` | `secret_ref` 必须是 `箱:格`，两侧 safe token |
| 密钥箱 | `crates/rc-secret/src/lib.rs` | `secrets.enc` 用 Argon2id 派生 + ChaCha20-Poly1305；口令在 `.pass`；值 `Zeroizing`，不落日志 |
| 座位装配 | `crates/hub/src/main.rs` 的 `live_keys` / `live_slots_with_deadline` | 按 `[[providers]]` 顺序解析密钥、查预设表、读设备身份、构造 `MeteredBackend` |
| 预设表 | `crates/rc-llm-http/src/preset.rs` | `provider + model` 复合键；表外组合启动即失败 |
| 轮转与账 | `Rotating` + `crates/rc-ledger/src/llm_state.rs` | 换座顺序、每台机器一行状态；`llm_calls` 与回放同账 |
| 单发墙 | `crates/rc-llm-http/src/transport.rs` | `CALL_WALL_MS = 300_000`，旗标只能调小 |

**为什么没有 `hub llm init` 这种命令**：配置本体是文件（可版本化、可 diff），密钥值只能经 stdin
进加密箱，不进数据库、不进参数表、不进日志；一键命令会把这三条边界重新打开。校验放在启动/运行时，
失败就退出 3，不静默回落到默认模型。

## 6. 安全

- 密钥只从 stdin 进；不要写进 `llm.toml`、README、提交历史或聊天记录；
- `<箱名>.enc` 与 `<箱名>.pass` 要一起备份、分开存放；丢了口令等于丢了箱；
- `llm.toml` 可以进备份，`*.enc` / `*.pass` 不要进公开仓库。

## 7. 验收

- `distill once` 退出 0，日志里能看到 `llm_calls` 增长；
- `recollect-doctor --data-dir DIR --device-id dev-...` 的 `llm_state` 有行；
- 换一座（改 `llm.toml` 顺序）后再跑，读数应按新顺序换座。
