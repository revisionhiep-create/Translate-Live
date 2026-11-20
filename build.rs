use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let png_path = manifest_dir.join("assets").join("icon.png");
    let ico_path = manifest_dir.join("assets").join("icon.ico");

    assert!(
        png_path.is_file(),
        "Missing required icon asset: {}",
        png_path.display()
    );
    assert!(
        ico_path.is_file(),
        "Missing required Windows icon asset: {}",
        ico_path.display()
    );

    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().ok_or("Invalid icon.ico path")?);
        res.compile()?;
    }

    Ok(())
}
