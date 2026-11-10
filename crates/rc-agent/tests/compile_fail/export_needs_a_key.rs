use rc_agent::ExportDoc;

fn main() {
    // 守卫 12 的第二种越界尝试：在 rc-agent 之外凭空造一个"已经装好正文"的出口对象。
    // 构造器是 pub(crate)，所以从测试 crate 调用报 E0624（method is private）。
    let doc = ExportDoc::from_markdown("任何字符串都能变成导出件".to_string());
    println!("{}", doc.text());
}
