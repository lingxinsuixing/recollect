#!/bin/bash
# 抽干排期的**离线**对照闸：不调模型，用一个桩 `hub` 把每条命令的读数按形状回出来，
# 只看"每一族今天到底开跑了几次、在什么条件下收手"。
#
# 为什么要有这一条：排期缺陷在真跑里几乎看不出来。2026-10-29 是 time 只审不抽，
# 2026-10-31 是共用一条钱包时 naming（`--batch-turns 1`，一轮一次调用）把抽取钱包吃光，
# `run_distill state` 与 event/fact 的抽取**一行日志都没有**——不是跑空，是一次都没开跑。
# 这类"排在后面的族饿死"只有把队列做成可控变量才暴露，所以这里用桩把每一族的队列长度做成可控的。
#
# **前提换了（2026-11-06 W19b）**：日脚本里已经没有夜间预算、也没有九档百分比，每一档跑到
# 自己的队列空（连续两次空批）或连续失败才收手。于是旧判据里的"每一族每夜都有一段"与
# "`used` 不越过总额"两条**不再成立**，本闸也不再断言它们（那不是坏，那是裁定本身）。
# 现在盯的是五类事：
#   1. **无帽**：naming 桩连发 45 次 `turns=1`，日脚本仍继续（旧上限是 40 次）；收手只因为
#      桩交回空批，而不是因为某一格数到了；
#   2. **自然终止**：每一族都有 `phase` 行、`why` 只有自然终止那几个词，没有"额度到"那一格；
#   3. **失败仍收手**：连失败闸照旧，毒批不许把整夜吃掉；
#   4. **连续隔离停族（W19c2/W19c2b/W19c2c）**：naming 恒交回 `state=quarantined` + `retry_exhausted=1`
#      ⇒ 第 5 发停在 `why=quarantine_streak`（`calls=5`，后面的族照常出现）；审核档走同一条墙、
#      判据换成它自己读数面的 `quarantined`/`errors`（naming 恒 `quarantined=1, remaining=5`
#      ⇒ 同样第 5 发停在同一个词上）；退避中的 `state=pending` 不停族（既不计数也不推进墙）；
#      **抽取档的质量隔离不算**（`state=quarantined` 而 `retry_exhausted=0` ⇒ 连隔离计数归零）；
#   5. **读数面**：`start`/`phase`/`end` 行里**没有** budget/caps/cap 那几格（砍掉的东西不许
#      从读数面爬回来）。
#   6. **保底档的维护步（W26）**：`--full` 必须把 `hub anomaly sweep` 真的发一次，phase 行
#      （kind=maintenance）带四格且等于读数；无 `--full` 的普通档不许跑它；它失败时只记
#      `fails`、四格 -1、下游各档照常收手（维护项不是数据面）。
#   7. **整批拒收是推进（W31）**：naming 抽取/审核连续交回**合法 JSON + rc=2**（`state:"rejected"`）
#      ⇒ 不许被连失败闸停族、`end.fails` 不涨、日志里无 `why=fail_streak`；阴性对照是同一份
#      合法 JSON 但 rc=3 ⇒ 连 3 发照旧 `fail_streak` 停（判据是退出码，不是读数正文）。
# 另加既有的六条：derive 被计量（`derive_check`）、episode 被计量且不动总账（`episode_check`）、
# 分诊三格"停在 -1"律（`poison_check`）、日志每行合法 JSON（`json_check`）、阳性对照（`live_check`）、
# `blanks=0`/`unparsed=0`。
#
# 每一种桩都必须**收敛**：日脚本已经没有额度兜底，桩若恒交回"还有活"，那一档就会转到天亮。
# 收敛一律走日脚本自己的判据（distill 的连续两次 `turns=0`；review 的 `remaining=0`；
# derive/episode 的空批 + 游标不动连续两次），不改日脚本的任何一格。
#
# 用法：bash tools/pipeline/test_drain_schedule.sh
#   本机 `python3` 是 Microsoft Store 空壳时，先加桩目录：
#   PATH="<repo>/.superpowers/sdd/2026-11-04-l2-l3-phase1/stubs/bin:$PATH" bash tools/pipeline/test_drain_schedule.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
# 自检：日脚本拼错了的话下面每条臂都会以奇怪的方式红，先把语法这一关单独报出来。
bash -n "$HERE/daily-drain.sh" || { echo "红：daily-drain.sh 语法不过（先修它，下面的臂无从跑起）"; exit 1; }
STUB_DIR=$(mktemp -d)
WORK=$(mktemp -d)
trap 'rm -rf "$STUB_DIR" "$WORK"' EXIT

cat > "$STUB_DIR/hub-stub" <<'STUB'
#!/bin/bash
# 桩：只回读数，不碰库。参数里的 --pass / --family 原样进读数，子命令决定读数的键。
# 每一臂的收敛点都写在下面的注释里——**没有额度帽之后，桩必须自己收敛**。
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    anomaly) sub=anomaly ;;
    backup) sub=backup ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill)
    if [ "$pass" = time ]; then
      # 时间回填是纯程序：第 3 发起交回 0 轮，配合日脚本"连续两次 0"的判据在第 4 发收。
      n=$(cat "$STUB_COUNT_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_COUNT_FILE"
      [ "$n" -ge 3 ] && turns=0 || turns=8
    else
      # 每一族各自的计数文件（naming 与其余各族的要求不同，共用一个文件会互相打乱）。
      #   naming：前 45 发各 1 轮（旧上限是 40），第 46 发起交回 0 ⇒ 日脚本在第 47 发收
      #           （45 发有产出 + 2 发空批），calls=47、used=45。
      #   其余族：前 2 发各 1 轮，第 3 发起交回 0 ⇒ 每族 4 发收。
      f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      if [ "$pass" = naming ]; then
        [ "$n" -le 45 ] && turns=1 || turns=0
      else
        [ "$n" -le 2 ] && turns=1 || turns=0
      fi
    fi
    printf '{"schema_version":1,"pass":"%s","family":"%s","turns":%s,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"pending","retry_exhausted":0}\n' "$pass" "$fam" "$turns"
    ;;
  review)
    # 审核臂：前 2 发 `remaining_pending_review=1`（队列里还有），第 3 发交回 `remaining=0`
    # ⇒ 日脚本按"队列清空"收手。**不能恒回 1**：旧版靠额度切片才没死循环，额度没了之后
    # 恒回 1 就是原地转到天亮。
    if [ "$pass" = rows ]; then printf '{"processed":0}\n'; else
      f="$STUB_REVIEW_DIR/$pass"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      [ "$n" -le 2 ] && rem=1 || rem=0
      printf '{"processed":1,"applied":1,"rejected":0,"quarantined":0,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0}\n' "$rem"
    fi
    ;;
  derive)
    # derive 臂（2026-11-04 I-1b / P-3）：键名与 `hub derive run --json` 的读数面一格不差
    # （`crates/hub/src/main.rs` 的 `derive_run`：schema_version / job / batch / rows / rejected
    # / dropped / quarantined / issue_id / cursor_moved / lag_rows）。
    # 这一臂存在的意义是**让排期闸看得见 derive 这一族**：没有它，桩交回的读数里没有 `rows` 键，
    # 日路径那一档就在零产出上空转而日志里说不出原因（控制器 2026-11-04 实跑即此形状）。
    # `rows=8` 是**桩给的量级**，不是真库的量级，更不是上限。`DERIVE_MAX_ROWS_PER_BATCH` = 40 数的
    # 是**选进来的输入行数**，而 `rows` 交回的是**实际插入的行数**：含 mention 与 I6 的别名补铸
    # （`crates/rc-derive/src/derive.rs:160-163` 把补铸并进 `rows_written`）⇒ 一批落库可以超过批宽 40。
    # 副本库实测（V-3 评审 A3，`<data-dir>/derive-evidence-accept2`）：一批（输入 ≤40 行）落
    # **51 行**，`memory_mention` 48→99，51 行全是 `source_agent='rc-derive'` 的别名补铸，同轮
    # `card`=36 与 `fact_link`=0 一动没动 ⇒ 上界 = 批行数 × 单行命中的锚数（观测到单行 5 个锚）。
    # 收敛：前 3 发有产出（`cursor_moved:true`），第 4 发起交回空批 + 游标不动 ⇒ 第 5 发收。
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    if [ "$n" -le 3 ]; then rows=8; moved=true; else rows=0; moved=false; fi
    printf '{"schema_version":1,"job":"derive","batch":1,"rows":%s,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":%s,"lag_rows":1}\n' "$rows" "$moved"
    ;;
  episode)
    # episode 臂（2026-11-04 I-6b）：键名与 `hub episode run --json` 的读数面一格不差
    # （`crates/hub/src/main.rs` 的 `episode_run`：schema_version / job / batch / candidates /
    # merged_pairs / span_pruned / episodes_written / members_linked / cards_written /
    # superseded / dropped_members / mid_band_deferred / cursor_moved / lag_rows）。
    # 这一臂存在的意义是**让排期闸看得见 episode 这一族**：没有它，桩交回的读数里没有
    # `episodes_written` 键 ⇒ `parse_episodes` 得 -1，日路径那一档按连失败闸收手而日志里
    # 说不出原因（I-6b 红态实证：两臂 episode 全是 why=fail_streak、episodes_total=0）。
    # `episodes_written=4` 是**桩给的量级**，不是真库的量级。收批判据落在游标（与 derive 同律）：
    # 第 3 发起 episodes_written=0 且 cursor_moved=false，配合日路径"连续两次空 + 游标不动"
    # 的判据在第 4 发收（why=queue_empty）。episode 计数用独立文件（STUB_EPISODE_FILE），
    # 免得与 time / derive 的计数互相打乱。
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  anomaly)
    # `hub anomaly sweep --json`（W26）的读数面：两族各一对，共四格。
    # 四个数**故意互不相同**（2/1/3/4）：`phase` 行四格的顺序若被接错（例如把 `resolved` 读成
    # `raised`），下面的 grep 立刻红——全 0 的桩挡不住这种错。
    # `STUB_ANOMALY_FAIL=1` 时这一臂整个失败（rc 3），用来钉"维护项失败只记账、不止损整条链"。
    if [ "${STUB_ANOMALY_FAIL:-0}" = 1 ]; then
      printf 'error: anomaly sweep failed — run: doctor\n' >&2
      exit 3
    fi
    printf '{"contradiction":{"raised":2,"resolved":1},"ghosts":{"raised":3,"resolved":4}}\n'
    ;;
  backup)
    # `hub backup`（W28 本体）的读数面：name / memory / ledger / core / memory_rows / extras 六格。
    # 真命令**没有 `--json` 旗标**（`BackupArgs` 里没有那一格），它总是印这一行；日路径也照原样发，
    # 桩跟着这个形状回。`memory_rows=42`、`extras=2` 故意非零，顺序接错（例如把 `extras`
    # 读成 `ledger`）下面的 grep 立刻红。`ledger` 恒是真文件名（必备段）；
    # `core` 交 **null**——私房本懒库缺席是合法读数（`parse_backup` 的三态 core），
    # 桩钉住"null 不许被读成 -1"这一条。
    # 被调过就记一行标记：判据读标记文件，才分得清"日路径真的发了这一发"与"日志里刚好有一行"。
    # `STUB_BACKUP_FAIL=1` 时这一臂整个失败（rc 3），用来钉"备份失败只记账、不止损整条链"。
    if [ -n "${STUB_BACKUP_MARK:-}" ]; then printf 'backup\n' >> "$STUB_BACKUP_MARK"; fi
    if [ "${STUB_BACKUP_FAIL:-0}" = 1 ]; then
      printf 'error: backup failed — run: doctor\n' >&2
      exit 3
    fi
    printf '{"backup":true,"name":"transcript-1794000000000.db","memory":"memory-1794000000000.db","ledger":"ledger-1794000000000.db","core":null,"memory_rows":42,"extras":2}\n'
    ;;
  *) printf '{}\n' ;;
esac
exit 0
STUB
chmod +x "$STUB_DIR/hub-stub"

# 失败桩：每一发都回"读数 + error: llm timeout + rc 3"——就是 2026-10-31 实测的形状
# （读数里的 `turns` 照样是批大小，所以只看 `turns` 会把"永远失败的同一批"读成"还在推进"）。
cat > "$STUB_DIR/hub-fail" <<'STUBFAIL'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill) printf '{"schema_version":1,"pass":"%s","family":"%s","turns":8,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"pending","retry_exhausted":0}\nerror: llm timeout\n' "$pass" "$fam" ;;
  review) printf '{"processed":0,"applied":0,"rejected":0,"quarantined":0,"skipped":0,"conflicts":0,"errors":1,"exhausted":false,"remaining_pending_review":1,"remaining_quarantined":0,"remaining_approved":0}\nerror: llm timeout\n' ;;
  derive) printf '{"schema_version":1,"job":"derive","batch":1,"rows":8,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":true,"lag_rows":1}\nerror: llm timeout\n' ;;
  episode) printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":4,"members_linked":9,"cards_written":4,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":true,"lag_rows":1}\nerror: llm timeout\n' ;;
esac
exit 3
STUBFAIL
chmod +x "$STUB_DIR/hub-fail"

# 整批拒收桩（W31）：naming 抽取连续 45 发交回**一行合法 JSON + rc=2**（`state:"rejected"`），
# 就是 2026-11-07 现场实测的形状：`hub distill once` 对被程序闸拒收的轮打印 `state:"rejected"`
# 并以 rc=2 退出（`crates/hub/src/main.rs` 的"退出码即契约"：0 全收 / 2 整批拒 / 3 运维失败）。
# 第 46、47 发交回空批（rc=0）⇒ 日脚本按自己的"连续两次空"收手（calls=47）。
# 这一臂钉的是"rc=2 是推进，不是座位故障"：旧脚本把任何 rc≠0 都记进连失败 ⇒ naming 会停在
# calls=3 / why=fail_streak、end.fails=3。naming 审核也交回两发 rc=2（共享契约的"整批拒收"
# 口径，覆盖 `run_review` 那一支），其余各族与主桩同形、各自收敛在自己的队列空上。
# `STUB_REJECT_RC` 把**抽取档**的退出码换成 3（读数形状一字不改）：阴性对照——同一份
# `state:"rejected"` 合法 JSON，只要退出码是 3，连失败闸就必须照旧在第 3 发收手。
# 判据是退出码，不是读数正文。审核档不跟这一格走（恒 rc=2），否则 `end.fails` 会被它搅成 5。
cat > "$STUB_DIR/hub-reject" <<'STUBREJECT'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
rc=0
case "$sub" in
  distill)
    if [ "$pass" = time ]; then
      n=$(cat "$STUB_COUNT_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_COUNT_FILE"
      [ "$n" -ge 3 ] && turns=0 || turns=8
      state=done
    else
      f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      if [ "$pass" = naming ] && [ "$n" -le 45 ]; then
        turns=1; state=rejected; rc=${STUB_REJECT_RC:-2}
      elif [ "$n" -le 2 ]; then
        turns=1; state=done
      else
        turns=0; state=empty
      fi
    fi
    printf '{"schema_version":1,"pass":"%s","family":"%s","turns":%s,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"%s","retry_exhausted":0}\n' "$pass" "$fam" "$turns" "$state"
    ;;
  review)
    f="$STUB_REVIEW_DIR/$pass"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    if [ "$pass" = naming ] && [ "$n" -le 2 ]; then
      # 审核档**不跟** `STUB_REJECT_RC` 走：阴性对照要钉的只是抽取档那 3 发 rc=3，
      # 审核档照旧两发 rc=2（同一轮里 `end.fails` 因此恰好等于 3，不被这一支搅成 5）。
      rem=5; rc=2
    else
      [ "$n" -le 2 ] && rem=1 || rem=0
    fi
    printf '{"processed":1,"applied":0,"rejected":1,"quarantined":0,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0}\n' "$rem"
    ;;
  derive)
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    if [ "$n" -le 3 ]; then rows=8; moved=true; else rows=0; moved=false; fi
    printf '{"schema_version":1,"job":"derive","batch":1,"rows":%s,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":%s,"lag_rows":1}\n' "$rows" "$moved"
    ;;
  episode)
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  *) printf '{}\n' ;;
esac
exit $rc
STUBREJECT
chmod +x "$STUB_DIR/hub-reject"

# 隔离熔断桩（W19c2c）：naming 抽与 naming 审**恒交回隔离**，其余各族照常收敛。
# 这一臂要证明的是"连续 5 个隔离就停这一族"——它必须由日脚本自己的墙收手：
# 桩恒回隔离（每发都有产出），没有墙的话这一档会转到天亮。
#   抽取：`turns=1` + `state=quarantined` + `retry_exhausted=1`（脚本只认这一格，第 5 发触发墙
#         ⇒ calls=5）；其余各族的 `retry_exhausted` 恒 0（质量隔离/正常批都不推进墙）。
#   审核：`quarantined=1` + `remaining_pending_review=5`（第 5 发触发审核档那条墙 ⇒ calls=5）。
#   其余族：抽取前 2 发 1 轮、第 3 发起空批（4 发收）；审核前 2 发 rem=1、第 3 发 rem=0；
#   derive / episode 与主桩同序列。后三档照常出现，证明"停的只是这一族"。
cat > "$STUB_DIR/hub-quarantine" <<'STUBQUAR'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill)
    f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    if [ "$pass" = naming ]; then
      turns=1; state=quarantined; ex=1
    else
      if [ "$pass" = time ]; then [ "$n" -le 2 ] && turns=1 || turns=0
      elif [ "$n" -le 2 ]; then turns=1; else turns=0; fi
      state=done; ex=0
    fi
    printf '{"schema_version":1,"pass":"%s","family":"%s","turns":%s,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"%s","retry_exhausted":%s}\n' "$pass" "$fam" "$turns" "$state" "$ex"
    ;;
  review)
    f="$STUB_REVIEW_DIR/$pass"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    if [ "$pass" = naming ]; then
      q=1; rem=5
    else
      q=0
      [ "$n" -le 2 ] && rem=1 || rem=0
    fi
    printf '{"processed":1,"applied":0,"rejected":0,"quarantined":%s,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0}\n' "$q" "$rem"
    ;;
  derive)
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    if [ "$n" -le 3 ]; then rows=8; moved=true; else rows=0; moved=false; fi
    printf '{"schema_version":1,"job":"derive","batch":1,"rows":%s,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":%s,"lag_rows":1}\n' "$rows" "$moved"
    ;;
  episode)
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  *) printf '{}\n' ;;
esac
exit 0
STUBQUAR
chmod +x "$STUB_DIR/hub-quarantine"

# 重试不误停桩（W19c2c）：naming 交替交回 `state=pending`（第 1、3、5 发）与 `state=done`
# （第 2、4、6 发），第 7、8 发空批 ⇒ 日脚本按"连续两次空"收手（calls=8）。
# 钉的是"pending 不计数；只有 retry_exhausted 推进隔离墙"：那一格既不推进隔离墙，
# 也不走连失败闸，隔离计数恒 0。
cat > "$STUB_DIR/hub-retry" <<'STUBRETRY'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill)
    f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    if [ "$pass" = naming ] && [ "$n" -le 6 ]; then
      turns=1
      if [ $((n % 2)) -eq 1 ]; then state=pending; else state=done; fi
    else
      if [ "$pass" = time ]; then [ "$n" -le 2 ] && turns=1 || turns=0
      elif [ "$n" -le 2 ]; then turns=1; else turns=0; fi
      state=done
    fi
    printf '{"schema_version":1,"pass":"%s","family":"%s","turns":%s,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"%s"}\n' "$pass" "$fam" "$turns" "$state"
    ;;
  review)
    if [ "$pass" = rows ]; then printf '{"processed":0}\n'; else
      f="$STUB_REVIEW_DIR/$pass"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      [ "$n" -le 2 ] && rem=1 || rem=0
      printf '{"processed":1,"applied":1,"rejected":0,"quarantined":0,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0}\n' "$rem"
    fi
    ;;
  derive)
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    if [ "$n" -le 3 ]; then rows=8; moved=true; else rows=0; moved=false; fi
    printf '{"schema_version":1,"job":"derive","batch":1,"rows":%s,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":%s,"lag_rows":1}\n' "$rows" "$moved"
    ;;
  episode)
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  *) printf '{}\n' ;;
esac
exit 0
STUBRETRY
chmod +x "$STUB_DIR/hub-retry"

# 分诊序列桩（2026-11-04 I-2c / T-1）：与 hub-stub 差两处——
#   ① derive 臂逐发交回 `$STUB_DERIVE_SPEC` 的第 n 行（`nm ne ag` 三个数），行内容是单个 `-`
#      时读数里**根本没有那三格**（= 旧二进制那一型，`parse_derive_rejects` 恒交回 -1）；
#      spec 行读完之后交回空批 + 游标不动 ⇒ 由日脚本自己收手（旧版靠额度恰好切在第 5 发，
#      额度没了之后必须由桩自己收敛，否则这一臂会转到天亮）。
#   ② 抽取各族（含 naming）与 time 都两发就收敛：这一臂只判 derive 的分诊读数，不重复
#      "无帽"那一条（那一条在主臂上一次就够；45 发 × 3 个臂只是把闸拖长好几分钟）。
# 为什么必须有这一支：既有的 hub-stub 没有那三格，分诊三格因此**永远**走"读不出"那一支，
# 于是"被读不出打回 -1 之后又复活成部分和"这一整族缺陷（I-2c 的 M-1）在别的臂里全静默。
cat > "$STUB_DIR/hub-seq" <<'STUBSEQ'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill)
    f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    if [ "$pass" = time ]; then [ "$n" -le 2 ] && turns=8 || turns=0
    else [ "$n" -le 2 ] && turns=1 || turns=0
    fi
    printf '{"schema_version":1,"pass":"%s","family":"%s","turns":%s,"anchors":0,"mentions":0,"filtered":0,"system_turns_skipped":0,"state":"pending","retry_exhausted":0}\n' "$pass" "$fam" "$turns"
    ;;
  review)
    if [ "$pass" = rows ]; then printf '{"processed":0}\n'; else
      f="$STUB_REVIEW_DIR/$pass"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      [ "$n" -le 2 ] && rem=1 || rem=0
      printf '{"processed":1,"applied":1,"rejected":0,"quarantined":0,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0}\n' "$rem"
    fi
    ;;
  derive)
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    line=$(sed -n "${n}p" "$STUB_DERIVE_SPEC")
    if [ -z "$line" ]; then
      # spec 读完了 = 队列空（空批 + 游标不动）；连续两次由日脚本自己收手。
      # 三格照样交回 **0**（现代二进制的读数里它们恒在）：这一发是"可读的空批"，不是
      # "读不出来"。要是这里把三格漏掉，收手那两发会把整档打回 -1，阳性对照永远亮不起来
      # （本闸 2026-11-06 就是这么先红了一次）——那三格的语义是"这一族今天拒了几条"，
      # 空批是"拒了 0 条"，不是"这一格我不认识"。
      printf '{"schema_version":1,"job":"derive","batch":1,"rows":0,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":false,"lag_rows":1,"rejected_no_mechanism":0,"rejected_no_evidence":0,"rejected_anchor_gate":0}\n'
    elif [ "$line" = "-" ]; then
      # 旧二进制形状：`rows`/`cursor_moved` 都在，只有分诊那三格缺席。
      printf '{"schema_version":1,"job":"derive","batch":1,"rows":8,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":true,"lag_rows":1}\n'
    else
      read -r a b c <<<"$line"
      printf '{"schema_version":1,"job":"derive","batch":1,"rows":8,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":true,"lag_rows":1,"rejected_no_mechanism":%s,"rejected_no_evidence":%s,"rejected_anchor_gate":%s}\n' "$a" "$b" "$c"
    fi
    ;;
  episode)
    # episode 臂（2026-11-04 I-6b）：与 hub-stub 的 episode 臂同形（收敛序列：第 3 发起
    # episodes_written=0 且游标不动）。分诊序列臂换的档只管 derive 那一族（STUB_DERIVE_SPEC），
    # episode 这一族照常收敛，分诊三臂的读数因此不会因加档而变形。
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  *) printf '{}\n' ;;
esac
exit 0
STUBSEQ
chmod +x "$STUB_DIR/hub-seq"

# 半坏的 python3（I-2c / T-1 第二臂 · M-2）：**只**让 `parse_derive_rejects` 那一支的调用交回空串
# （本机 `python3` 是 Microsoft Store 空壳时的形状），其余 python3 调用原样转给真 python。
# 挑这一支的唯一钩子是它脚本里那个键名 `rejected_no_mechanism`——`parse_rows`/`parse_turns`/
# `parse_cursor_moved`/`jstr` 都不带这个词，所以坏只坏在新解析器那一格。
# 这一臂的存在意义是**阳性对照**：同一轮日志里其余行照样全部解析通过，只有那一行 `phase` 非法。
REAL_PYTHON=$(command -v python3 || true)
# 存在 ≠ 能跑：本机 `python3` 可能是 WindowsApps 商店假身，一跑就打"Python was not found"
# （2026-11-05 控制器复跑实录：解析层四条红，episode 臂被连带掩盖）。
if [ -z "$REAL_PYTHON" ] || ! "$REAL_PYTHON" -c 'pass' >/dev/null 2>&1; then
  echo "红：本机 python3 不存在或不能跑（商店假身？），桩臂无从执行。修法：PATH 前置指向真 python 的 python3 shim（如 exec /c/Python312/python \"\$@\"）"
  exit 1
fi
export REAL_PYTHON
mkdir -p "$STUB_DIR/brokenpy"
cat > "$STUB_DIR/brokenpy/python3" <<'BROKENPY'
#!/bin/bash
for a in "$@"; do
  case $a in
    *rejected_no_mechanism*) exit 0 ;;   # 空串、退出码 0：解析器"跑起来了但什么都没交回"
  esac
done
exec "$REAL_PYTHON" "$@"
BROKENPY
chmod +x "$STUB_DIR/brokenpy/python3"

# 桩需要 `bin/` 目录形状（脚本取 `ls $DATA/bin/hub-*` 的最后一个当二进制）。
mkdir -p "$WORK/bin" "$WORK/logs"
cp "$STUB_DIR/hub-stub" "$WORK/bin/hub-0000000"
# （W28 本体落地后）脚本侧不再有 `run_cold_backup`——ledger/core 的冷备是 `hub backup`
# 组内的事，桩不碰库，所以 WORK 里也不再需要预建 ledger.db/core.db 的夹具。
# 每一族各自的计数文件（time / 抽取各族 / 审核各族 / derive / episode）。每次跑前清空——
# "第几发"永远从每一臂的第一发起算，臂与臂之间不许串味。
export STUB_COUNT_FILE="$STUB_DIR/time_count"
export STUB_DERIVE_FILE="$STUB_DIR/derive_count"
export STUB_EPISODE_FILE="$STUB_DIR/episode_count"
export STUB_DISTILL_DIR="$STUB_DIR/distill_counts"
export STUB_REVIEW_DIR="$STUB_DIR/review_counts"
# `hub backup` 被调过几次（W28）：只记一行/次，`run_once` 每次清空。
export STUB_BACKUP_MARK="$STUB_DIR/backup_mark"

run_once() { # [hub 桩路径] [排在 PATH 最前的目录] [传给日脚本的额外参数] -> 把这一轮的读数打到 stdout
  local hubbin=${1:-$WORK/bin/hub-0000000} extrapath=${2:-} mode=${3:-}
  rm -rf "$STUB_DISTILL_DIR" "$STUB_REVIEW_DIR"
  mkdir -p "$STUB_DISTILL_DIR" "$STUB_REVIEW_DIR"
  rm -f "$STUB_COUNT_FILE" "$STUB_DERIVE_FILE" "$STUB_EPISODE_FILE" "$STUB_BACKUP_MARK"
  : > "$WORK/logs/daily-test.jsonl"
  RECOLLECT_DATA_DIR=$WORK RECOLLECT_HUB=$hubbin \
  RECOLLECT_LLM_CONFIG=$WORK/llm.toml RECOLLECT_LOG=$WORK/logs/daily-test.jsonl \
  RECOLLECT_DRAIN_SLEEP=0 \
  PATH="${extrapath:+$extrapath:}$PATH" bash "$HERE/daily-drain.sh" ${mode:+"$mode"} 2>/dev/null
  python3 - "$WORK/logs/daily-test.jsonl" <<'PY'
import json, sys
blanks = 0
unparsed = 0
# budget / caps / cap 三格在**任何**一行里出现几次：砍掉的东西不许从读数面爬回来（W19b）。
stray_keys = 0
for raw in open(sys.argv[1], encoding="utf-8"):
    line = raw.strip()
    if not line:
        blanks += 1                   # 空行 = 日志形状坏了（`log()` 的空载荷守卫守的就是这一条）
        continue
    try:
        r = json.loads(line)
    except Exception:
        print("unparsed:", line[:80])
        unparsed += 1                 # 非法 JSON 的行数（I-2c/M-2 的判据就读这一格：坏的那一行必须为 0）
        continue
    for k in ("budget", "caps", "cap"):
        if k in r:
            stray_keys += 1
            print("stray:", k, "in", line[:80])
    if r.get("t") == "phase":
        # `rows_total` 只有 derive 那一档有（2026-11-04 I-1b / P-2：`used` 记次、行数另记）。
        # 附在最后，不改前面各格的列序——`check`/`review_check` 的 grep 吃的是行首到 calls= 那一段。
        extra = "" if "rows_total" not in r else " rows_total=%s" % (r["rows_total"],)
        # 分诊三格也附在最后（2026-11-04 I-2c）：`rej=<nm>/<ne>/<ag>`，读不出＝-1。
        # 与上面同一律：不加列、只追加，既有各条 grep 吃的段序一个不动。
        if "rej_no_mechanism_total" in r:
            extra += " rej=%s/%s/%s" % (r["rej_no_mechanism_total"],
                                        r["rej_no_evidence_total"],
                                        r["rej_anchor_gate_total"])
        # episode 档的行数格与游标格也附在最后（2026-11-04 I-6b），同一律：只追加。
        if "episodes_total" in r:
            extra += " episodes_total=%s cursor_moved=%s" % (r["episodes_total"], r.get("cursor_moved"))
        # 维护档（`--full` 的 anomaly sweep，W26）的四格也附在最后，同一律：只追加。
        # 四格读不出来时是 -1（`parse_anomaly`），`why` 另说"这一发跑成了没有"。
        if "contradiction_raised" in r:
            extra += " contradiction=%s/%s ghosts=%s/%s" % (
                r["contradiction_raised"], r["contradiction_resolved"],
                r["ghosts_raised"], r["ghosts_resolved"])
        # 备份档（`--full` 的 run_backup，W28）的读数格也附在最后，同一律：只追加。
        # `hub backup` 的读数面同名同层（name / memory / ledger / core / memory_rows / extras）；
        # 读不出来时是 -1，`core=null`（懒库缺席的合法读数）原样落行。
        if "memory_rows" in r:
            extra += " name=%s memory=%s ledger=%s core=%s memory_rows=%s extras=%s" % (
                r["name"], r["memory"], r.get("ledger"), r.get("core"),
                r["memory_rows"], r["extras"])
        # W19c2c：`quarantined` 一格（抽取档数重试耗尽的发数，审核档数本族隔离条数），同一律：只追加。
        if "quarantined" in r:
            extra += " quarantined=%s" % (r["quarantined"],)
        print("%s %-7s %-9s calls=%s why=%s used=%s%s" % (
            r["kind"], r["pass"], r.get("family") or "", r["calls"], r["why"], r["used"], extra))
    # 注意键名：`start`/`end` 两行用的是**时间戳本身**当键（`{"start":…}` / `{"end":…}`），
    # 没有 `t` 那一格——上一版按 `t == "end"` 取，永远取不到，于是把"end 行没打出来"
    # 误报成"end 行缺 fails 那一格"（原始日志里 `fails:26` 一格不少）。
    if "start" in r:
        print("start episode_calls_cap=%s" % (r.get("episode_calls_cap"),))
    # （W28 本体落地后脚本侧不再产 `t:"cold_backup"` 行——ledger/core 在 `hub backup` 组内，
    # 人读面的该行打印随之退休。summarize_drain.py 的同名分支变成死分支，无害。）
    if "end" in r:
        # W19c2c 追加一格（只追加）：抽取档因重试耗尽整批隔离的发数总账。
        # 审核档的隔离条数**不在这里**（它只进自己那一行 phase，单位是条不是批）。
        tail = ""
        if "quarantined" in r:
            tail += " quarantined=%s" % (r["quarantined"],)
        print("end used=%s fails=%s%s" % (r.get("used"), r.get("fails"), tail))
print("blanks=%d" % blanks)
print("unparsed=%d" % unparsed)
print("stray_keys=%d" % stray_keys)
PY
}

: > "$WORK/llm.toml"   # 脚本只查存在性

echo "== 无帽主臂：naming 桩连发 45 次 turns=1，日脚本必须继续（旧上限 40 不在）=="
BMAIN=$(run_once); printf '%s\n' "$BMAIN"
echo "== 失败臂：每一发都 rc=3（连失败闸仍收手，毒批不许吃整夜）=="
BFAIL=$(run_once "$STUB_DIR/hub-fail"); printf '%s\n' "$BFAIL"
echo "== 隔离熔断臂：naming 恒 state=quarantined ⇒ 第 5 发停这一族（why=quarantine_streak），后面的族照常出现 =="
BQUAR=$(run_once "$STUB_DIR/hub-quarantine"); printf '%s\n' "$BQUAR"
echo "== 重试不误停臂：naming 交替 state=pending → state=done ⇒ 不停族、隔离计数恒 0 =="
BRETRY=$(run_once "$STUB_DIR/hub-retry"); printf '%s\n' "$BRETRY"

echo "== rc=2 推进臂（W31）：naming 连发 45 次合法 JSON + rc=2（state=rejected）⇒ 不停族、fails 不涨 =="
BREJECT=$(run_once "$STUB_DIR/hub-reject"); printf '%s\n' "$BREJECT"
# 判据落在**原始 JSONL**上（人读面另说）：这一夜一行 `"why":"fail_streak"` 都不许有。
reject_streak=$(grep -c '"why":"fail_streak"' "$WORK/logs/daily-test.jsonl" 2>/dev/null || true)
reject_streak=${reject_streak:-0}

echo "== rc=2 阴性对照臂（W31）：同一份 state=rejected 合法 JSON、但 rc=3 ⇒ 连 3 发仍 fail_streak 停 =="
STUB_REJECT_RC=3; export STUB_REJECT_RC
BREJECT3=$(run_once "$STUB_DIR/hub-reject")
unset STUB_REJECT_RC
printf '%s\n' "$BREJECT3"

# ---- 保底档（`--full`，W26）-----------------------------------------------------------------
# 两条臂，钉的是"维护步真的被日路径接上了"与"它坏了不许伤整条链"：
#   ① 正常：队尾的 `hub anomaly sweep` 发一次、phase 行带四格，且四格等于桩交回的四个数
#      （2/1/3/4，互不相同——顺序接错就红）；
#   ② 失败：这一臂 rc=3 ⇒ phase 行 why=fail_streak、四格 -1、`end.fails=1`，而其余各档的
#      phase 行照样在（维护项是维护项，不是数据面；它失败不该让这一夜的读数作废）。
# 另有一条反向判据：**无 `--full` 的普通档不跑 sweep**（保底档才跑，普通档保持轻量）。
echo "== 保底档臂①（--full）：队尾的 anomaly sweep 真跑一次，phase 行四格等于读数 =="
BFULL=$(run_once "$STUB_DIR/hub-stub" "" --full); printf '%s\n' "$BFULL"
echo "== 保底档臂②（--full + sweep 失败）：只记 fails，不止损整条链 =="
STUB_ANOMALY_FAIL=1; export STUB_ANOMALY_FAIL
BFULLFAIL=$(run_once "$STUB_DIR/hub-stub" "" --full)
unset STUB_ANOMALY_FAIL
printf '%s\n' "$BFULLFAIL"

# ---- 备份档（`--full` 的 run_backup，W28 本体）----------------------------------------------
# 三条判据，钉的是"备份真的被日路径接上了、普通档不跑、失败不止损整条链"：
#   ① `--full` ⇒ `hub backup` 恰好发一次（标记文件一行/次），phase 行六格等于桩交回的
#      读数（ledger=真文件名、core=null——懒库缺席的合法读数不许被读成 -1；42/2 故意非零，
#      顺序接错即红）；
#   ② 普通档（无 `--full`）一次都不许调 `hub backup`——备份只跟保底档走；
#   ③ `STUB_BACKUP_FAIL=1` ⇒ 这一臂 rc=3 ⇒ phase 行 why=fail_streak、六格 -1、
#      `end.fails=1`，而 episode 那一档的 phase 行照样在（备份失败不许掐断整条链）。
echo "== 备份臂①（--full）：hub backup 发一次，phase 行六格等于桩读数 =="
BBACKUP=$(run_once "$STUB_DIR/hub-stub" "" --full); printf '%s\n' "$BBACKUP"
backup_calls=0
if [ -f "$STUB_BACKUP_MARK" ]; then backup_calls=$(wc -l < "$STUB_BACKUP_MARK"); fi
backup_calls=${backup_calls:-0}

echo "== 备份臂②（--full + backup 失败）：只记 fails，不止损整条链 =="
STUB_BACKUP_FAIL=1; export STUB_BACKUP_FAIL
BBACKUPFAIL=$(run_once "$STUB_DIR/hub-stub" "" --full)
unset STUB_BACKUP_FAIL
printf '%s\n' "$BBACKUPFAIL"

# ---- 分诊三格的三臂（2026-11-04 I-2c）--------------------------------------------------
# 序列桩自己收敛（spec 读完之后空批 + 游标不动），所以"第 1 发可读 / 第 2 发读不出 /
# 第 3..5 发可读 + 2 发收手"这个序列在一轮里跑完，不依赖任何额度恰好切在哪一发。
echo "== 分诊臂①（M-1）：一发读不出 + 之后三发可读 ⇒ 三格必须停在 -1，不许复活成部分和 =="
printf '%s\n' '2 1 0' '-' '5 5 5' '5 5 5' '5 5 5' > "$STUB_DIR/spec-poison"
# 用 `export` 而不是 `VAR=… run_once`：函数调用前缀的赋值不保证传给函数**再派下去**的桩，
# 而这一格的读者正是桩本身（它按发数读第 n 行）。序列必须在三次分诊臂之间逐臂换档，
# `run_once` 会把 derive 的发数计数器清零，所以"第几发"永远从这一臂的第一发起算。
export STUB_DERIVE_SPEC="$STUB_DIR/spec-poison"
BPOISON=$(run_once "$STUB_DIR/hub-seq"); printf '%s\n' "$BPOISON"

echo "== 分诊臂②（M-2）：只有 parse_derive_rejects 那一支交回空串 ⇒ 日志每行必须是合法 JSON =="
BJSON=$(run_once "$STUB_DIR/hub-stub" "$STUB_DIR/brokenpy"); printf '%s\n' "$BJSON"

echo "== 分诊臂③（T-3 阳性对照）：整轮全部可读 ⇒ 三格必须是桩交回值之和（不许恒 -1）=="
printf '%s\n' '1 2 3' '4 5 6' '7 8 9' '1 0 2' '3 1 4' > "$STUB_DIR/spec-live"
export STUB_DERIVE_SPEC="$STUB_DIR/spec-live"
BLIVE=$(run_once "$STUB_DIR/hub-seq"); printf '%s\n' "$BLIVE"

fail=0
check() { # <label> <text> <phase 行里必须出现的 "kind pass family">
  local label=$1 text=$2 want=$3
  if ! printf '%s\n' "$text" | grep -qE "^distill $want .*calls=[1-9]"; then
    echo "红：$label 里 $want 没有开跑（phase 行缺失或 calls=0）"; fail=1
  fi
}
check "无帽主臂 抽取" "$BMAIN" "naming"
check "无帽主臂 抽取" "$BMAIN" "event"
check "无帽主臂 抽取" "$BMAIN" "fact"
check "无帽主臂 抽取" "$BMAIN" "state"

review_check() { # <label> <text> <pass>
  local label=$1 text=$2 pas=$3
  if ! printf '%s\n' "$text" | grep -qE "^review +$pas .*calls=[1-9]"; then
    echo "红：$label 里 review $pas 没有开跑"; fail=1
  fi
}
for pas in naming event fact time state; do
  review_check "无帽主臂 审核" "$BMAIN" "$pas"
done

# ---- 无帽主臂：45 发有产出仍然继续（旧上限 40 不在）----------------------------------------
# 桩给 naming 45 发 `turns=1`，第 46、47 发交回空批（日脚本的"连续两次空"判据收手）。
# 于是这一行必须是 calls=47、used=45、why=queue_empty。旧上限若还在，calls 会停在 40、
# why 会是"额度到"那一格——两个数都当场红。
naming_line=$(printf '%s\n' "$BMAIN" | grep -E '^distill naming ' | head -1)
if [ -z "$naming_line" ]; then
  echo "红：无帽主臂里没有 naming 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$naming_line" | grep -qE "calls=47( |$)"; then
    echo "红：naming 连发 45 次有产出后仍应继续（calls=47：45 发有 + 2 发收手），实测：$naming_line"; fail=1
  fi
  if ! printf '%s\n' "$naming_line" | grep -qE "used=45( |$)"; then
    echo "红：naming 的 used 应是 45（45 轮），实测：$naming_line"; fail=1
  fi
  if ! printf '%s\n' "$naming_line" | grep -q "why=queue_empty"; then
    echo "红：naming 收手的原因应是队列空（不是额度到），实测：$naming_line"; fail=1
  fi
fi
# 审核族同理收在"队列清空/停住"上（桩第 3 发交回 remaining=0）。
review_line=$(printf '%s\n' "$BMAIN" | grep -E '^review +naming ' | head -1)
if [ -z "$review_line" ]; then
  echo "红：无帽主臂里没有 review naming 的 phase 行"; fail=1
elif ! printf '%s\n' "$review_line" | grep -q "why=queue_empty_or_stalled"; then
  echo "红：review naming 收手的原因应是队列空/停住，实测：$review_line"; fail=1
fi

# derive 档的计量证据（2026-11-04 I-1b / P-6）：这一族的 `phase` 行必须存在，且必须"真的被计量过"。
# 三件事都可判：
#   calls>0                —— 这一族开跑了（与前面各族同一条律）；
#   why 不是 fail_streak   —— 读数解析得出来。解析不出来（桩少了 derive 臂就是这个形状：读数里
#                            没有 `rows` 键 ⇒ `parse_rows` 得 -1）时，日路径按连失败闸收手，
#                            `why` 落 `fail_streak`，这一条当场红；
#   rows_total>0           —— 落库行数另记进了这一格（`used` 记的是次，见 daily-drain.sh 的 P-1）。
derive_check() { # <label> <text>
  local label=$1 text=$2 line
  line=$(printf '%s\n' "$text" | grep -E "^derive " | head -1)
  if [ -z "$line" ]; then
    echo "红：$label 里没有 derive 的 phase 行（这一族一次都没开跑）"; fail=1; return
  fi
  if ! printf '%s\n' "$line" | grep -qE "calls=[1-9]"; then
    echo "红：$label 的 derive 行 calls=0：$line"; fail=1
  fi
  if printf '%s\n' "$line" | grep -q "why=fail_streak"; then
    echo "红：$label 的 derive 行 why=fail_streak（读数解析不出来，十有八九是桩缺了 derive 臂）：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "rows_total=[1-9]"; then
    echo "红：$label 的 derive 行没有 rows_total>0（这一族没被计量出行数）：$line"; fail=1
  fi
}
derive_check "无帽主臂 推导" "$BMAIN"

# episode 档的计量证据（2026-11-04 I-6b，与 derive_check 同构）：这一族的 `phase` 行必须存在，
# 且必须"真的被计量过"。三件事都可判：
#   calls>0           —— 这一族开跑了（与前面各族同一条律）；
#   why 非 fail_streak —— 读数解析得出来。解析不出来（桩缺 episode 臂就是这个形状：读数里
#                        没有 `episodes_written` 键 ⇒ `parse_episodes` 得 -1）时，日路径按
#                        连失败闸收手，`why` 落 fail_streak，这一条当场红；
#   episodes_total>0  —— 落库行数另记进了这一格（`used` 恒 0，见 daily-drain.sh 的 I-6b ①）。
episode_check() { # <label> <text>
  local label=$1 text=$2 line
  line=$(printf '%s\n' "$text" | grep -E '^episode ' | head -1)
  if [ -z "$line" ]; then
    echo "红：$label 里没有 episode 的 phase 行（这一族一次都没开跑）"; fail=1; return
  fi
  if ! printf '%s\n' "$line" | grep -qE "calls=[1-9]"; then
    echo "红：$label 的 episode 行 calls=0：$line"; fail=1
  fi
  if printf '%s\n' "$line" | grep -q "why=fail_streak"; then
    echo "红：$label 的 episode 行 why=fail_streak（读数解析不出来，十有八九是桩缺了 episode 臂）：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "episodes_total=[1-9]"; then
    echo "红：$label 的 episode 行没有 episodes_total>0（这一族没被计量出行数）：$line"; fail=1
  fi
}
episode_check "无帽主臂 归组" "$BMAIN"

# episode 档的记账口径（I-6b）：纯程序零调用 ⇒ episode 的 phase 行 used == end 行 used。
# 只在无帽主臂上核（读数最全）。
ep_used=$(printf '%s\n' "$BMAIN" | grep -E '^episode ' | head -1 | sed -n 's/.* used=\([0-9-]*\).*/\1/p')
end_used=$(printf '%s\n' "$BMAIN" | grep -E '^end ' | sed -n 's/.* used=\([0-9-]*\).*/\1/p')
if [ -z "$ep_used" ] || [ -z "$end_used" ] || [ "$ep_used" != "$end_used" ]; then
  echo "红：episode 的 phase 行 used=$ep_used 与 end 行 used=$end_used 不等——这一档动了总账的账（纯程序应是 0 调用）"; fail=1
fi

# ---- 保底档的维护步（`--full` 的 anomaly sweep，W26）------------------------------------
# 三条判据：
#   ① full 臂必须有 `kind=maintenance` / `pass=anomaly` 的 phase 行，calls=1、why=ok，且四格
#      **恰好等于**桩交回的那四个数（2/1/3/4，互不相同）——四格顺序接错、把 -1 写成 0、或压根
#      没跑，三种坏形状各红在不同格上；
#   ② 普通臂（无 `--full`）**不许**出现这一行——维护步只在保底档跑，普通触发档保持轻量；
#   ③ full + 失败臂：why=fail_streak、四格 -1（读不出来不猜 0）、`end.fails=1`（只记一次），
#      且 episode 那一档的 phase 行仍在（维护项失败不许掐断整条链）。
anomaly_line() { printf '%s\n' "$1" | grep -E '^maintenance +anomaly ' | head -1; }
line=$(anomaly_line "$BFULL")
if [ -z "$line" ]; then
  echo "红：保底档臂①里没有 maintenance anomaly 的 phase 行（--full 没把 sweep 接上）"; fail=1
else
  if ! printf '%s\n' "$line" | grep -qE "calls=1( |$)"; then
    echo "红：保底档的 sweep 应恰好发 1 次，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -q "why=ok"; then
    echo "红：保底档的 sweep 应记 why=ok，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "contradiction=2/1 ghosts=3/4"; then
    echo "红：保底档四格应等于桩交回的 2/1/3/4（四格顺序或解析错了），实测：$line"; fail=1
  fi
fi
if printf '%s\n' "$BMAIN" | grep -qE '^maintenance +anomaly '; then
  echo "红：无 --full 的普通档跑了 anomaly sweep（维护步只该在保底档跑）"; fail=1
fi
line=$(anomaly_line "$BFULLFAIL")
if [ -z "$line" ]; then
  echo "红：保底档臂②（sweep 失败）里没有 maintenance anomaly 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$line" | grep -q "why=fail_streak"; then
    echo "红：sweep 失败那一发应收在 fail_streak 上，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "contradiction=-1/-1 ghosts=-1/-1"; then
    echo "红：sweep 失败时四格必须是 -1（读不出来不许念成 0），实测：$line"; fail=1
  fi
fi
if ! printf '%s\n' "$BFULLFAIL" | grep -qE '^end used=[0-9]+ fails=1 quarantined=0$'; then
  echo "红：sweep 失败应只记一次 fails（其余各档没失败），实测：$(printf '%s\n' "$BFULLFAIL" | grep -E '^end ' | head -1)"; fail=1
fi
if ! printf '%s\n' "$BFULLFAIL" | grep -E '^episode ' | grep -qE "calls=[1-9]"; then
  echo "红：sweep 失败掐断了下游（episode 那一档没开跑）——维护项失败不许止损整条链"; fail=1
fi

# ---- 备份档（`--full` 的 run_backup，W28 本体）的三条判据 -----------------------------------
# 判据的成文律住在 `daily-drain.sh` 的 `run_backup` 函数头。
# ① full 臂：`hub backup` 恰好一次（标记文件数行）、phase 行六格等于桩读数（core=null 原样）；
# ② 普通臂：一条 maintenance backup 读数都不许有（备份只跟保底档走）；
# ③ 失败臂：why=fail_streak、六格 -1、`end.fails=1`（只记一次）、episode 档照常开跑
#    （备份是维护项，失败不许止损整条链）。
if [ "$backup_calls" != 1 ]; then
  echo "红：备份臂①里 hub backup 应恰好发 1 次，实测 $backup_calls 次"; fail=1
fi
backup_line() { printf '%s\n' "$1" | grep -E '^maintenance +backup ' | head -1; }
line=$(backup_line "$BBACKUP")
if [ -z "$line" ]; then
  echo "红：备份臂①里没有 maintenance backup 的 phase 行（--full 没把备份接上）"; fail=1
else
  if ! printf '%s\n' "$line" | grep -qE "calls=1( |$)"; then
    echo "红：备份档应恰好发 1 次，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -q "why=ok"; then
    echo "红：备份档成功那一发应记 why=ok，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "name=transcript-[0-9]+\.db memory=memory-[0-9]+\.db ledger=ledger-[0-9]+\.db core=None memory_rows=42 extras=2"; then
    echo "红：备份档六格应等于桩交回的 name/memory/ledger/core(null)/42/2（顺序或解析错了），实测：$line"; fail=1
  fi
fi
if printf '%s\n' "$BMAIN" | grep -qE '^maintenance +backup '; then
  echo "红：无 --full 的普通档跑了备份（备份只该在保底档跑）"; fail=1
fi
line=$(backup_line "$BBACKUPFAIL")
if [ -z "$line" ]; then
  echo "红：备份臂②（backup 失败）里没有 maintenance backup 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$line" | grep -q "why=fail_streak"; then
    echo "红：backup 失败那一发应收在 fail_streak 上，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "name=-1 memory=-1 ledger=-1 core=-1 memory_rows=-1 extras=-1"; then
    echo "红：backup 失败时六格必须是 -1（读不出来不许念成 0），实测：$line"; fail=1
  fi
fi
if ! printf '%s\n' "$BBACKUPFAIL" | grep -qE '^end used=[0-9]+ fails=1 quarantined=0$'; then
  echo "红：backup 失败应只记一次 fails（其余各档没失败），实测：$(printf '%s\n' "$BBACKUPFAIL" | grep -E '^end ' | head -1)"; fail=1
fi
if ! printf '%s\n' "$BBACKUPFAIL" | grep -E '^episode ' | grep -qE "calls=[1-9]"; then
  echo "红：backup 失败掐断了下游（episode 那一档没开跑）——备份失败不许止损整条链"; fail=1
fi

# ---- 读数面：预算与九档 caps 那几格必须从 `start`/`phase`/`end` 行里消失（W19b）-------------
# 从原始 JSONL 逐行数键名（`stray_keys`），不看人读面——人读面改了、日志还在写，
# 那才是"从读数面爬回来"的真形状。
for arm in BMAIN BFAIL BPOISON BJSON BLIVE BQUAR BRETRY BREJECT BREJECT3 BFULL BFULLFAIL BBACKUP BBACKUPFAIL; do
  textvar=$arm
  if ! printf '%s\n' "${!textvar}" | grep -q '^stray_keys=0$'; then
    echo "红：$arm 的日志里还有 budget/caps/cap 键（见上面几行 stray:）"; fail=1
  fi
  if ! printf '%s\n' "${!textvar}" | grep -q '^blanks=0$'; then
    echo "红：$arm 的日志里有空行"; fail=1
  fi
done
for arm in BMAIN BJSON BQUAR BRETRY BREJECT BREJECT3 BFULL BFULLFAIL BBACKUP BBACKUPFAIL; do
  textvar=$arm
  if ! printf '%s\n' "${!textvar}" | grep -q '^unparsed=0$'; then
    echo "红：$arm 的日志里有不是合法 JSON 的行"; fail=1
  fi
done

# 失败臂：四族抽取都必须"连失败就收手"（约定 ≤3 次），而不是把整夜烧在一批毒数据上。
fail_lines=$(printf '%s\n' "$BFAIL" | grep -E "^distill +(naming|event|fact|state)")
if [ -z "$fail_lines" ]; then
  echo "红：失败臂里四族一条 phase 行都没有"; fail=1
fi
not_streak=$(printf '%s\n' "$fail_lines" | grep -vc "why=fail_streak"); not_streak=${not_streak:-0}
if [ "$not_streak" != "0" ]; then
  echo "红：失败臂里有族没收手（$not_streak 条的 why 不是 fail_streak）"; fail=1
fi
max_calls=$(printf '%s\n' "$fail_lines" | sed -n 's/.*calls= *\([0-9]*\).*/\1/p' | sort -n | tail -1)
# 失败也要有账：`end` 行的 `fails` 必须 > 0（整夜全失败不许与"无事可做"同形）。
if printf '%s
' "$BFAIL" | grep -qE "^end used=0 .*fails=0"; then
  echo "红：失败臂的 end 行报了 fails=0"; fail=1
fi
if ! printf '%s
' "$BFAIL" | grep -qE "^end used=[0-9]+ fails=[1-9]"; then
  echo "红：失败臂的 end 行没有 fails 那一格"; fail=1
fi
if [ "${max_calls:-99}" -gt 3 ]; then
  echo "红：失败臂最多发了 $max_calls 次才停（约定上限 3）"; fail=1
fi

# 失败臂的 derive 单独一条（与 distill 那几条同一个律）：连失败要收手，而且**失败那一发不许记账**。
# 失败桩的读数照样带着 `rows:8` + `error: llm timeout` + rc 3——只看读数就会把永远失败的那一批
# 读成"还在推进"，这正是 distill 失败臂注释里点过的同一个坑。
derive_fail=$(printf '%s\n' "$BFAIL" | grep -E "^derive " | head -1)
if [ -z "$derive_fail" ]; then
  echo "红：失败臂里没有 derive 的 phase 行"; fail=1
elif ! printf '%s\n' "$derive_fail" | grep -q "why=fail_streak"; then
  echo "红：失败臂的 derive 没收手（why 不是 fail_streak）：$derive_fail"; fail=1
elif ! printf '%s\n' "$derive_fail" | grep -qE "calls=[1-3]( |$)"; then
  echo "红：失败臂的 derive 发的次数越过了约定上限 3：$derive_fail"; fail=1
elif ! printf '%s\n' "$derive_fail" | grep -qE "used=0( |$)"; then
  echo "红：失败臂的 derive 把失败的读数记进了 used（这一族又按行数记账了？）：$derive_fail"; fail=1
fi

# 失败臂的 episode 单独一条（与 derive 那条同一个律，2026-11-04 I-6b）：连失败要收手，
# 且**失败那一发不许记账**——episode 本来就是零调用，used 在失败臂里更不该被它动过。
episode_fail=$(printf '%s\n' "$BFAIL" | grep -E "^episode " | head -1)
if [ -z "$episode_fail" ]; then
  echo "红：失败臂里没有 episode 的 phase 行"; fail=1
elif ! printf '%s\n' "$episode_fail" | grep -q "why=fail_streak"; then
  echo "红：失败臂的 episode 没收手（why 不是 fail_streak）：$episode_fail"; fail=1
elif ! printf '%s\n' "$episode_fail" | grep -qE "calls=[1-3]( |$)"; then
  echo "红：失败臂的 episode 发的次数越过了约定上限 3：$episode_fail"; fail=1
elif ! printf '%s\n' "$episode_fail" | grep -qE "used=0( |$)"; then
  echo "红：失败臂的 episode 把账记进了 used（纯程序零调用）：$episode_fail"; fail=1
fi

# ---- 隔离熔断的两条判据（W19c2c）--------------------------------------------------------
# ① 抽取档：naming 恒交回 `state=quarantined` + `retry_exhausted=1` ⇒ 第 5 发必须收手
#    （calls=5、why=quarantine_streak、quarantined=5）。发数多于 5 就是"墙没立起来"
#    （桩恒回隔离，没有墙会转到天亮），少于 5 就是"提前停了"——两个方向都红。
# ② 审核档：naming 恒交回 `quarantined=1` + `remaining=5` ⇒ 同一格、同一个词（墙的判据
#    换成审核读数面自己的两格）。
# 另断"后面的族照常出现"：停的只能是这一族，不是整夜。
quarantine_wall() { # <label> <text> <phase 行前缀（正则）> <应发的发数>
  local label=$1 text=$2 prefix=$3 want=$4 line
  line=$(printf '%s\n' "$text" | grep -E "$prefix" | head -1)
  if [ -z "$line" ]; then
    echo "红：$label 里没有 $prefix 的 phase 行（这一族一次都没开跑）"; fail=1; return
  fi
  if ! printf '%s\n' "$line" | grep -qE "calls=${want}( |$)"; then
    echo "红：$label 应恰好发 ${want} 次就停在隔离墙上，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -q "why=quarantine_streak"; then
    echo "红：$label 的收手词应是 quarantine_streak，实测：$line"; fail=1
  fi
  if ! printf '%s\n' "$line" | grep -qE "quarantined=${want}( |$)"; then
    echo "红：$label 应数到 ${want} 个隔离，实测：$line"; fail=1
  fi
}
quarantine_wall "隔离熔断臂 抽取" "$BQUAR" '^distill naming ' 5
quarantine_wall "隔离熔断臂 审核" "$BQUAR" '^review +naming ' 5
for want in event fact state; do
  check "隔离熔断臂 后面的族" "$BQUAR" "$want"
done

# ---- 重试不误停（W19c2c）---------------------------------------------------------------
# 桩让 naming 交替 `state=pending` / `state=done`（6 发）再空两发收手：
# 于是 phase 行必须是 calls=8、why=queue_empty、quarantined=0。
# `pending` 被当成失败的话，这一行会落在 fail_streak 上（连失败闸先收），当场红。
# 它也不再被当成"重试耗尽"：脚本只认 hub 读数面的那一格显式信号，`state` 一概不读。
retry_line=$(printf '%s\n' "$BRETRY" | grep -E '^distill naming ' | head -1)
if [ -z "$retry_line" ]; then
  echo "红：重试不误停臂里没有 naming 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$retry_line" | grep -qE "calls=8( |$)"; then
    echo "红：重试不误停臂的 naming 应发 8 次（6 发交替 + 2 发空批）才收手，实测：$retry_line"; fail=1
  fi
  if ! printf '%s\n' "$retry_line" | grep -q "why=queue_empty"; then
    echo "红：重试不误停臂的 naming 应收在队列空上（不是 fail/quarantine 墙），实测：$retry_line"; fail=1
  fi
  if ! printf '%s\n' "$retry_line" | grep -qE "quarantined=0( |$)"; then
    echo "红：重试不误停臂里一个隔离都不该有，实测：$retry_line"; fail=1
  fi
fi
early_stop=$(printf '%s\n' "$BRETRY" | grep -cE "why=(fail_streak|quarantine_streak)")
if [ "${early_stop:-0}" != "0" ]; then
  echo "红：重试不误停臂里有族被墙提前停掉了（$early_stop 条）"; fail=1
fi
# 总账（W19c2c）：`end` 行那一格必须与逐族读数对得上（只追加，前列序不动）。
# 隔离熔断臂的 5 只来自抽取档那 5 发 `retry_exhausted=1`：审核档那 5 条隔离**不进**这一格
# （单位是条不是批），所以这里恰好是 5 而不是 10。
if ! printf '%s\n' "$BRETRY" | grep -qE '^end used=[0-9]+ fails=0 quarantined=0$'; then
  echo "红：重试不误停臂的 end 行应报 fails=0 quarantined=0，实测：$(printf '%s\n' "$BRETRY" | grep -E '^end ' | head -1)"; fail=1
fi
if ! printf '%s\n' "$BQUAR" | grep -qE '^end used=[0-9]+ fails=0 quarantined=5$'; then
  echo "红：隔离熔断臂的 end 行应报 fails=0 quarantined=5（隔离不算失败，审核档的条数也不进这一格），实测：$(printf '%s\n' "$BQUAR" | grep -E '^end ' | head -1)"; fail=1
fi

# ---- 整批拒收是推进（W31）------------------------------------------------------------------
# 判据的成文律住在 `daily-drain.sh` 的 "rc=2 不是墙" 那一段。
# ① 正向：naming 抽 45 发**合法 JSON + rc=2**（state=rejected）之后仍继续，收手在队列空上
#    （calls=47：45 发拒收 + 2 发空批）；`end.fails=0`；原始 JSONL 里无 `"why":"fail_streak"`。
#    旧脚本会在第 3 发就停在 fail_streak 上（calls=3、fails=3）——三处判据各自红在不同格上。
#    naming 审核那两发同样 rc=2：按推进走，收在队列空/停住上（calls=3）。
# ② 阴性对照：同一份 `state:"rejected"` 合法 JSON、退出码换成 3 ⇒ 连失败闸照旧第 3 发收手
#    （calls=3、why=fail_streak、end.fails=3）。红的时候是"判据看的是读数正文而不是退出码"：
#    这一族会跟着 rc=2 那条一起放过，`why` 落在 queue_empty 上。
rej2_line=$(printf '%s\n' "$BREJECT" | grep -E '^distill naming ' | head -1)
if [ -z "$rej2_line" ]; then
  echo "红：rc=2 推进臂里没有 naming 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$rej2_line" | grep -qE "calls=47( |$)"; then
    echo "红：rc=2 推进臂的 naming 应继续到队列空（calls=47：45 发拒收 + 2 发空批），实测：$rej2_line"; fail=1
  fi
  if ! printf '%s\n' "$rej2_line" | grep -q "why=queue_empty"; then
    echo "红：rc=2 推进臂的 naming 应收在队列空上（rc=2 被当成座位故障？），实测：$rej2_line"; fail=1
  fi
  if printf '%s\n' "$rej2_line" | grep -q "why=fail_streak"; then
    echo "红：rc=2 推进臂的 naming 被连失败闸停掉了（rc=2 仍记 bad）：$rej2_line"; fail=1
  fi
fi
if ! printf '%s\n' "$BREJECT" | grep -qE '^end used=[0-9]+ fails=0 quarantined=0$'; then
  echo "红：rc=2 推进臂的 end 行应报 fails=0 quarantined=0（拒收不是失败），实测：$(printf '%s\n' "$BREJECT" | grep -E '^end ' | head -1)"; fail=1
fi
if [ "${reject_streak:-0}" != "0" ]; then
  echo "红：rc=2 推进臂的日志里有 $reject_streak 行 why=fail_streak（rc=2 被记成失败）"; fail=1
fi
rejR_line=$(printf '%s\n' "$BREJECT" | grep -E '^review +naming ' | head -1)
if [ -z "$rejR_line" ]; then
  echo "红：rc=2 推进臂里没有 review naming 的 phase 行"; fail=1
elif ! printf '%s\n' "$rejR_line" | grep -q "why=queue_empty_or_stalled"; then
  echo "红：rc=2 推进臂的 review naming 应收在队列空/停住上（审核档的 rc=2 应算推进），实测：$rejR_line"; fail=1
fi
rej3_line=$(printf '%s\n' "$BREJECT3" | grep -E '^distill naming ' | head -1)
if [ -z "$rej3_line" ]; then
  echo "红：rc=3 阴性对照臂里没有 naming 的 phase 行"; fail=1
else
  if ! printf '%s\n' "$rej3_line" | grep -q "why=fail_streak"; then
    echo "红：rc=3 即使读数写着 state=rejected 也应按连失败收手，实测：$rej3_line"; fail=1
  fi
  if ! printf '%s\n' "$rej3_line" | grep -qE "calls=3( |$)"; then
    echo "红：rc=3 阴性对照臂的 naming 应在第 3 发收手（FAIL_STREAK=3），实测：$rej3_line"; fail=1
  fi
fi
if ! printf '%s\n' "$BREJECT3" | grep -qE '^end used=[0-9]+ fails=3 quarantined=0$'; then
  echo "红：rc=3 阴性对照臂的 end 行应报 fails=3（每发都记一次失败），实测：$(printf '%s\n' "$BREJECT3" | grep -E '^end ' | head -1)"; fail=1
fi

# ---- 分诊三格（derive 的 `rej_*_total`）的三条判据（2026-11-04 I-2c）----------------------
# 这三条钉的是"读不出"与"记过账"这两种 -1 的关系，与上面各条各管一事：
#   derive_check 管这一族被计量过，这三条管**分诊读数会不会说谎**。
# 判据的成文律住在 `daily-drain.sh` 的分诊累计规则那一段：任何一发读不出来 ⇒ 三格一起标 -1
# 并**永久停在** -1；之后的读数不许把它复活成"从坏那一发之后起算的部分和"。
derive_rej() { # <derive 的 phase 行> -> 打出 `rej=` 那一格（没有就打空）
  printf '%s\n' "$1" | sed -n 's/.* rej=\([^ ]*\).*/\1/p'; }
derive_line() { printf '%s\n' "$1" | grep -E '^derive ' | head -1; }

# 判据 5（M-1）：一发读不出 + 之后三发可读 ⇒ 三格必须是 -1/-1/-1。
# 这条红的形状就是"部分和"：坏一发之后那三发的 5/5/5 被加成 15/15/15，
# 而第 1 发真实的 2/1/0 被丢掉——它与总账、与"今天有没有读不出的一发"都没有可核对关系。
# 序列前提：spec 的 5 行都跑到 + 2 发收手 = calls=7（旧版靠额度恰好切在第 5 发，现在由桩自己收敛）。
poison_check() { # <label> <text>
  local label=$1 text=$2 line rej
  line=$(derive_line "$text")
  if [ -z "$line" ]; then echo "红：$label 里没有 derive 的 phase 行"; fail=1; return; fi
  # 先钉住这一臂的前提：spec 那五发都开了跑、并且是跑到空批才收手——不然"之后还有可读的发"
  # 根本没发生，那三格是 -1 也只是蒙对的（本仓"惰性守卫"那一族点过名）。
  if ! printf '%s\n' "$line" | grep -qE "calls=7( |$)"; then
    echo "红：$label 的 derive 没跑满 spec 的 5 发 + 2 发收手，序列前提没成立（判据无从谈起）：$line"; fail=1
  fi
  if printf '%s\n' "$line" | grep -q "why=fail_streak"; then
    echo "红：$label 的 derive 收在 fail_streak（序列桩的 rows 读不出来，与分诊无关的坏）：$line"; fail=1
  fi
  rej=$(derive_rej "$line")
  if [ "$rej" != "-1/-1/-1" ]; then
    echo "红：$label 有一发分诊读数读不出、其后还有可读的发 ⇒ 三格必须停在 -1，实测 rej=$rej（复活成部分和）"; fail=1
  fi
}
poison_check "分诊臂①" "$BPOISON"

# 判据 6（M-2）：`parse_derive_rejects` 交回空串时，日志**每一行**仍必须是合法 JSON。
# 红的时候那一行长这样（`rej_no_mechanism_total` 后面什么都没有）：
#   {"t":"phase","pass":"derive",…,"rej_no_mechanism_total":,"rej_no_evidence_total":,…}
# ⇒ 整行不可解析，破的是 `log()` 之后"一行一个合法 JSON"这条形状。同一轮其余行全部解析通过，
# 这就是阳性对照：读者有效，坏的只有这一行。
json_check() { # <label> <text>
  local label=$1 text=$2 n rej line
  n=$(printf '%s\n' "$text" | sed -n 's/^unparsed=\([0-9]*\)$/\1/p'); n=${n:-读不到}
  if [ "$n" != "0" ]; then
    echo "红：$label 的日志里有 $n 行不是合法 JSON（见上面那几行 unparsed: 原文）"; fail=1
  fi
  line=$(derive_line "$text")
  if [ -z "$line" ]; then
    echo "红：$label 里 derive 的 phase 行压根解析不出来（就是那一行非法）"; fail=1; return
  fi
  rej=$(derive_rej "$line")
  if [ "$rej" != "-1/-1/-1" ]; then
    echo "红：$label 解析器交回空串时三格该并进 -1 那一态，实测 rej=$rej"; fail=1
  fi
}
json_check "分诊臂②" "$BJSON"

# 判据 7（T-3 阳性对照）：整轮全部可读 ⇒ 三格必须是**桩交回值之和**，且非零。
# 这条是给新加的状态位（poison 位）设的反证：如果它被写成恒 -1 的惰性守卫，判据 5 与这条不可能同时绿。
# 期望值直接从桩读的那张表算出来（awk 求和），不在这里手写常数——手写常数就退化成"断言桩"。
live_check() { # <label> <text> <spec 文件>
  local label=$1 text=$2 spec=$3 line rej want
  line=$(derive_line "$text")
  if [ -z "$line" ]; then echo "红：$label 里没有 derive 的 phase 行"; fail=1; return; fi
  want=$(awk '{a += $1; b += $2; c += $3} END {printf "%d/%d/%d", a, b, c}' "$spec")
  rej=$(derive_rej "$line")
  if [ "$rej" = "-1/-1/-1" ]; then
    echo "红：$label 整轮全部可读却交回 -1（分诊守卫是惰性的？）：$line"; fail=1
  elif [ "$rej" != "$want" ]; then
    echo "红：$label 三格之和应当**恰好等于**桩交回的各发之和：期望 rej=$want，实测 rej=$rej"; fail=1
  fi
  if [ "$rej" = "0/0/0" ]; then echo "红：$label 三格全是 0（补 0 就是把'少算'念成'没拒过'）：$line"; fail=1; fi
}
live_check "分诊臂③" "$BLIVE" "$STUB_DIR/spec-live"

# ---- 观测面聚合的臂（2026-11-07 W33 评审：缺陷① ops 行遮蔽 + 建议② 无测试钉）------------
# `parse_call_obs` 曾是本闸里唯一没有桩臂的解析器（评审原话："这两格是唯一裸奔的"）。
# 单元臂直接从日脚本里原文提取函数定义（awk 摘出 `parse_*(){…}`、eval 装进本 shell）——
# 不抄函数体，抄了就是第二个真相；再一条**端到端臂**用 ops 桩走完整轮，
# 钉的是 run_distill / run_review / run_derive 三档真的把聚合接进了 end 行
# （缺陷②：review/derive 两档曾根本不聚合，闸门对它们失明）。
# 同根扩展（端到端臂实证）：ops 行遮蔽下 `parse_turns` 会读出 -1，而 run_distill 在 rc=0 时
# 无条件清 `bad` ⇒ 连失败闸永远凑不满、**这一族原地空转到天亮**——所以 `parse_turns` /
# `parse_exhausted` 与 `parse_call_obs` 一起改为扫全行，并有各自的遮蔽单元臂。
echo "== 观测臂①（ops 行遮蔽）：首行非 JSON 的 ops 行 + 次行带键 ⇒ 两格照读，不抹零 =="
eval "$(awk '/^parse_call_obs\(\)/{f=1} f{print} f&&/; \} *$/{exit}' "$HERE/daily-drain.sh")"
eval "$(awk '/^parse_turns\(\)/{f=1} f{print} f&&/; \} *$/{exit}' "$HERE/daily-drain.sh")"
eval "$(awk '/^parse_exhausted\(\)/{f=1} f{print} f&&/; \} *$/{exit}' "$HERE/daily-drain.sh")"
obs=$(parse_call_obs '{"ops":"self-heal retry","attempt":2}
{"pass":"naming","max_call_ms":1234,"timeouts":1}')
if [ "$obs" != "1234 1" ]; then
  echo "红：观测臂① 首行被 ops 行遮蔽时两格被抹零（缺陷①复发？），实测 '$obs'"; fail=1
fi
echo "== 观测臂①b（同根·turns/exhausted 遮蔽）：首行 [llm ...] ops 行 ⇒ turns/exhausted 照读 =="
obs=$(parse_turns '[llm dev-x 200 412ms self-heal: identical resend] {"choices":[]}
{"pass":"naming","turns":8,"state":"pending"}')
[ "$obs" = "8" ] || { echo "红：观测臂①b turns 被遮蔽读成 '$obs'（旧版读 -1 ⇒ 这一族永不收敛）"; fail=1; }
obs=$(parse_exhausted '[llm dev-x 200 412ms self-heal: identical resend] {"choices":[]}
{"pass":"naming","turns":1,"retry_exhausted":1}')
[ "$obs" = "1" ] || { echo "红：观测臂①b exhausted 被遮蔽读成 '$obs'"; fail=1; }
obs=$(parse_turns 'garbage')
[ "$obs" = "-1" ] || { echo "红：观测臂①b 无键 garbage 支实测 '$obs'（该是 -1）"; fail=1; }
obs=$(parse_exhausted '{"turns":8}')
[ "$obs" = "0" ] || { echo "红：观测臂①b exhausted 无键支实测 '$obs'（该是 0）"; fail=1; }
echo "== 观测臂②（多行聚合）：多行都带键 ⇒ max 取最慢、timeouts 求和，无键/垃圾行不贡献 =="
obs=$(parse_call_obs '{"max_call_ms":100,"timeouts":1}
{"max_call_ms":300,"timeouts":2}
{"max_call_ms":50}
error: not json')
if [ "$obs" != "300 3" ]; then
  echo "红：观测臂② 跨行聚合错（期望 max=300 sum=3），实测 '$obs'"; fail=1
fi
echo "== 观测臂③（缺键贡献 0）：garbage / 无键 JSON / 空串 ⇒ 0 0，不发明 -1 =="
obs=$(parse_call_obs 'garbage line')
[ "$obs" = "0 0" ] || { echo "红：观测臂③ garbage 支实测 '$obs'"; fail=1; }
obs=$(parse_call_obs '{"turns":8}')
[ "$obs" = "0 0" ] || { echo "红：观测臂③ 无键支实测 '$obs'"; fail=1; }
obs=$(parse_call_obs '')
[ "$obs" = "0 0" ] || { echo "红：观测臂③ 空串支实测 '$obs'"; fail=1; }

# 端到端臂的桩：distill 的读数**先打一行 stderr ops 行**（2>&1 之下它是首行——形状照抄
# `rc-llm-http/src/lib.rs` 的 `ops_line`：`[llm {label} {status} {slow_ms}ms {category}] {body}`，
# 非 JSON 且不带读数键；旧版"只看第一行"的解析器在这一形状上把 `parse_call_obs` 整发抹零、
# 把 `parse_turns` 读成 -1 而这一族永不收敛，缺陷①的复现形状），再打带
# `max_call_ms`/`timeouts` 键的合法读数。review/derive 档的读数干净且带键——它们的解析器
# （`find("{")` / `parse_rows` 族）不在本工单的修复范围，遮蔽下的行为另行处理；
# episode 与 time 恒不带键（纯程序不开 LLM 口，缺键贡献 0 的真实形状）。
# 收敛序列与主桩同形（各档收在自己的队列空/游标不动上）。
# 三个档的键值互不相同且各档内逐发可辨：distill 第 n 发交 n×1000（max 由第 4 发的 4000 决定）、
# time-outs 各 1；review 恒 100/2；derive 恒 999/3（单批一行，timeouts 即批增量）。
# ⇒ end 行期望 timeouts = 24（distill 6 族×4 发×1）+ 30（review 5 pass×3 发×2）
#   + 15（derive 5 发×3）= 69，max_call_ms = 4000。
cat > "$STUB_DIR/hub-obs" <<'OBS'
#!/bin/bash
pass=fam=; sub=
args=("$@")
for ((i=0;i<${#args[@]};i++)); do
  case "${args[i]}" in
    distill) sub=distill ;;
    review) sub=review ;;
    derive) sub=derive ;;
    episode) sub=episode ;;
    --pass) pass=${args[i+1]} ;;
    --family) fam=${args[i+1]} ;;
  esac
done
case "$sub" in
  distill)
    printf '[llm dev-x 200 412ms self-heal: identical resend] {"choices":[{"text":"..."}]}\n' >&2
    if [ "$pass" = time ]; then
      n=$(cat "$STUB_COUNT_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_COUNT_FILE"
      [ "$n" -ge 3 ] && turns=0 || turns=8
      printf '{"pass":"%s","turns":%s,"state":"pending"}\n' "$pass" "$turns"
    else
      f="$STUB_DISTILL_DIR/${pass}-${fam:-all}"
      n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
      [ "$n" -le 2 ] && turns=1 || turns=0
      printf '{"schema_version":2,"pass":"%s","family":"%s","turns":%s,"state":"pending","retry_exhausted":0,"max_call_ms":%s,"timeouts":1}\n' "$pass" "$fam" "$turns" "$((n*1000))"
    fi
    ;;
  review)
    f="$STUB_REVIEW_DIR/$pass"
    n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$f"
    [ "$n" -le 2 ] && rem=1 || rem=0
    printf '{"processed":1,"applied":1,"rejected":0,"quarantined":0,"skipped":0,"conflicts":0,"errors":0,"exhausted":false,"remaining_pending_review":%s,"remaining_quarantined":0,"remaining_approved":0,"max_call_ms":100,"timeouts":2}\n' "$rem"
    ;;
  derive)
    n=$(cat "$STUB_DERIVE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_DERIVE_FILE"
    if [ "$n" -le 3 ]; then rows=8; moved=true; else rows=0; moved=false; fi
    printf '{"schema_version":1,"job":"derive","batch":1,"rows":%s,"rejected":0,"dropped":0,"quarantined":0,"issue_id":null,"cursor_moved":%s,"lag_rows":1,"max_call_ms":999,"timeouts":3}\n' "$rows" "$moved"
    ;;
  episode)
    n=$(cat "$STUB_EPISODE_FILE" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$STUB_EPISODE_FILE"
    if [ "$n" -ge 3 ]; then ew=0; moved=false; else ew=4; moved=true; fi
    printf '{"schema_version":1,"job":"episode","batch":1,"candidates":12,"merged_pairs":3,"span_pruned":1,"episodes_written":%s,"members_linked":9,"cards_written":%s,"superseded":0,"dropped_members":0,"mid_band_deferred":2,"cursor_moved":%s,"lag_rows":1}\n' "$ew" "$ew" "$moved"
    ;;
  *) printf '{}\n' ;;
esac
exit 0
OBS
chmod +x "$STUB_DIR/hub-obs"
echo "== 观测臂④（端到端）：ops 桩走完整轮 ⇒ end 行两格 = distill/review/derive 三档的跨行聚合 =="
BOBS=$(run_once "$STUB_DIR/hub-obs"); printf '%s\n' "$BOBS"
obs_end=$(python3 - "$WORK/logs/daily-test.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    r = json.loads(line)
    if "end" in r:
        print("%s %s" % (r.get("max_call_ms"), r.get("timeouts")))
PY
)
if [ "$obs_end" != "4000 69" ]; then
  echo "红：观测臂④ end 行两格应当是 '4000 69'（timeouts=69：distill 24 + review 30 + derive 15；max 由 distill 第 4 发的 4000 决定），实测 '$obs_end'"; fail=1
fi

# ---- 水位闸（`drain-if-pending.sh`，W25）--------------------------------------------------
# 闸的消费者是每 10 分钟一次的定时器，它只做一件事：`hub pending` 五个计数有正的 ⇒ 跑日脚本，
# 否则零输出退出 0。三条判据，第三条比前两条重要：
#   ① 无待办 ⇒ 不调日脚本、零输出、退出 0（10 分钟一次若每次都留一行，触发行就被噪声淹了）；
#   ② 有待办 ⇒ 调日脚本，并把五个计数原样落一行 `{"t":"trigger","pending":{…}}`；
#   ③ **读不出来 ⇒ 退出 3**（`hub pending` 非零，或读数不是合法 JSON）。把"读不出来"念成
#      "今天没事做"是这道闸最坏的失败形状：整夜一次都不跑，日志里只有一个 `exit 0`。
# 闸按 `"$(dirname "$0")/daily-drain.sh"` 找日脚本，所以这里把闸**原样拷进**临时目录、旁边放一个
# 只写标记的 `daily-drain.sh` 桩：被验证的是仓库里那一行调用，不靠任何环境变量开后门。
GATE_DIR="$STUB_DIR/gate"
mkdir -p "$GATE_DIR"
cp "$HERE/drain-if-pending.sh" "$GATE_DIR/drain-if-pending.sh"
cat > "$GATE_DIR/daily-drain.sh" <<'GATEDRAIN'
#!/bin/bash
printf 'called\n' >> "$GATE_MARKER"
exit 0
GATEDRAIN
chmod +x "$GATE_DIR/daily-drain.sh"

# 三个 `pending` 桩：空表 / 有三件待办 / 读不出来。前两个的键面与 `hub pending --json` 逐字同形
# （多一个键或少一个键，闸的解析都不该被蒙过去——解析读的是那五格的名字）。
cat > "$STUB_DIR/hub-idle" <<'IDLE'
#!/bin/bash
if [ "${1:-}" = pending ]; then
  printf '{"extract":{"pending_passes":0,"running_passes":1,"lag_turns":0,"scopes_with_lag":0},"review":{"pending":0,"quarantined":0},"derive":{"lag_rows":0,"cursor_at_ms":0,"last_success_at_ms":null},"pass":{"last_success_at_ms":null}}\n'
  exit 0
fi
printf '{}\n'; exit 0
IDLE
cat > "$STUB_DIR/hub-busy" <<'BUSY'
#!/bin/bash
if [ "${1:-}" = pending ]; then
  printf '{"extract":{"pending_passes":0,"running_passes":0,"lag_turns":2,"scopes_with_lag":1},"review":{"pending":1,"quarantined":0},"derive":{"lag_rows":0,"cursor_at_ms":0,"last_success_at_ms":null},"pass":{"last_success_at_ms":null}}\n'
  exit 0
fi
printf '{}\n'; exit 0
BUSY
# 读不出来的两种形状：非零退出，与"退出 0 但交回的不是 JSON"。
cat > "$STUB_DIR/hub-broken" <<'BROKEN'
#!/bin/bash
printf 'error: store unreachable — run: doctor\n' >&2
exit 3
BROKEN
cat > "$STUB_DIR/hub-garbage" <<'GARBAGE'
#!/bin/bash
printf 'Python was not found; run without arguments to install from the Microsoft Store\n'
exit 0
GARBAGE
chmod +x "$STUB_DIR/hub-idle" "$STUB_DIR/hub-busy" "$STUB_DIR/hub-broken" "$STUB_DIR/hub-garbage"

gate_run() { # <hub 桩> <标记文件> <日志文件> -> 退出码（stdout/stderr 落 $STUB_DIR/gate-*）
  local hub=$1 mark=$2 logf=$3
  rm -f "$mark" "$logf"
  GATE_MARKER="$mark" RECOLLECT_DATA_DIR="$WORK" RECOLLECT_HUB="$hub" RECOLLECT_LOG="$logf" \
    bash "$GATE_DIR/drain-if-pending.sh" > "$STUB_DIR/gate-out" 2> "$STUB_DIR/gate-err"
}

echo "== 水位闸①：无待办（running 不算待办）⇒ 零输出、退出 0、不调日脚本 =="
gate_run "$STUB_DIR/hub-idle" "$STUB_DIR/gate-mark-idle" "$STUB_DIR/gate-idle.jsonl"; grc=$?
gout=$(cat "$STUB_DIR/gate-out")
if [ "$grc" != 0 ]; then
  echo "红：无待办时闸应退出 0，实测 $grc（stderr：$(cat "$STUB_DIR/gate-err")）"; fail=1
fi
if [ -n "$gout" ]; then echo "红：无待办时闸应零输出，实测：$gout"; fail=1; fi
if [ -e "$STUB_DIR/gate-mark-idle" ]; then echo "红：无待办时闸调了日脚本"; fail=1; fi
if [ -e "$STUB_DIR/gate-idle.jsonl" ]; then echo "红：无待办时闸写了日志"; fail=1; fi

echo "== 水位闸②：有待办（lag_turns=2 + review_pending=1）⇒ 调日脚本 + 一行 trigger =="
gate_run "$STUB_DIR/hub-busy" "$STUB_DIR/gate-mark-busy" "$STUB_DIR/gate-busy.jsonl"; grc=$?
if [ "$grc" != 0 ]; then
  echo "红：有待办时闸应退出 0（日脚本桩成功），实测 $grc（stderr：$(cat "$STUB_DIR/gate-err")）"; fail=1
fi
if [ ! -e "$STUB_DIR/gate-mark-busy" ]; then echo "红：有待办时闸没有调日脚本"; fail=1; fi
gate_log=$(python3 - "$STUB_DIR/gate-busy.jsonl" <<'PY'
import json, sys
try:
    lines = [l for l in open(sys.argv[1], encoding="utf-8").read().splitlines() if l.strip()]
except Exception:
    print("no-log"); raise SystemExit
if len(lines) != 1:
    print("lines=%d" % len(lines)); raise SystemExit
try:
    r = json.loads(lines[0])
except Exception:
    print("bad-json"); raise SystemExit
want = {"pending_passes": 0, "lag_turns": 2, "review_pending": 1,
        "quarantined": 0, "derive_lag_rows": 0}
got = r.get("pending") or {}
print("ok" if r.get("t") == "trigger" and got == want else "shape=%s" % (r,))
PY
)
if [ "$gate_log" != "ok" ]; then
  echo "红：有待办时闸应落恰好一行 {\"t\":\"trigger\",\"pending\":{五格}}，实测：$gate_log"; fail=1
fi

echo "== 水位闸③：读不出来（hub 非零 / 交回非 JSON）⇒ 退出 3，不调日脚本、不当成没待办 =="
for arm in broken garbage; do
  gate_run "$STUB_DIR/hub-$arm" "$STUB_DIR/gate-mark-$arm" "$STUB_DIR/gate-$arm.jsonl"; grc=$?
  if [ "$grc" != 3 ]; then
    echo "红：$arm 臂里闸应退出 3（读不出来 ≠ 没待办），实测 $grc"; fail=1
  fi
  if [ -e "$STUB_DIR/gate-mark-$arm" ]; then echo "红：$arm 臂里闸调了日脚本（读不出来时不许开跑）"; fail=1; fi
  if [ -e "$STUB_DIR/gate-$arm.jsonl" ]; then echo "红：$arm 臂里闸写了 trigger 行（它没读到任何待办）"; fail=1; fi
done

if [ "$fail" = 0 ]; then
  echo "排期闸：通过（naming 连发 45 次仍在跑、各档收在自己的队列空/连失败上、连续 5 个隔离就停这一族而 pending 不停族、start/phase/end 无 budget/caps/cap、derive 与 episode 的读数被计量、episode 档 used 恒等 end 行、分诊三格的 -1 律与 JSON 形状都守住）"
fi
exit $fail
