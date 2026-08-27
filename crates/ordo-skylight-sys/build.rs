// SkyLight is a *private* framework: it isn't on the default framework search
// path, so we add PrivateFrameworks explicitly and link it by name. The public
// frameworks that carry the symbols we also declare here (CoreFoundation for CF
// getters, ApplicationServices for the private _AXUIElementGetWindow) are on the
// default path and linked by name.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");
        println!("cargo:rustc-link-lib=framework=SkyLight");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }
}
