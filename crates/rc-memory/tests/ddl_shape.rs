use rc_memory::keys::{AnchorRole, MemKey, MemTable, ScopeKind, Stance, Verdict};
use rc_memory::keys::PROVENANCE_ROLES;
use rc_memory::keys::REF_KINDS;

/// 所有枚举的变体清单：往返断言与 CHECK 对拍共用这一份，两处不许各写一遍。
const MEM_TABLES: &[MemTable] = &[MemTable::Anchor, MemTable::Card];
const SCOPE_KINDS: &[ScopeKind] = &[ScopeKind::Person, ScopeKind::Project, ScopeKind::Entity];
const ANCHOR_ROLES: &[AnchorRole] = &[AnchorRole::Human, AnchorRole::Agent, AnchorRole::System];
const VERDICTS: &[Verdict] = &[Verdict::Unreviewed, Verdict::Passed, Verdict::Low, Verdict::Failed];
const STANCES: &[Stance] = &[Stance::Confirmed, Stance::Corroborated, Stance::Rejected, Stance::Retired];

/// 排序后的字面量集合：集合比较不看顺序，DDL 里的书写顺序因此不是隐式契约。
fn sorted(texts: impl IntoIterator<Item = impl std::fmt::Display>) -> Vec<String> {
    let mut v: Vec<String> = texts.into_iter().map(|t| t.to_string()).collect();
    v.sort();
    v
}

/// 取 `table` 的建表原文，抓出 `CHECK(<column> IN (...))` 那一条里的引号字面量（排序后）。
/// 找不到就 panic：那一列的 CHECK 被删、被改形、换了列名，本身就是要红的事，
/// 绝不静默返回空集——静默空集等于把这条守卫降级成"只查 attestation"。
fn check_literals(root: &rc_store::Root, table: &str, column: &str) -> Vec<String> {
    let text: String = root.memory().conn().query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [table], |r| r.get(0)).unwrap();
    // DDL 里 CHECK 的换行位置不固定（"IN" 后可能直接换行再接括号），所以锚到 "IN"，
    // 再越过它后面的第一个 "("——守卫仍要求这一列必须有 IN (...) 形态，只是不假设空白。
    let pat = format!("CHECK({column} IN");
    let at = text.find(&pat)
        .unwrap_or_else(|| panic!("{table}.{column} 没有 CHECK({column} IN (...)) 形态的约束，对拍无法执法"))
        + pat.len();
    let at = at + text[at..].find('(')
        .unwrap_or_else(|| panic!("{table}.{column} 的 CHECK 没有字面量列表")) + 1;
    let rest = &text[at..];
    let end = rest.find(')').unwrap_or_else(|| panic!("{table}.{column} 的 CHECK 字面量列表没有闭合"));
    let mut got: Vec<String> = rest[..end].split('\'').skip(1).step_by(2).map(String::from).collect();
    got.sort();
    got
}

/// 取 `provenance` 建表原文里 **role 那一列的定义片段**（到同一段里的 `PRIMARY KEY` 为止，
/// 表级约束之后没有本列的东西）。
/// 锚点要排掉 `anchor_role`——它的列名尾巴与 `role` 同名，直接 `find("role TEXT")` 会先命中它。
/// 找不到锚点就 panic：`role` 那一格被删、改名或换了形状，本身就是要红的事。
fn role_column_def(root: &rc_store::Root) -> String {
    let text: String = root.memory().conn().query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        ["provenance"], |r| r.get(0)).unwrap();
    let at = text.match_indices("role TEXT")
        .find(|(i, _)| !text[..*i].ends_with("anchor_"))
        .map(|i| i.0)
        .unwrap_or_else(|| panic!("provenance 的建表原文里没有 role TEXT 那一格，对拍无法执法：{text}"));
    let rest = &text[at..];
    // 迁移库的原文里表级 PRIMARY KEY 在补列之前（ALTER 把新列接在收尾括号前），
    // 所以这里"找得到就截到它、找不到就取到末尾"，两条路都只覆盖 role 那一格。
    let end = rest.find("PRIMARY KEY").unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn handle_strings_are_the_only_cross_module_shape() {
    // 句柄的文本形状只有一个来源：任务 3 之后的 §6 内容寻址 id 是 64 hex，
    // key_string 的分隔符是 ':'。宿主 agent 把它写进自己的笔记，跨会话再指认——
    // 这个格式因此不能随表名重构而漂。
    let k = MemKey { table: MemTable::Card, id: "a".repeat(64) };
    assert_eq!(k.key_string(), format!("card:{}", "a".repeat(64)));
    assert_eq!(MemTable::parse("anchor"), Some(MemTable::Anchor));
    assert_eq!(MemTable::parse("card"), Some(MemTable::Card));
    assert_eq!(MemTable::parse("assertion"), None);
    assert_eq!(MemTable::parse("CARDS"), None);
    assert_eq!(AnchorRole::Human.as_str(), "human");
    assert_eq!(Stance::Corroborated.as_str(), "corroborated");
    assert_eq!(ScopeKind::Project.as_str(), "project");
    // Ord 必须按 key_string，不是字段序：同分次序的唯一保证
    let a = MemKey { table: MemTable::Card, id: "b".repeat(64) };
    let b = MemKey { table: MemTable::Card, id: "a".repeat(64) };
    assert!(b < a);
    // 跨表比较按 prefix 的字典序（anchor < card）：Ord 仍走 key_string，
    // 而不是 variant 的声明序。
    let e = MemKey { table: MemTable::Anchor, id: "a".repeat(64) };
    let s = MemKey { table: MemTable::Card, id: "a".repeat(64) };
    assert!(e < s);
}

#[test]
fn every_enum_slug_round_trips_through_parse() {
    // Rust 侧的自洽：每个变体的 slug 必须 parse 回它自己，一个不是 slug 的串必须 parse 成 None。
    // MemTable 的 slug 是 prefix()（句柄形状），其余四个是 as_str()。
    // Verdict 在这里第一次有牙：按裁定 verdict 列不加 SQL CHECK（写侧由任务 3 的类型化
    // set_verdict 把守），所以 as_str()/parse() 的这趟往返是它唯一的守卫。
    for t in MEM_TABLES {
        assert_eq!(MemTable::parse(t.prefix()), Some(*t), "{} 解析不回 {:?}", t.prefix(), t);
    }
    assert_eq!(MemTable::parse("不是任何 slug 的串"), None, "MemTable 收下了不该收的串");
    for k in SCOPE_KINDS {
        assert_eq!(ScopeKind::parse(k.as_str()), Some(*k), "{} 解析不回 {:?}", k.as_str(), k);
    }
    assert_eq!(ScopeKind::parse("不是任何 slug 的串"), None, "ScopeKind 收下了不该收的串");
    for r in ANCHOR_ROLES {
        assert_eq!(AnchorRole::parse(r.as_str()), Some(*r), "{} 解析不回 {:?}", r.as_str(), r);
    }
    assert_eq!(AnchorRole::parse("不是任何 slug 的串"), None, "AnchorRole 收下了不该收的串");
    for v in VERDICTS {
        assert_eq!(Verdict::parse(v.as_str()), Some(*v), "{} 解析不回 {:?}", v.as_str(), v);
    }
    assert_eq!(Verdict::parse("不是任何 slug 的串"), None, "Verdict 收下了不该收的串");
    for s in STANCES {
        assert_eq!(Stance::parse(s.as_str()), Some(*s), "{} 解析不回 {:?}", s.as_str(), s);
    }
    assert_eq!(Stance::parse("不是任何 slug 的串"), None, "Stance 收下了不该收的串");

    // 裁定 R16：`key_string` 的文本形状只有一个家——逆函数 `MemKey::parse` 也住在 keys.rs。
    // 逐变体往返：每个表用它的 prefix 拼句柄，parse 必须原样拆回同一个 MemKey。
    for t in MEM_TABLES {
        let k = MemKey { table: *t, id: "a".repeat(64) };
        assert_eq!(MemKey::parse(&k.key_string()), Some(k), "{} 句柄往返不回 {:?}", t.prefix(), t);
    }
    // 冒号只按【第一个】分隔点切：id 里的其余冒号原样留着（正文可含冒号的判据）。
    assert_eq!(
        MemKey::parse("card:a:b"),
        Some(MemKey { table: MemTable::Card, id: "a:b".to_string() }),
        "id 里的冒号不该被当成第二个分隔点"
    );
    assert_eq!(MemKey::parse("nope:x"), None, "认不出的表名前缀必须 None");
    assert_eq!(MemKey::parse("card"), None, "没有冒号分隔点必须 None");
    assert_eq!(MemKey::parse(":x"), None, "空表名前缀必须 None");
}

#[test]
fn rust_enums_and_sql_checks_agree_on_legal_values() {
    // CHECK 约束与 Rust 枚举各写一遍就是下一次"库里存得下、代码读不出"的来源。
    // 三处 CHECK 逐个对拍（stance / actor / anchor_role）：旧写法把入参 sql 丢掉、
    // 硬编码 name='attestation'，于是 provenance.anchor_role 与 attestation.actor
    // 这两处 CHECK 从来没被任何用例对拍过。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();

    // 正向：枚举里每个 slug 都必须在对应列的 CHECK 里，CHECK 也不许多放行一个。
    assert_eq!(check_literals(&root, "attestation", "stance"),
               sorted(STANCES.iter().map(|s| s.as_str())),
               "attestation.stance 的 CHECK 与 Stance 不一致");
    assert_eq!(check_literals(&root, "provenance", "anchor_role"),
               sorted(ANCHOR_ROLES.iter().map(|a| a.as_str())),
               "provenance.anchor_role 的 CHECK 与 AnchorRole 不一致");
    // actor 这一列没有对应的 slug 枚举（写票的只有 Center/Agent，schema.rs 的 Actor 另有余值），
    // 所以钉死字面量集合：往 CHECK 里多塞一个值、或改掉一个值，都当场红。
    assert_eq!(check_literals(&root, "attestation", "actor"),
               sorted(["agent", "center"]),
               "attestation.actor 的 CHECK 不再是 agent|center");
    // 相等 = 两个方向都管：CHECK 少一个枚举值红（库里存不下代码写得出的值），
    // 多放行一个枚举读不出的字面量也红（库里存得下、代码读不出）。
}

#[test]
fn ref_kinds_match_the_sql_check_on_provenance() {
    // 裁定 2：ref_kind 是 Rust 侧的 `REF_KINDS` 常量 ↔ SQL CHECK 字面量集合，二者必须对拍。
    // CHECK 多一个字面量 = 库里存得下、代码读不出；少一个字面量 = 库里存不下、代码写得出的值。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let got = check_literals(&root, "provenance", "ref_kind");
    let want = sorted(REF_KINDS.iter().copied());
    assert_eq!(got, want, "provenance.ref_kind 的 CHECK 与 REF_KINDS 不一致");
}

#[test]
fn roles_match_the_sql_check_on_provenance() {
    // 批次③ §3 步 4：`PROVENANCE_ROLES` ↔ `provenance.role` 那一格的字面量集合，二者必须对拍。
    // 形状与 ref_kind 那条不同：role 的约束是跨列 CHECK `(role='link')=(ord IS NOT NULL)`，
    // 不是 `CHECK(role IN (...))`，所以 `check_literals` 那个助手在这里咬不到（它按
    // `CHECK(<列> IN (` 找），字面量因此从"那一列的定义原文"里取（DEFAULT 那一个也在其中）。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let def = role_column_def(&root);
    assert_eq!(sorted(def.split('\'').skip(1).step_by(2).map(String::from)),
               sorted(PROVENANCE_ROLES.iter().copied()),
               "provenance.role 那一格的字面量与 PROVENANCE_ROLES 不一致：{def}");
    // 第二条防的不是封闭枚举（实测：把那一格改成 `CHECK(role IN ('basis','link'))` 时 DEFAULT 的
    // 'basis' 与 IN 里那个重复，字面量数成三个 ⇒ 上面第一条当场红，轮不到这一条），
    // 而是"字面量集合照旧、role↔ord 的耦合被换掉"那一形——把等号写成蕴含 `<=` 时
    // `(role='link')<=(ord IS NOT NULL)` 仍含 `=(ord IS NOT NULL)` 这个短串，只看短串会放过它。
    // 所以这里钉整段原文（fresh 库的 sqlite_master 按 CREATE 原样存，与 ddl.rs 那一格逐字相同）。
    // 行为面的同一件事（依据带上序号就真的落得进库）由 rc-store 的 phase3_ddl 用 INSERT 钉。
    assert!(def.contains("CHECK((role='link')=(ord IS NOT NULL))"),
            "provenance.role 那一格不再是批次③ 钉下的跨列 CHECK 原文 ⇒ \
             库里可能出现没有序号的环节、或给依据配上序号：{def}");
}

#[test]
fn ref_kinds_are_turn_plus_the_canonical_referencable_tables() {
    // `keys.rs` 的注释声称 `REF_KINDS` 逐字等于 canonical DDL 的 `provenance.ref_kind` CHECK。
    // 上面那条对拍把 REF_KINDS 钉在 CHECK 上，而**两侧都是字面量**——于是"同步给 REF_KINDS 和
    // CHECK 各加一个 'fact_link'"这种改法全仓 0 红，可 T7 两跳溯源会把那个新值读成 `None`。
    // 这条用例钉的是值域的另一半：除 'turn' 之外每一项都必须是库里真实存在的表；
    // 两张可指认表（`MemTable` 的两个 prefix）必须在场；'fact_link' 是边、不是可引用行，不许出现。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let mut tables: Vec<String> = {
        let mut st = root.memory().conn().prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap()
            .filter_map(|r| r.ok()).collect()
    };
    tables.retain(|t| t != "rc_shadow" && t != "rc_index_marker" && !t.starts_with("rc_shadow_"));
    let mut non_turn: Vec<String> = REF_KINDS.iter().copied()
        .filter(|k| *k != "turn").map(|k| k.to_string()).collect();
    non_turn.sort();
    for k in &non_turn {
        assert!(tables.contains(k), "REF_KINDS 里的 {k} 不是库里的一张表");
    }
    for t in MEM_TABLES {
        assert!(non_turn.iter().any(|k| k == t.prefix()),
                "可指认表 {} 必须出现在 REF_KINDS 里", t.prefix());
        assert!(MemTable::parse(t.prefix()).is_some());
    }
    assert!(!non_turn.iter().any(|k| k == "fact_link"),
            "fact_link 是边、不是可指认行：它不许成为 ref_kind");
    assert_eq!(REF_KINDS.iter().copied().filter(|k| *k == "turn").count(), 1,
               "'turn' 必须在 REF_KINDS 里且只出现一次（它不属于任何表，两级溯源在类型上靠它可判）");
}

#[test]
fn fact_link_relation_check_is_exactly_the_nine_closed_enums() {
    // canonical：`fact_link.relation` 是九项封闭枚举（旧 relationship 的五项并集 A44 的因果值）。
    // 这里钉"CHECK 与设计字面量逐位对拍"。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let want = sorted(["causes", "enables", "prevents", "responds_to", "part_of",
                       "depends_on", "owns", "located_in", "role"]);
    assert_eq!(check_literals(&root, "fact_link", "relation"), want,
               "fact_link.relation 的 CHECK 偏离封闭枚举九项");
}

#[test]
fn attention_kind_check_is_exactly_the_five_canonical_enums() {
    // canonical §4：attention.kind 五值 = contradiction/alias_ambiguity/duplicate_title/
    // state_conflict/quote_conflict。Rust 侧 `AttentionKind` 今天只生产前三格，
    // 但 CHECK 是 canonical 值域（后两格归 M3）⇒ 这里钉五值，并另断"每个生产者快照都落在值域里"。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let want = sorted(["contradiction", "alias_ambiguity", "duplicate_title",
                       "state_conflict", "quote_conflict"]);
    let got = check_literals(&root, "attention", "kind");
    assert_eq!(got, want, "attention.kind 的 CHECK 偏离 canonical 五值");
    for k in rc_memory::keys::AttentionKind::ALL {
        assert!(got.contains(&k.as_str().to_string()),
                "AttentionKind 的 {} 不在 CHECK 值域里 ⇒ 写入会被库拒掉", k.as_str());
    }
}

#[test]
fn causal_type_check_is_exactly_the_three_chain_types() {
    // 批次 M：`card.causal_type` 三值 = causes/enables/prevents（只对 kind='causal' 的链头行有意义；
    // NULL = 老行/未给出，读侧按 unknown 渲染归段 C）。
    // 形状与 ref_kind/relation 那几条不同：约束外面套了 `IS NULL OR`，所以
    // `check_literals` 的锚点 `CHECK(<列> IN (` **咬不到**它——那个助手被七处既有对拍共用，
    // 这里自带一个局部取字面量的助手，从 `causal_type IN (` 起到最近的 `)`，按 `'` 隔取。
    // 三件断言：① 字面量集合恰好三值；② 不给 causal_type 的行合法且读回 NULL；
    // ③ 枚举外被拒、三值之一被收。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();

    fn literals_after(sql: &str, mark: &str) -> Vec<String> {
        let at = sql.find(mark)
            .unwrap_or_else(|| panic!("card 的建表原文里没有 `{mark}`，对拍无法执法：{sql}"));
        let rest = &sql[at + mark.len()..];
        let body = &rest[..rest.find(')')
            .unwrap_or_else(|| panic!("`{mark}` 的字面量列表没有闭合：{sql}"))];
        let mut got: Vec<String> = body.split('\'').skip(1).step_by(2).map(String::from).collect();
        got.sort();
        got
    }

    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='card'",
        [], |r| r.get(0)).unwrap();
    // ① 字面量集合：多一个/少一个都红。
    assert_eq!(literals_after(&sql, "causal_type IN ("),
               sorted(["causes", "enables", "prevents"]),
               "card.causal_type 的 CHECK 偏离链头三值：{sql}");

    // ② 空值合法：11 个 NOT NULL 列齐全、不给 causal_type ⇒ 读回 NULL。
    conn.execute(
        "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                          source_agent,device_id,created_at_ms,norm_version)
         VALUES('cd-null','causal','t','b','project','p',1,'agent-a','dev-1',1,1)", []).unwrap();
    let got: Option<String> = conn.query_row(
        "SELECT causal_type FROM card WHERE id='cd-null'", [], |r| r.get(0)).unwrap();
    assert_eq!(got, None, "不给 causal_type 的行必须落 NULL（NULL = 未给出，不是被 DEFAULT 填成三值之一）");

    // ③ 枚举外被拒；三值之一必须成功（否则"拒收"可能只是整列不可写）。
    assert!(conn.execute("UPDATE card SET causal_type='bogus' WHERE id='cd-null'", []).is_err(),
            "causal_type 的 CHECK 没生效：枚举外值 'bogus' 被收下了");
    conn.execute("UPDATE card SET causal_type='causes' WHERE id='cd-null'", [])
        .expect("'causes' 必须被收下，否则这条 CHECK 拒的是整列而不是枚举外值");
    let ok: Option<String> = conn.query_row(
        "SELECT causal_type FROM card WHERE id='cd-null'", [], |r| r.get(0)).unwrap();
    assert_eq!(ok.as_deref(), Some("causes"), "三值之一写进去读不回原值");
}

#[test]
fn memory_mention_primary_key_dedupes_the_same_source_and_anchor() {
    // 这条钉的是段 B `put_mentions` 幂等的**前提**（主键形状），不是它的行为本身：
    // 同一 (mem_table, mem_id, anchor_id) 的重复标注只该是一行——`created_at_ms` 不同也一样；
    // 换 anchor_id 就是另一条标注，必须落第二行。主键写错（少一列、写成唯一索引又漏掉）
    // ⇒ 段 B 的幂等只能在写侧再补一次去重。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    let insert = |anchor: &str, at: i64| {
        conn.execute(
            "INSERT OR IGNORE INTO memory_mention(mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms, norm_version)
             VALUES('card', 'c1', ?1, 'agent-a', 'dev-1', ?2, 1)",
            rusqlite::params![anchor, at]).unwrap()
    };
    insert("a1", 1);
    insert("a1", 2);
    let n: i64 = conn.query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "同一行对同一锚的重复标注必须只剩一行，实际落了 {n} 行");

    insert("a2", 3);
    let n: i64 = conn.query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "换 anchor_id 是另一条标注，必须落第二行，实际落了 {n} 行");
}
