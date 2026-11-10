use rc_agent::InjectionBlock;

fn main() {
    // 守卫 12 的越界尝试：把注入块拆成可自由拼接的字符串再转发。
    let block: InjectionBlock = panic!();
    let raw: String = block.inner;
    println!("{raw}");
}
