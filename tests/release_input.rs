//! Real EROFS release input lifetime integration, invoked by release.py.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|s| s == "--pack") {
        let root = std::path::Path::new(args.get(2).ok_or("root required")?);
        let image = std::path::Path::new(args.get(3).ok_or("image required")?);
        levitate_recipe::target::finalize(root)?;
        distro_builder::create_erofs(root, image, "lz4hc", 9, 1048576)?;
        return Ok(());
    }
    let manifest = std::path::PathBuf::from(std::env::args_os().nth(1).ok_or("manifest required")?);
    let verified = recab::release::verify_bundle(&manifest, None, true, None)?;
    let original = manifest
        .parent()
        .unwrap()
        .join(&verified.manifest().artifacts.kernel.path);
    let before = std::fs::read(verified.kernel())?;
    let replacement = original.with_extension("replacement");
    std::fs::write(&replacement, b"replaced mutable source")?;
    std::fs::rename(replacement, &original)?;
    std::fs::write(&manifest, b"replaced mutable manifest")?;
    if std::fs::read(verified.kernel())? != before {
        return Err("verified bytes changed with source path".into());
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .open(verified.kernel())
        .is_ok()
    {
        return Err("verified input is writable".into());
    }
    let consumed = recab::release::verify_bundle(&verified.manifest_path(), None, true, None)?;
    if consumed.id() != verified.id() {
        return Err("consumer reopened a different release".into());
    }
    println!("PASS: replacement of mutable source paths cannot change verified read-only inputs");
    Ok(())
}
