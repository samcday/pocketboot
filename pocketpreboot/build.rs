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
    println!("cargo:rerun-if-changed=src/spin_table.S");
    println!("cargo:rerun-if-changed=src/exceptions.S");
    println!(
        "cargo:rustc-link-arg-bin=pocketpreboot=-T{}",
        linker_script.display()
    );
    println!("cargo:rustc-link-arg-bin=pocketpreboot=--gc-sections");
    println!("cargo:rustc-link-arg-bin=pocketpreboot=-pie");
    println!("cargo:rustc-link-arg-bin=pocketpreboot=--no-dynamic-linker");
    // The raw image is relocated before enabling an MMU; ELF RELRO segments
    // have no loader to apply permissions and need not be grouped together.
    println!("cargo:rustc-link-arg-bin=pocketpreboot=-znorelro");
}
