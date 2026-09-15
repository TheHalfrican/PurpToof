//! Embeds the Windows resources: the icon and the version block.
//!
//! Without this the executable is a generic blank page in Explorer, the Start
//! menu and Add/Remove Programs, and its Properties dialog shows no version or
//! publisher. The eframe icon only covers the app's own title bar - everything
//! Windows itself draws reads the compiled-in resource.

fn main() {
    // Only meaningful on Windows, and the crate does not build elsewhere, but
    // guarding keeps the build script honest about what it assumes.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/purptoof.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/purptoof.ico")
        .set("FileDescription", "Bluetooth audio receiver that recovers itself")
        .set("ProductName", "PurpToof")
        .set("CompanyName", "TheHalfrican")
        .set("LegalCopyright", "MIT licensed");

    if let Err(e) = res.compile() {
        // A missing resource compiler should not stop a developer building and
        // testing; it only costs the icon and the version block.
        println!("cargo:warning=could not embed Windows resources: {e}");
    }
}
