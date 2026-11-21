fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if !cfg!(debug_assertions) {
        println!("cargo:rerun-if-changed=../../web/dist");
    }
}
