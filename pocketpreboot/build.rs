use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let linker_script = manifest_dir.join("linker.ld");

    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rerun-if-changed=src/start.S");
    println!(
        "cargo:rustc-link-arg-bin=pocketpreboot=-T{}",
        linker_script.display()
    );
    println!("cargo:rustc-link-arg-bin=pocketpreboot=--gc-sections");
}
