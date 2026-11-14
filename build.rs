fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("sidemic.ico", "1");
        res.set("ProductName", "SideMic");
        res.set("FileDescription", "SideMic - microphone passthrough for game streaming");
        res.set("LegalCopyright", "MIT");
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource failed: {e}");
        }
    }
}
