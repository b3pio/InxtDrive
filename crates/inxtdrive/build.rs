fn main() {
    println!("cargo:rerun-if-changed=../../assets/inxtdrive.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // WinFsp's DLL is loaded at runtime from its install folder, so the
        // binaries must delay-load it rather than import it at startup.
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winfsp-x64.dll");
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:winfsp-x64.dll");

        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("../../assets/inxtdrive.ico", "1")
            .set("ProductName", "InxtDrive")
            .set("FileDescription", "InxtDrive - Internxt cloud drive for Windows (unofficial)")
            .set("LegalCopyright", "GPL-3.0-or-later");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}
