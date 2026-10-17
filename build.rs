fn main() {
    slint_build::compile("ui/bundle.slint").expect("the Slint UI should compile");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("app.manifest");
    res.set_icon("assets/icon.ico");
    res.set("CompanyName", "BenZoN");
    res.set("LegalCopyright", "Copyright (c) 2026 BenZoN");
    res.set("ProductName", "Auto Renamer");
    res.set("FileDescription", "Auto Renamer");
    res.compile()
        .expect("kunne ikke indlejre Windows-manifestet");
}
