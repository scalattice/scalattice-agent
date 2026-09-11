fn main() {
    #[cfg(windows)]
    {
        let version = env!("CARGO_PKG_VERSION");
        let mut res = winres::WindowsResource::new();
        res.set_icon("installer/windows/scalattice.ico");
        res.set("ProductName", "Scalattice Agent");
        res.set("FileDescription", "Scalattice GPU agent");
        res.set("CompanyName", "Robottik Ltd");
        res.set("LegalCopyright", "Copyright (C) Robottik Ltd");
        // Keep string + numeric versions aligned so Explorer/ARP update after upgrades.
        res.set("ProductVersion", version);
        res.set("FileVersion", &format!("{version}.0"));
        let parts: Vec<u64> = version
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect();
        let major = *parts.first().unwrap_or(&0);
        let minor = *parts.get(1).unwrap_or(&0);
        let patch = *parts.get(2).unwrap_or(&0);
        let packed = (major << 48) | (minor << 32) | (patch << 16);
        res.set_version_info(winres::VersionInfo::FILEVERSION, packed);
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed);
        if let Err(err) = res.compile() {
            eprintln!("winres: {err}");
        }

        // CUDA/Vulkan runtimes and the NVIDIA driver are not present on CPU-only
        // PCs (or GitHub-hosted Windows). Delay-load so set-token / foreground
        // can start; llama.cpp then fails soft if the backend is actually used.
        if std::env::var("CARGO_FEATURE_CUDA").is_ok() {
            println!("cargo:rustc-link-arg=/DELAYLOAD:nvcuda.dll");
            println!("cargo:rustc-link-arg=/DELAYLOAD:cudart64_12.dll");
            println!("cargo:rustc-link-arg=/DELAYLOAD:cublas64_12.dll");
            println!("cargo:rustc-link-arg=/DELAYLOAD:cublasLt64_12.dll");
            println!("cargo:rustc-link-lib=delayimp");
        }
        if std::env::var("CARGO_FEATURE_VULKAN").is_ok() {
            println!("cargo:rustc-link-arg=/DELAYLOAD:vulkan-1.dll");
            println!("cargo:rustc-link-lib=delayimp");
        }
    }

    #[cfg(target_os = "macos")]
    {
        let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let plist = manifest.join("installer/macos/Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
