use rc_memory::{check_alias, put_entity, EntityContent, MemError, Provenance, AnchorRole, Scope, ScopeKind};
use rc_store::Root;

fn p() -> Vec<Provenance> { vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }] }
fn ent(title: &str, aliases: &[&str]) -> EntityContent {
    EntityContent { kind: "person".into(), title: title.into(),
                    scope: Scope { kind: ScopeKind::Person, key: "p-synthetic".into() },
                    aliases: aliases.iter().map(|s| s.to_string()).collect() }
}

#[test]
fn pronouns_never_become_aliases() {
    for bad in ["我", "你", "他", "她", "它", "我们", "你们", "他们", "自己", "别人",
                "I", "you", "he", "she", "it", "they", "We", "YOU", "This"] {
        assert!(matches!(check_alias(bad), Err(MemError::AliasIsPronoun(_))), "{bad} 竟然过了闸门");
    }
    // 闸门判的是"整串等于一个代词"，不是"含一个代词字符"——误拦比漏拦贵（真实体进不了库）
    for ok in ["我们的项目", "小明", "It-Designs", "自己人规则", "i-95"] {
        assert!(check_alias(ok).is_ok(), "{ok} 被误拦");
    }
}

#[test]
fn the_gate_is_on_the_only_write_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert!(matches!(put_entity(root.memory(), &ent("老王", &["我"]), &p(), 1).unwrap_err(),
                     MemError::AliasIsPronoun(_)));
    assert!(matches!(put_entity(root.memory(), &ent("它", &["老王"]), &p(), 1).unwrap_err(),
                     MemError::AliasIsPronoun(_)));
    for t in ["anchor", "alias"] {
        let n: i64 = root.memory().conn()
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "被闸门拦下的必须一条都不留");
    }
}

#[test]
fn a_pronoun_alias_cannot_be_sneaked_in_by_editing_the_fixture() {
    // 这条是守卫 10 的"第二个方向"：§8 说有人会"顺手把'我'加进别名表来修处境类召回"。
    // 合成播种里若出现代词别名 ⇒ 当场红（synth 自己也走闸门，没有旁路）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    rc_memory::synth::seed(root.memory(), 1).unwrap();
    let mut st = root.memory().conn()
        .prepare("SELECT alias FROM alias").unwrap();
    let aliases: Vec<String> = st.query_map([], |r| r.get(0)).unwrap()
        .filter_map(|r| r.ok()).collect();
    for a in aliases {
        assert!(check_alias(&a).is_ok(), "synth 播进了代词别名：{a}");
    }
}
