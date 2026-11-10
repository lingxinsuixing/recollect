#!/usr/bin/env bash
# 判据 4⑦：fixtures、golden、eval 真值集与各 crate 的测试夹具里，不许出现真实姓名、
# 真实设备名或真实内网/组网地址。扫描面**不含** docs/（那里可能有运维记录）。
#
# 真实值清单**不进仓库**：维护者放在 `$RECOLLECT_PII_PATTERNS`
# （缺省 `$HOME/.recollect/pii-patterns.txt`，一行一个固定串）。仓库里只保留通用规则
# （经典/细粒度 GitHub token 前缀），避免"检测脚本自己泄露要挡的值"。
# 命中只打 `文件:行`，不回显整行正文；脚本自身不写任何口令/密钥。
#
# 纪律同其余 check-*.sh：0 = 零命中，3 = 命中或扫描面本身坏了，不许静默跳过。
set -euo pipefail
cd "$(dirname "$0")/.."

red() { echo "红：$1" >&2; exit 3; }

# 扫描面。少一个目录就红：目录被改名/删掉时，零命中是"没扫"而不是"干净"。
scan_dirs=(fixtures golden eval/truth)
for dir in "${scan_dirs[@]}"; do
  [ -d "$dir" ] || red "扫描面 $dir 不存在（零命中不能是没扫出来的）"
done

shopt -s nullglob
crate_tests=(crates/*/tests)
shopt -u nullglob
[ ${#crate_tests[@]} -gt 0 ] || red "crates/*/tests 一个都没匹配到（扫描面空了）"
for dir in "${crate_tests[@]}"; do
  [ -d "$dir" ] || red "扫描面 $dir 不是目录"
done

scan_paths=("${scan_dirs[@]}" "${crate_tests[@]}")
files="$(find "${scan_paths[@]}" -type f | wc -l | tr -d ' ')"
[ "$files" -gt 0 ] || red "扫描面里一个文件都没有"

# 本机私有清单：一行一个固定串；文件不存在就只跑通用规则。
patterns=()
extra="${RECOLLECT_PII_PATTERNS:-$HOME/.recollect/pii-patterns.txt}"
if [ -f "$extra" ]; then
  while IFS= read -r p || [ -n "$p" ]; do
    p="${p%$'\r'}"
    [ -n "$p" ] && patterns+=("$p")
  done < "$extra"
fi

hits=""
fixed_count=0
if [ ${#patterns[@]} -gt 0 ]; then
  fixed_count=${#patterns[@]}
  grep_args=()
  for pattern in "${patterns[@]}"; do
    grep_args+=(-e "$pattern")
  done
  # -F 固定串：清单里可能有正则元字符（路径、主机名），按字面比。
  set +e
  hits="$(grep -rInFi "${grep_args[@]}" "${scan_paths[@]}")"
  grep_rc=$?
  set -e
  case "$grep_rc" in
    0) ;;
    1) hits="" ;;
    *) red "grep 自己失败（rc=$grep_rc），零命中不成立" ;;
  esac
fi

# 通用规则：只收固定前缀的秘密形态，避免把测试里的合成钥匙当成泄漏。
# 经典 PAT = ghp_ + 36；细粒度 = github_pat_ + 20 以上。
set +e
generic_hits="$(grep -rInE 'ghp_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{20,}' "${scan_paths[@]}")"
generic_rc=$?
set -e
case "$generic_rc" in
  0) ;;
  1) generic_hits="" ;;
  *) red "通用 grep 失败（rc=$generic_rc），零命中不成立" ;;
esac

if [ -n "$hits" ] || [ -n "$generic_hits" ]; then
  echo "红：扫描面里出现真实姓名/设备名/内网地址/令牌（只给 文件:行）：" >&2
  { [ -n "$hits" ] && printf '%s\n' "$hits"; [ -n "$generic_hits" ] && printf '%s\n' "$generic_hits"; } \
    | awk -F: '{ print "  " $1 ":" $2 }' | sort -u >&2
  exit 3
fi

echo "OK：fixtures 扫描零命中（$files 个文件，固定模式 ${fixed_count} 条 + 令牌前缀 2 条）"
