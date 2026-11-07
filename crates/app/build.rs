fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/icon/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/icon.ico");
        res.set("ProductName", "WoW is a Feeling");
        res.set("FileDescription", "WoW is a Feeling");
        res.set(
            "LegalCopyright",
            "GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.",
        );
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
