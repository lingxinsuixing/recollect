#!/usr/bin/env bash
# C4 判据 4⑨：校验两份 SHA256SUMS 与各自目录里的产物一一对应。
#
# - 两个平台目录都必须在；缺任何一个 ⇒ 退出 3，并点名去跑 scripts/build-release.sh。
#   本脚本**不**调用构建（release 构建很慢，闸里不许重复构建），但也不许静默跳过：
#   没有产物就是红。
# - 校验内容：六份预期产物在场且非空；目录里没有未列出的文件；SHA256SUMS 恰好六行、
#   无重复、每个 sha256 与现算值一致；aarch64 产物真的是 ELF/aarch64（e_machine = 0x3E），
#   Windows 产物真的是 PE（MZ）。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail() {
  echo "RELEASE-ARTIFACTS FAILED: $1" >&2
  echo "  (missing artifacts are red: run \`bash scripts/build-release.sh\` first)" >&2
  exit 3
}

BASES=(recollect-hub recollect-node recollect-mcp recollect-hook recollect-doctor recollect-core)
PLATFORMS=(windows-x86_64 linux-x86_64)

for platform in "${PLATFORMS[@]}"; do
  dir="dist/$platform"
  [ -d "$dir" ] || fail "$dir is missing"
  [ -f "$dir/SHA256SUMS" ] || fail "$dir/SHA256SUMS is missing"
  [ -s "$dir/SHA256SUMS" ] || fail "$dir/SHA256SUMS is empty"

  ext=""
  [ "$platform" = "windows-x86_64" ] && ext=".exe"
  expected=()
  for b in "${BASES[@]}"; do expected+=("$b$ext"); done

  # 六份预期产物必须都在，且不是空壳。
  for name in "${expected[@]}"; do
    [ -s "$dir/$name" ] || fail "$dir/$name is missing or empty"
  done

  # 目录里不许有未列出的产物：SHA256SUMS 与文件是一一对应，不是"至少列出这些"。
  for path in "$dir"/*; do
    [ -f "$path" ] || continue
    name="$(basename "$path")"
    [ "$name" = "SHA256SUMS" ] && continue
    matched=0
    for e in "${expected[@]}"; do
      if [ "$name" = "$e" ]; then
        matched=1
        break
      fi
    done
    [ "$matched" = "1" ] || fail "$dir/$name is not one of the six expected artifacts"
  done

  # 逐行核对 SHA256SUMS：格式、无重复、文件在场、现算 sha256 一致。
  n=0
  seen=" "
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [ -z "$line" ] && continue
    n=$((n + 1))
    read -r hash name _rest <<< "$line"
    name="${name#\*}"
    [ -n "$hash" ] && [ -n "$name" ] || fail "$dir/SHA256SUMS line $n is malformed: $line"
    [[ "$hash" =~ ^[0-9a-f]{64}$ ]] || fail "$dir/SHA256SUMS line $n is not a sha256: $line"
    case "$seen" in
      *" $name "*) fail "$dir/SHA256SUMS lists $name twice" ;;
    esac
    seen="$seen$name "
    matched=0
    for e in "${expected[@]}"; do
      if [ "$name" = "$e" ]; then
        matched=1
        break
      fi
    done
    [ "$matched" = "1" ] || fail "$dir/SHA256SUMS lists an unexpected file: $name"
    [ -f "$dir/$name" ] || fail "$dir/SHA256SUMS lists a file that is not there: $name"
    actual="$(sha256sum "$dir/$name" | awk '{print $1}')" || fail "sha256sum failed for $dir/$name"
    [ "$actual" = "$hash" ] || fail "$dir/$name sha256 does not match SHA256SUMS"
  done < "$dir/SHA256SUMS"
  [ "$n" -eq 6 ] || fail "$dir/SHA256SUMS has $n entries, expected 6"

  # 架构证明：Windows = PE (MZ)，aarch64 = ELF 且 e_machine 0xB7。
  for name in "${expected[@]}"; do
    if [ "$platform" = "windows-x86_64" ]; then
      magic="$(od -An -tx1 -N2 "$dir/$name" | tr -d ' \n')"
      [ "$magic" = "4d5a" ] || fail "$dir/$name is not a PE binary (magic=$magic)"
    else
      magic="$(od -An -tx1 -N4 "$dir/$name" | tr -d ' \n')"
      [ "$magic" = "7f454c46" ] || fail "$dir/$name is not an ELF binary (magic=$magic)"
      machine="$(od -An -tx1 -N2 -j18 "$dir/$name" | tr -d ' \n')"
      [ "$machine" = "3e00" ] || fail "$dir/$name is not x86-64 (e_machine=$machine, expected 3e00)"
    fi
  done

  echo "OK: $dir/SHA256SUMS 6/6 一一对应，sha256 全部复核通过（$platform）"
done

echo "OK: 双 target 产物与 sha256 清单已校验（windows-x86_64 / linux-x86_64）"
