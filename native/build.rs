use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../resources/icons/mailgo.ico");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return Ok(());
    }

    let icon =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../resources/icons/mailgo.ico");
    let icon = icon.to_str().ok_or("MailGo icon path is not valid UTF-8")?;

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon)
        .set("ProductName", "MailGo")
        .set("FileDescription", "MailGo desktop email client")
        .set("CompanyName", "neko233-com")
        .set("LegalCopyright", "Copyright (c) neko233-com");
    resource.compile()?;
    Ok(())
}
