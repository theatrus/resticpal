fn main() {
    println!("cargo:rerun-if-changed=resticpal.rc");
    println!("cargo:rerun-if-changed=../../assets/resticpal.ico");
    embed_resource::compile("resticpal.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("resticpal tray resources should compile");
}
