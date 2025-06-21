fn main() {
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/api.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
