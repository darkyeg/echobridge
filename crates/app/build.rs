//! Compiles the Slint UI into Rust and, on Windows, embeds the icon and version details.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("the UI compiles");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/icon.ico")
            .set("ProductName", "EchoBridge")
            .set("FileDescription", "EchoBridge")
            .set("CompanyName", "EchoBridge")
            .set("LegalCopyright", "Apache-2.0");
        resource.compile().expect("the Windows resources compile");
    }
}
