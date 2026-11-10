#!/usr/bin/env bash
# C4：产出两个 target 的 release 产物与各自的 SHA256SUMS。
#
# 环境事实（控制器已实测，本脚本不重装工具链）：
# - Windows 侧只装了 x86_64-pc-windows-msvc；产物是
#   `${CARGO_TARGET_DIR:-target}/release/recollect-*.exe`。
# - Linux aarch64 走 WSL 交叉：aarch64-unknown-linux-gnu target 已用 USTC 镜像装好，
#   链接器是 /usr/bin/aarch64-linux-gnu-gcc，经
#   `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` 指过去；交叉 target 目录放在
#   `$HOME/.cache/recollect-target-aarch64`（用 WSL_CARGO_TARGET_DIR 可覆盖），
#   不写仓库的 target/。
# - 真机 Pi 未接入：aarch64 产物只证明"能交叉构建出 ARM 可执行文件"，
#   没有在任何 arm64 机器上执行过；本脚本不伪造运行读数。
#
# 任何一步失败都退出 3，并点名是哪一步；不伪造产物、不跳过平台。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

STEP="(not started)"
die_step() {
  echo "BUILD FAILED at step: $STEP" >&2
  echo "  $1" >&2
  exit 3
}

# 可复现 + 隐私：把构建机的主目录与仓库根从 panic/源码路径里抹掉。
# 依赖源码路径（cargo registry）会以绝对路径编进二进制，发布前必须去掉用户名。
ROOT_REMAP="$(cygpath -w "$(pwd)" 2>/dev/null || pwd)"
if [ -n "${USERPROFILE:-}" ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${USERPROFILE}=/build --remap-path-prefix=${ROOT_REMAP}=/src"
elif [ -n "${HOME:-}" ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${HOME}=/build --remap-path-prefix=${ROOT_REMAP}=/src"
fi
run_step() {
  STEP="$1"
  shift
  "$@" || die_step "the command exited $?"
}

BINARIES=(recollect-hub recollect-node recollect-mcp recollect-hook recollect-doctor recollect-core)
WIN_DIR="$ROOT/dist/windows-x86_64"
LINUX_DIR="$ROOT/dist/linux-aarch64"
WIN_TARGET="${CARGO_TARGET_DIR:-target}"

# ---- 1. Windows x86_64 ----
run_step "windows x86_64 release build (cargo build --release -p hub -p node -p mcp -p hook -p doctor -p rc-core)" \
  cargo build --release -p hub -p node -p mcp -p hook -p doctor -p rc-core

STEP="copy windows x86_64 artifacts"
rm -rf "$WIN_DIR"
mkdir -p "$WIN_DIR" || die_step "cannot create dist/windows-x86_64"
for b in "${BINARIES[@]}"; do
  src="$WIN_TARGET/release/$b.exe"
  [ -f "$src" ] || die_step "missing artifact: $src (did the release build really finish?)"
  [ -s "$src" ] || die_step "empty artifact: $src"
  cp "$src" "$WIN_DIR/" || die_step "cannot copy $src"
done

# `sha256sum` 在 Git Bash 会把二进制模式的 `*` 贴到文件名前；这里只取第一列，
# 自己写 `hash  name` 两空格格式，两个平台同一形状。
write_sums() {
  local dir="$1"
  shift
  local f h
  : > "$dir/SHA256SUMS" || return 1
  for f in "$@"; do
    h="$(sha256sum "$dir/$f" | awk '{print $1}')" || return 1
    [ "${#h}" -eq 64 ] || return 1
    printf '%s  %s\n' "$h" "$f" >> "$dir/SHA256SUMS" || return 1
  done
}

STEP="write windows x86_64 SHA256SUMS"
win_names=()
for b in "${BINARIES[@]}"; do win_names+=("$b.exe"); done
write_sums "$WIN_DIR" "${win_names[@]}" || die_step "sha256sum failed for dist/windows-x86_64"
[ "$(wc -l < "$WIN_DIR/SHA256SUMS")" -eq 6 ] || die_step "dist/windows-x86_64/SHA256SUMS does not have 6 lines"
echo "OK: windows x86_64 -> dist/windows-x86_64/ (6 binaries + SHA256SUMS)"

# ---- 2. Linux aarch64（WSL 交叉）----
STEP="linux aarch64 cross build (WSL)"
command -v wsl >/dev/null 2>&1 || die_step "wsl is not available on this machine"
WIN_ABS="$(pwd -W)" || die_step "cannot resolve the repository path for WSL (pwd -W)"
ROOT_WSL="$(wsl -e wslpath -a "$WIN_ABS")" || die_step "wslpath could not convert $WIN_ABS"
[ -n "$ROOT_WSL" ] || die_step "wslpath returned an empty path for $WIN_ABS"

WSL_BUILD="cd '$ROOT_WSL' && \
CARGO_TARGET_DIR=\${WSL_CARGO_TARGET_DIR:-\$HOME/.cache/recollect-target-aarch64} \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
cargo build --release --target aarch64-unknown-linux-gnu \
  -p hub -p node -p mcp -p hook -p doctor -p rc-core"
run_step "linux aarch64 cross build (WSL, aarch64-unknown-linux-gnu)" \
  wsl -e bash -lc "$WSL_BUILD"

STEP="copy linux aarch64 artifacts"
rm -rf "$LINUX_DIR"
mkdir -p "$LINUX_DIR" || die_step "cannot create dist/linux-aarch64"
WSL_COPY="cd '$ROOT_WSL' && mkdir -p dist/linux-aarch64 && \
T=\${WSL_CARGO_TARGET_DIR:-\$HOME/.cache/recollect-target-aarch64}/aarch64-unknown-linux-gnu/release && \
for b in recollect-hub recollect-node recollect-mcp recollect-hook recollect-doctor recollect-eval; do \
  [ -s \"\$T/\$b\" ] || { echo \"missing artifact: \$T/\$b\" >&2; exit 1; }; \
  cp \"\$T/\$b\" dist/linux-aarch64/ || exit 1; \
done"
run_step "copy linux aarch64 artifacts (WSL -> dist/linux-aarch64)" \
  wsl -e bash -lc "$WSL_COPY"

STEP="write linux aarch64 SHA256SUMS"
for b in "${BINARIES[@]}"; do
  [ -s "$LINUX_DIR/$b" ] || die_step "missing or empty artifact: dist/linux-aarch64/$b"
done
write_sums "$LINUX_DIR" "${BINARIES[@]}" || die_step "sha256sum failed for dist/linux-aarch64"
[ "$(wc -l < "$LINUX_DIR/SHA256SUMS")" -eq 6 ] || die_step "dist/linux-aarch64/SHA256SUMS does not have 6 lines"
echo "OK: linux aarch64 -> dist/linux-aarch64/ (6 binaries + SHA256SUMS)"

if command -v file >/dev/null 2>&1; then
  file "$LINUX_DIR/recollect-hub"
fi
echo "NOTE: aarch64 真机读数见 docs/2026-10-22-cold-start-and-binaries.md（2026-10-22 树莓派实测）"
