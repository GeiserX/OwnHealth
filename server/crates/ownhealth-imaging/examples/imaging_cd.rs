//! Write the synthetic CT disc for the Imaging CD workflow.
#[path = "../tests/support/mod.rs"]
mod support;

fn main() -> anyhow::Result<()> {
    let output = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: imaging_cd <output.zip>"))?;
    let mut ct = support::Study::new("1.2.826.0.1.3680043.10.999.44", support::ALEX);
    ct.series = 4;
    ct.per_series = 500;
    ct.size = 512;
    std::fs::write(output, support::zip_files(&support::hospital_cd(&[ct])))?;
    Ok(())
}
