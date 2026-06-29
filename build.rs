fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("installer/windows/scalattice.ico");
        if let Err(err) = res.compile() {
            eprintln!("winres: {err}");
        }
    }
}
