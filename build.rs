fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/kite.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/kite.ico");
        res.set("ProductName", "Kite");
        res.set("FileDescription", "Kite video editor");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}
