// Nuschel patch: shaders are compiled at run time from embedded source (see
// directx_renderer.rs), so no Windows-only fxc.exe step runs here.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
