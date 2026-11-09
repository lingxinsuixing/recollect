# 贡献指南

## 维护状态

这是一个个人项目：不保证固定更新节奏，也不承诺响应时间。你可以按 MIT 许可自行 fork、修改与分发；保留版权与许可声明即可。

## 开 issue

请尽量让问题可复现：

- 版本：对应二进制的 `--version`（`recollect-eval` 除外）；
- 平台：操作系统与架构（例如 Windows 11 x64 / Ubuntu 22.04 x86_64）；
- 场景：中心模式还是节点 `--replica`；用了哪个工具/命令；
- 复现步骤与期望结果；
- 读数：`recollect-doctor --capabilities`（对 `--data-dir` 运行时）或相关工具的错误码/`kind:code`；
- **不要**贴密钥、token、配对码、真实会话正文或本机绝对路径。

安全相关问题（密钥、鉴权、可利用漏洞）不要开公开 issue，先私下联系维护者。

## 提交 PR

1. Fork 后从 `main` 开分支，改动尽量聚焦一个问题；
2. 代码风格跟随现有模块：注释只写"为什么"，不写"做了什么"；
3. 提交前在仓库根跑：

   ```bash
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   bash scripts/check-fixtures-pii.sh
   bash scripts/check-readme-surface.sh
   ```

4. 测试夹具只用合成数据：不要放真实姓名、真实主机名、内网地址或真实会话正文；
5. 新增/修改工具、命令或配置时，同步更新 `README.md` 的命令面块与相关说明书；
6. PR 描述写清：改了什么、为什么、怎么验证；涉及行为变化请附读数。

## 许可

提交的贡献按 MIT 许可进入本项目。不要求签署 CLA。
