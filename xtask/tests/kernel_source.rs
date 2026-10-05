use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "pocketboot-kernel-cli-{}-{nonce}",
            std::process::id()
        )))
    }

    fn kernel(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        for (file, contents) in [
            ("Makefile", ""),
            ("README", "Linux kernel\n============\n"),
            ("scripts/kconfig/merge_config.sh", ""),
        ] {
            let file = path.join(file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, contents).unwrap();
        }
        fs::canonicalize(path).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .env_remove("DELTA_DATABASE_DIR")
            .env("CDPATH", &self.0);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn outside_delta_ignores_populated_cdpath() {
    let f = Fixture::new();
    f.kernel("first");
    f.kernel("second");
    // A missing device exercises configured-source fallback without fetching.
    let output = f
        .command()
        .args(["kernel-src", "missing/not-a-device"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("configs/"), "{stderr}");
    assert!(!stderr.contains("multiple kernel"), "{stderr}");
}

#[test]
fn all_entrypoints_reject_ambiguous_delta_kernels_before_building() {
    let f = Fixture::new();
    f.kernel("first");
    f.kernel("second");
    for args in [
        &["build", "qcom/sdm670-google-sargo"][..],
        &["kernel", "qcom/sdm670-google-sargo"][..],
        &["kernel-src", "qcom/sdm670-google-sargo"][..],
        &["qemu", "--build-only", "--", "-smp", "2"][..],
    ] {
        let output = f
            .command()
            .env("DELTA_DATABASE_DIR", "test-context")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("multiple kernel trees in Delta CDPATH"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn kernel_src_accepts_explicit_path_over_ambiguous_delta_kernels() {
    let f = Fixture::new();
    f.kernel("first");
    let selected = f.kernel("kernel with spaces");
    let output = f
        .command()
        .env("DELTA_DATABASE_DIR", "test-context")
        .args(["kernel-src", "qcom/sdm670-google-sargo"])
        .arg(&selected)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("{} (explicit;", selected.display())),
        "{stdout}"
    );
}
