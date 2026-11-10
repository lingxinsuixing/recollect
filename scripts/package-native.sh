#!/usr/bin/env bash
# 原生安装器打包（W39 发布面）：Linux deb/rpm（nfpm，WSL）+ Windows setup.exe（Inno Setup）。
#
# 前置：
# - dist/windows-x86_64、dist/linux-x86_64 各六件已由构建步骤产出；
# - WSL 里有 nfpm（~/.local/bin/nfpm，本机已装）；Windows 有 Inno Setup 6；
# - LICENSE 存在时会被 Inno 一并打包；nfpm 的 license 字段取 RECOLLECT_LICENSE。
#
# 用法：bash scripts/package-native.sh [--linux|--windows]（缺省两边都打）
# 环境：RECOLLECT_VERSION / RECOLLECT_MAINTAINER / RECOLLECT_HOMEPAGE / RECOLLECT_LICENSE
set -uo pipefail

DO_LINUX=1
DO_WINDOWS=1
case "${1:-}" in
  --linux) DO_WINDOWS=0;;
  --windows) DO_LINUX=0;;
  "") ;;
  *) echo "usage: $0 [--linux|--windows]" >&2; exit 3;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(awk '/^\[workspace\.package\]/{f=1;next} f&&/^version[[:space:]]*=/{gsub(/[" ]/,"",$3);print $3;exit}' Cargo.toml)"
VERSION="${RECOLLECT_VERSION:-$VERSION}"
MAINTAINER="${RECOLLECT_MAINTAINER:-recollect contributors}"
HOMEPAGE="${RECOLLECT_HOMEPAGE:-https://github.com/OWNER/recollect}"
LICENSE="${RECOLLECT_LICENSE:-NOASSERTION}"

if [ "$DO_LINUX" -eq 1 ]; then
  [ -d dist/linux-x86_64 ] || { echo "missing dist/linux-x86_64 (build Linux first)" >&2; exit 3; }

  # ---- Linux: deb + rpm（nfpm；本机走 WSL） ----
  sed -e "s|@VERSION@|$VERSION|g" \
      -e "s|@MAINTAINER@|$MAINTAINER|g" \
      -e "s|@HOMEPAGE@|$HOMEPAGE|g" \
      -e "s|@LICENSE@|$LICENSE|g" \
      packaging/nfpm.yaml.in > packaging/nfpm.yaml || exit 3

  if [ "$(uname -s)" = "Linux" ]; then
    NFPM_BIN="${NFPM:-$HOME/.local/bin/nfpm}"
    "$NFPM_BIN" package -f packaging/nfpm.yaml -p deb -t dist || exit 3
    "$NFPM_BIN" package -f packaging/nfpm.yaml -p rpm -t dist || exit 3
  else
    WSL_ROOT="$(wsl -e wslpath -a "$ROOT" | tr -d '\r')"
    wsl -e bash -lc "cd '$WSL_ROOT' && ~/.local/bin/nfpm package -f packaging/nfpm.yaml -p deb -t dist && ~/.local/bin/nfpm package -f packaging/nfpm.yaml -p rpm -t dist" || exit 3
  fi
fi

if [ "$DO_WINDOWS" -eq 1 ]; then
  [ -d dist/windows-x86_64 ] || { echo "missing dist/windows-x86_64 (build Windows first)" >&2; exit 3; }

  # ---- Windows: setup.exe（Inno Setup；不自动改宿主配置） ----
  ISCC="${ISCC:-C:/Users/$USERNAME/AppData/Local/Programs/Inno Setup 6/ISCC.exe}"
  [ -f "$ISCC" ] || ISCC="C:/Program Files (x86)/Inno Setup 6/ISCC.exe"
  [ -f "$ISCC" ] || { echo "ISCC.exe not found; install Inno Setup 6 or set ISCC" >&2; exit 3; }

  # Git Bash 会把 `/D...` 当路径改写；ISCC 的 /D 定义必须原样穿过。
  MSYS_NO_PATHCONV=1 "$ISCC" "/DMyVersion=$VERSION" "/DMyPublisher=$MAINTAINER" packaging/recollect.iss || exit 3
fi

echo "native installers:"
# 只列本次真的产出的形态；缺一边是正常的（--linux / --windows），不是失败。
if [ "$DO_WINDOWS" -eq 1 ]; then
  ls -l dist/*setup*.exe 2>/dev/null || true
fi
if [ "$DO_LINUX" -eq 1 ]; then
  ls -l dist/*.deb dist/*.rpm 2>/dev/null || true
fi
exit 0
