fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile the Slint UI");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=../../assets/branding");
}
