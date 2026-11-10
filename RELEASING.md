# 发布流程（维护者）

本文件只写"怎么发一个版本"。发布形态与用户安装方式见 `README.md` 与
`docs/2026-11-09-agent-install-and-import.md`。

## 0. 发布前检查

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-fixtures-pii.sh        # 零命中
bash scripts/check-readme-surface.sh      # 命令面与 README 对拍
bash scripts/check-release-artifacts.sh   # 产物与 sha 一一对应
```

Tag 必须等于 `Cargo.toml` 里 `[workspace.package].version`；CI 的 `guard` job 会再核一次。

## 1. 本地出产物（Windows + Linux x64）

```bash
# Windows x64（六件 → dist/windows-x86_64）
cargo build --release -p hub -p node -p mcp -p hook -p doctor -p rc-core
mkdir -p dist/windows-x86_64
for b in hub node mcp hook doctor core; do cp target/release/recollect-$b.exe dist/windows-x86_64/; done

# Linux x64（musl 静态；在 WSL / Linux 上）
export CC_x86_64_unknown_linux_musl=musl-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
cargo build --release --target x86_64-unknown-linux-musl \
  -p hub -p node -p mcp -p hook -p doctor -p rc-core
mkdir -p dist/linux-x86_64
for b in hub node mcp hook doctor core; do
  cp target/x86_64-unknown-linux-musl/release/recollect-$b dist/linux-x86_64/
done

# 便携包：zip / tar.gz + manifest.json + SHA256SUMS
bash scripts/package-portable.sh

# 安装器：setup.exe（Inno Setup）+ deb/rpm（nfpm）
RECOLLECT_LICENSE=MIT RECOLLECT_HOMEPAGE=https://github.com/lingxinsuixing/recollect \
  bash scripts/package-native.sh
```

产物都在 `dist/`。Linux 二进制是 **musl 静态**，不依赖发行版 glibc。

## 2. 单提交发布分支

公开仓库**不携带本机完整历史**（当前 800+ 条）。发布树从一个孤立分支出：

```bash
git checkout --orphan release/vX.Y.Z
git rm -r --cached .
# 只 add 发布树（示例见下），排除内部 docs/、data/、dist/、target/、.worktrees/
git add Cargo.toml Cargo.lock LICENSE README.md .gitignore RELEASING.md \
        crates contracts eval fixtures golden packaging .github skills \
        docs/2026-10-07-recollect-rust-architecture.md \
        docs/2026-11-08-onboarding.md \
        docs/2026-11-09-agent-install-and-import.md \
        hub.toml.example llm.toml.example \
        tools/converters tools/pipeline \
        scripts/build-release.sh scripts/check-fixtures-pii.sh \
        scripts/check-readme-surface.sh scripts/check-release-artifacts.sh \
        scripts/package-portable.sh scripts/package-native.sh
git commit -m "recollect X.Y.Z"
git tag -a vX.Y.Z -m "recollect X.Y.Z"
git checkout main
```

推送只推发布分支与 tag，**不要**把本机 `main` 的历史推上去：

```bash
git push origin release/vX.Y.Z:main
git push origin vX.Y.Z
```

## 3. CI 出包

推 tag 后 `.github/workflows/release.yml` 会：

1. 校验 tag 与 workspace 版本；
2. Windows：构建六件 → zip + manifest → Inno `setup.exe`；
3. Linux：musl 静态六件 → tar.gz + nfpm `deb`/`rpm`；
4. 汇总 `SHA256SUMS` / `manifest.json`，挂到 GitHub Release。

安装器**不自动改宿主 MCP 配置**；这一步由 agent/用户按说明书完成。
