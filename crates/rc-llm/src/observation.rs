//! 观测面（W33-A10）的共享写口：真实出网耗时的观测格。
//!
//! **取 max 是这个类型的一部分，不是使用方的纪律**：写口只有一个（[`MaxCell::note`]），
//! 语义是"只记比现存更慢的一发"——观测口径是"最慢一发"，任何绕过 `note` 的裸写
//! 都写不出来（字段私有）。读口 [`MaxCell::get`] 随便读。
//!
//! 住 `rc-llm` 是依赖方向的裁定：写口在 `rc-llm-http` 的出网点（transport 的单调钟），
//! 读口在 `rc-cycle` 的账本（`Metered`），两层都依赖本 crate——这一格不能只属于任何一层。

use std::cell::Cell;

/// 本进程"最慢一发真实出网耗时（毫秒）"的观测格。单调钟的数，不是墙上钟。
#[derive(Debug, Default)]
pub struct MaxCell(Cell<i64>);

impl MaxCell {
    /// 观测格的构造点：起账 0（"还没有一发真实出网"）。
    pub fn new() -> MaxCell {
        MaxCell(Cell::new(0))
    }

    /// 唯一写口：只记比现存更慢的一发。
    pub fn note(&self, ms: i64) {
        if ms > self.0.get() {
            self.0.set(ms);
        }
    }

    /// 读口：当前最慢一发的毫秒数。没有真实出网 ⇒ 0（stub 不发明耗时）。
    pub fn get(&self) -> i64 {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::MaxCell;

    #[test]
    fn note_keeps_the_max_not_the_last() {
        let c = MaxCell::new();
        assert_eq!(c.get(), 0, "还没有一发 ⇒ 0");
        c.note(300);
        c.note(120);
        assert_eq!(c.get(), 300, "更慢的一发在前面 ⇒ 后写的小数不许覆盖");
        c.note(900);
        assert_eq!(c.get(), 900);
    }

    #[test]
    fn zero_and_negative_never_descend_below_an_existing_reading() {
        let c = MaxCell::new();
        c.note(0);
        assert_eq!(c.get(), 0);
        c.note(50);
        c.note(0);
        assert_eq!(c.get(), 50, "补测拿到的 0 不许把真实读数抹掉");
    }
}
