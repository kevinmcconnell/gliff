fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    pkg_config::Config::new()
        .atleast_version("1.20")
        .probe("libva")
        .expect("libva development files (pkg-config libva)");
    pkg_config::probe_library("libva-drm").expect("libva-drm development files");
}
