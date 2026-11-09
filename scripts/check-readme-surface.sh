#!/usr/bin/env bash
# 判据 4⑥：README 的“命令面”块与二进制 --help 的读面对拍。
#
# 读面来源：`recollect-hub --help` 与 `recollect-hook --help` 的 `Commands:` 段
# （缩进两格的首 token，排除 `help`）；`recollect-eval` 没有 `--help`（它把用法打到
# stderr 并以 2 退出），从它的用法行里取子命令。`node`/`mcp`/`doctor` 是旗标命令，
# 只对拍各自的**关键旗标**是否出现在 README 的命令面块里。
#
# README 的块用 `hub a | b | c` 这一种写法（工单 §二 的模板），所以“以 `hub <cmd>`
# 出现”按行归一：前缀之后按 `|` 切开，每一段 trim 成一个 `前缀 子命令` 的面。
#
# 纪律同其余 check-*.sh：缺项必须点名，不许静默跳过；0 = 全对上，3 = 红。
set -euo pipefail
cd "$(dirname "$0")/.."

README=README.md
TARGET_DIR="${CARGO_TARGET_DIR:-target}/debug"

red() { echo "红：$1" >&2; exit 3; }

# 产物定位：Unix 无后缀，Windows 多一个 `.exe`。缺产物 ⇒ 3，并提示先构建。
bin_path() {
  local name="$1"
  if [ -x "$TARGET_DIR/$name" ]; then
    printf '%s\n' "$TARGET_DIR/$name"
  elif [ -x "$TARGET_DIR/$name.exe" ]; then
    printf '%s\n' "$TARGET_DIR/$name.exe"
  else
    red "缺产物 $TARGET_DIR/$name（Windows 则是 $name.exe）——先跑 cargo build --workspace"
  fi
}

[ -f "$README" ] || red "缺 $README"

hub_bin="$(bin_path recollect-hub)"
hook_bin="$(bin_path recollect-hook)"
eval_bin="$(bin_path recollect-eval)"

# 命令面块：围栏行本身不算；块没写、或围栏只开不闭，都是红。
block="$(awk '
  /<!-- commands:start -->/ { inside = 1; next }
  /<!-- commands:end -->/ { inside = 0; next }
  inside { print }
' "$README")"
[ -n "$block" ] || red "README 里没有 <!-- commands:start --> ... <!-- commands:end --> 命令面块"

# 从块里取某个前缀的子命令集合：前缀行按 `|` 切开，每段是一个面。
surface() {
  local prefix="$1"
  printf '%s\n' "$block" | awk -v p="$prefix" '
    $1 == p {
      line = $0
      sub(/^[[:space:]]*[^[:space:]]+[[:space:]]+/, "", line)
      n = split(line, parts, "|")
      for (i = 1; i <= n; i++) {
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", parts[i])
        if (parts[i] != "") print parts[i]
      }
    }
  ' | sort -u
}

# 从 clap 的 `Commands:` 段取子命令名：缩进两格的首 token，排除 `help`。
clap_commands() {
  awk '
    /^Commands:/ { inside = 1; next }
    inside && /^[^[:space:]]/ { inside = 0 }
    inside && /^  [a-z0-9][a-z0-9-]*/ && $1 != "help" { print $1 }
  ' | sort -u
}

compare() {
  local what="$1" got="$2" want="$3" missing extra
  missing="$(comm -23 <(printf '%s\n' "$want") <(printf '%s\n' "$got") | sed '/^$/d')"
  extra="$(comm -13 <(printf '%s\n' "$want") <(printf '%s\n' "$got") | sed '/^$/d')"
  if [ -n "$missing" ] || [ -n "$extra" ]; then
    echo "红：README 的 $what 与 --help 对不上" >&2
    if [ -n "$missing" ]; then
      echo "  缺少（--help 有、README 没有）：" >&2
      printf '    %s\n' $missing >&2
    fi
    if [ -n "$extra" ]; then
      echo "  多出（README 有、--help 没有）：" >&2
      printf '    %s\n' $extra >&2
    fi
    exit 3
  fi
}

hub_help="$("$hub_bin" --help 2>&1)" || red "recollect-hub --help 退出非 0"
hook_help="$("$hook_bin" --help 2>&1)" || red "recollect-hook --help 退出非 0"

# eval 没有 `--help` 分支：它把用法打到 stderr 并以 2 退出。0/2 都算“拿到了用法”，
# 其余退出码说明产物坏了，直接红。
set +e
eval_help="$("$eval_bin" --help 2>&1)"
eval_rc=$?
set -e
case "$eval_rc" in
  0 | 2) ;;
  *) red "recollect-eval --help 意外退出 $eval_rc" ;;
esac

hub_want="$(printf '%s\n' "$hub_help" | clap_commands)"
hook_want="$(printf '%s\n' "$hook_help" | clap_commands)"
eval_want="$(printf '%s\n' "$eval_help" | awk '/^recollect-eval / { print $2 }' | sort -u)"

[ -n "$hub_want" ] || red "recollect-hub --help 里没抽出任何子命令（Commands: 段形状变了）"
[ -n "$hook_want" ] || red "recollect-hook --help 里没抽出任何子命令（Commands: 段形状变了）"
[ -n "$eval_want" ] || red "recollect-eval 的用法行里没抽出任何子命令（用法形状变了）"

hub_got="$(surface hub)"
hook_got="$(surface hook)"
eval_got="$(surface eval)"

compare "hub 子命令" "$hub_got" "$hub_want"
compare "hook 子命令" "$hook_got" "$hook_want"
compare "eval 子命令" "$eval_got" "$eval_want"

# 旗标命令：没有子命令可数，对拍 README 块里必须出现的**关键旗标**。
missing_flags=""
for flag in --code --once --push --pull; do
  printf '%s\n' "$block" | grep -qF -- "$flag" || missing_flags="$missing_flags node:$flag"
done
for flag in --data-dir --replica; do
  printf '%s\n' "$block" | grep -qF -- "$flag" || missing_flags="$missing_flags mcp:$flag"
done
for flag in --capabilities --json; do
  printf '%s\n' "$block" | grep -qF -- "$flag" || missing_flags="$missing_flags doctor:$flag"
done
if [ -n "$missing_flags" ]; then
  echo "红：README 命令面块缺旗标：$missing_flags" >&2
  exit 3
fi

hub_n="$(printf '%s\n' "$hub_want" | wc -l | tr -d ' ')"
hook_n="$(printf '%s\n' "$hook_want" | wc -l | tr -d ' ')"
eval_n="$(printf '%s\n' "$eval_want" | wc -l | tr -d ' ')"
echo "OK：README 命令面与 --help 对拍一致（hub ${hub_n} 条，hook ${hook_n} 条，eval ${eval_n} 条；旗标命令 3 个共 8 个关键旗标）"
