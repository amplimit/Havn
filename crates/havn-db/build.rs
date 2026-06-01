fn main() {
    // Recompile when migrations change so the embedded migration set stays fresh.
    println!("cargo:rerun-if-changed=migrations");
}
