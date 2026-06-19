fn main() {
    // Rebuild when the build id changes so option_env!("PBSGUI_BUILD") is current.
    println!("cargo:rerun-if-env-changed=PBSGUI_BUILD");
}
