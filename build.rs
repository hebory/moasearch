fn main() {
    println!("cargo:rerun-if-changed=assets/branding/moasearch.ico");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/branding/moasearch.ico")
            .set("ProductName", "MoaSearch")
            .set("FileDescription", "모아검색 - 내 파일을 위한 검색")
            .set("OriginalFilename", "moasearch.exe")
            .set("ProductVersion", env!("CARGO_PKG_VERSION"))
            .set("FileVersion", env!("CARGO_PKG_VERSION"))
            .compile()
            .expect("Windows icon and version resources must compile");
    }
}
