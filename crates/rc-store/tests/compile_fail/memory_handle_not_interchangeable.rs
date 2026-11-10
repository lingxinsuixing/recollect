use rc_store::Root;
fn needs_memory(_h: &rc_store::MemoryHandle) {}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Root::open(std::path::Path::new("."))?;
    needs_memory(root.transcript());
    Ok(())
}
