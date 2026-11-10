use rc_store::Root;
fn needs_transcript(_h: &rc_store::TranscriptHandle) {}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Root::open(std::path::Path::new("."))?;
    needs_transcript(root.ledger());
    Ok(())
}
