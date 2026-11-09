#!/usr/bin/env bash
# 便携包打包（W39 发布面）：把 dist/<platform>/ 的六件二进制打成给 agent 的 zip / tar.gz，
# 每份带 bin/ 布局、LICENSE（若在）、manifest.json 与 SHA256SUMS。
#
# 为什么单独一条：人工安装器（Inno/nfpm）不在本机工具链里，而 agent 路径不依赖它们——
# 解压即用、按 manifest 校验、按绝对路径调用。安装器接上后仍复用这里的产物。
#
# 用法：bash scripts/package-portable.sh [输出目录，缺省 dist/release]
# 输入：dist/windows-x86_64、dist/linux-x86_64、dist/linux-aarch64（存在哪个打哪个；
#       DIST_ROOT 可覆盖 dist 根，仅用于本地验证）
# 退出码：0 = 全部打包；3 = 有平台目录但六件不齐（不产半包）
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="${1:-$ROOT/dist/release}"
DIST_ROOT="${DIST_ROOT:-$ROOT/dist}"

VERSION="$(awk '/^\[workspace\.package\]/{f=1;next} f&&/^version[[:space:]]*=/{gsub(/[" ]/,"",$3);print $3;exit}' Cargo.toml)"
[ -n "$VERSION" ] || { echo "cannot read workspace version" >&2; exit 3; }
mkdir -p "$OUT" || exit 3

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

pack_one() {
  local dir="$1" plat os arch ext stage name asset
  plat="$(basename "$dir")"
  case "$plat" in
    windows-x86_64) os=windows; arch=x64; ext=".exe";;
    linux-x86_64)   os=linux;   arch=x64; ext="";;
    linux-aarch64)  os=linux;   arch=aarch64; ext="";;
    *) return 0;;
  esac
  stage="$OUT/recollect-$VERSION-$os-$arch"
  rm -rf "$stage"
  mkdir -p "$stage/bin" || return 3
  for b in hub node mcp hook doctor core; do
    if [ ! -s "$dir/recollect-$b$ext" ]; then
      echo "missing artifact: $dir/recollect-$b$ext" >&2
      return 3
    fi
    cp "$dir/recollect-$b$ext" "$stage/bin/"
  done
  [ -f "$ROOT/LICENSE" ] && cp "$ROOT/LICENSE" "$stage/"

  # 平台级清单（check-release-artifacts.sh 的核对面）：六件逐行 sha256。
  ( cd "$dir" && sha256sum recollect-* > SHA256SUMS ) || return 3

  # manifest：agent 先读它再选文件、再校验；顺序稳定（不依赖时钟）。
  {
    printf '{\n  "version": "%s",\n  "os": "%s",\n  "arch": "%s",\n  "format_version": 1,\n  "entrypoints": ["bin/recollect-mcp%s"],\n  "assets": [\n' \
      "$VERSION" "$os" "$arch" "$ext"
    local first=1 f rel h
    for f in "$stage"/bin/*; do
      rel="bin/$(basename "$f")"
      h="$(sha256_of "$f")" || return 3
      [ $first -eq 1 ] || printf ',\n'
      first=0
      printf '    {"path": "%s", "sha256": "%s"}' "$rel" "$h"
    done
    printf '\n  ]\n}\n'
  } > "$stage/manifest.json" || return 3

  if [ "$os" = "windows" ]; then
    name="recollect-$VERSION-windows-$arch.zip"
    ( cd "$OUT" && powershell -NoProfile -Command "Compress-Archive -Force -Path '$(basename "$stage")' -DestinationPath '$name'" ) || return 3
  else
    name="recollect-$VERSION-linux-$arch.tar.gz"
    # `--mode=0755`：Windows 侧 tar 会把 NTFS 的模式读成 0644，解压后二进制不可执行。
    # 这里显式写死可执行位（bin/、manifest、README 一律 0755，无害）。
    tar --mode=0755 -czf "$OUT/$name" -C "$OUT" "$(basename "$stage")" || return 3
  fi
  asset="$OUT/$name"
  echo "$(sha256_of "$asset")  $name"
}

: > "$OUT/SHA256SUMS"
rc=0
for dir in "$DIST_ROOT"/windows-* "$DIST_ROOT"/linux-*; do
  [ -d "$dir" ] || continue
  line="$(pack_one "$dir")" || { rc=3; continue; }
  [ -n "$line" ] && echo "$line" >> "$OUT/SHA256SUMS"
done

if [ $rc -eq 0 ]; then
  # 发布级 manifest：agent 只需读这一个文件就能选包、校验、拿入口。
  COMMIT="${RECOLLECT_COMMIT:-$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)}"
  {
    printf '{\n  "version": "%s",\n  "commit": "%s",\n  "format_version": 1,\n  "packages": [\n' "$VERSION" "$COMMIT"
    first=1
    while read -r h name; do
      [ -n "${name:-}" ] || continue
      os="$(printf '%s' "$name" | sed -n "s/^recollect-$VERSION-\([a-z0-9]*\)-.*/\1/p")"
      arch="$(printf '%s' "$name" | sed -n "s/^recollect-$VERSION-[a-z0-9]*-\([a-z0-9]*\)\..*/\1/p")"
      ext=""; [ "$os" = "windows" ] && ext=".exe"
      [ $first -eq 1 ] || printf ',\n'
      first=0
      printf '    {"os": "%s", "arch": "%s", "file": "%s", "sha256": "%s", "entrypoint": "bin/recollect-mcp%s"}' \
        "$os" "$arch" "$name" "$h" "$ext"
    done < "$OUT/SHA256SUMS"
    printf '\n  ]\n}\n'
  } > "$OUT/manifest.json" || rc=3
fi

if [ $rc -eq 0 ]; then
  echo "packaged into $OUT"
  cat "$OUT/SHA256SUMS"
  echo "manifest: $OUT/manifest.json"
else
  echo "PACKAGING FAILED: some platform is incomplete" >&2
fi
exit $rc
