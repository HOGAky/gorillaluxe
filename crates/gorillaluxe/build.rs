// Hybrid-GPU exports (TZ 0.2): NVIDIA Optimus and AMD PowerXpress
// drivers pick the adapter for OUR process by looking for these
// exported symbols on the main executable — without them a hybrid
// laptop may hand our DX12 compositor to the iGPU while the dGPU
// sits idle. `/EXPORT:` emits them into the .exe export table.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/EXPORT:NvOptimusEnablement");
        println!("cargo:rustc-link-arg=/EXPORT:AmdPowerXpressRequestHighPerformance");
    }
}
