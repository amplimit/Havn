fn main() {
    // Recompile when the dashboard is exported again, so the embedded copy
    // (src/dashboard.rs) stays fresh. The folder is made when missing: cargo
    // would take a missing path as changed on every build.
    let out = std::path::Path::new("../../apps/dashboard/out");
    if !out.exists() {
        let _ = std::fs::create_dir_all(out);
    }
    println!("cargo:rerun-if-changed=../../apps/dashboard/out");
}
