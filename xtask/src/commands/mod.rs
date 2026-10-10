pub(crate) mod bootimg;
pub(crate) mod build;
pub(crate) mod busybox;
pub(crate) mod ci_matrix;
mod config;
pub(crate) mod cpio;
pub(crate) mod kernel;
pub(crate) mod kernel_src;
pub(crate) mod preboot;
pub(crate) mod qemu;

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, Permissions},
    path::{Path, PathBuf},
    process::Command,
    thread,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::Result;

const DEFAULT_KERNEL_ARCH: &str = "arm64";

#[derive(Clone, Debug, Default)]
pub(super) struct FeatureSet {
    values: Vec<String>,
}

impl FeatureSet {
    pub(super) fn add(&mut self, value: &str) -> Result<()> {
        for feature in value.split(|ch: char| ch == ',' || ch.is_ascii_whitespace()) {
            if feature.is_empty() {
                continue;
            }
            validate_feature(feature)?;
            if !self.values.iter().any(|existing| existing == feature) {
                self.values.push(feature.to_string());
            }
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(super) fn contains(&self, feature: &str) -> bool {
        self.values.iter().any(|value| value == feature)
    }

    pub(super) fn cargo_value(&self) -> String {
        self.values.join(",")
    }

    pub(super) fn values(&self) -> &[String] {
        &self.values
    }
}

fn feature_set(values: &[String]) -> Result<FeatureSet> {
    let mut features = FeatureSet::default();
    for value in values {
        features.add(value)?;
    }
    Ok(features)
}

fn validate_feature(feature: &str) -> Result<()> {
    if feature
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/'))
    {
        Ok(())
    } else {
        Err(format!("invalid feature name: {feature}"))
    }
}

#[derive(Clone, Debug)]
struct KernelDevice {
    vendor: String,
    stem: String,
    soc: String,
}

impl KernelDevice {
    fn parse(value: &str) -> Result<Self> {
        let parts = value.split('/').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(format!(
                "device ID must be a canonical DTB path without suffix, e.g. qcom/msm8916-samsung-a5u-eur: {value}"
            ));
        }

        let vendor = parts[0];
        let stem = parts[1];
        validate_device_component("vendor", vendor)?;
        validate_device_component("device", stem)?;
        if stem.ends_with(".dts") || stem.ends_with(".dtb") {
            return Err(format!("device ID must omit .dts/.dtb suffix: {value}"));
        }

        let soc = stem.split_once('-').map_or(stem, |(soc, _)| soc);
        Ok(Self {
            vendor: vendor.to_string(),
            stem: stem.to_string(),
            soc: soc.to_string(),
        })
    }

    fn id(&self) -> String {
        format!("{}/{}", self.vendor, self.stem)
    }
}

impl std::str::FromStr for KernelDevice {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn validate_device_component(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(format!("invalid {kind} component in device ID: {value}"));
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(format!("invalid {kind} component in device ID: {value}"))
    }
}

fn has_linux_readme(path: &Path) -> bool {
    fs::read_to_string(path.join("README"))
        .is_ok_and(|readme| readme.trim_start().starts_with("Linux kernel"))
}

fn kernel_tree(path: &Path) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    if !has_linux_readme(&path) {
        return Err(format!(
            "{} does not identify a Linux kernel source tree",
            path.join("README").display()
        ));
    }
    ensure_file(&path.join("Makefile"), "kernel Makefile")?;
    ensure_file(
        &path.join("scripts/kconfig/merge_config.sh"),
        "merge_config.sh",
    )?;
    Ok(path)
}

fn canonical_file(path: &Path, description: &str) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    ensure_file(&path, description)?;
    Ok(path)
}

fn make_command_for_arch(kernel_tree: &Path, out_dir: &Path, arch: &str) -> Result<Command> {
    let make = env::var_os("MAKE").unwrap_or_else(|| "make".into());
    let mut output = OsString::from("O=");
    output.push(out_dir.as_os_str());

    let mut command = Command::new(make);
    command
        .current_dir(kernel_tree)
        .env("ARCH", arch)
        .arg(output);
    set_default_kernel_toolchain(&mut command, arch, Some(out_dir))?;
    Ok(command)
}

fn set_default_kernel_toolchain(
    command: &mut Command,
    arch: &str,
    out_dir: Option<&Path>,
) -> Result<()> {
    if env::var_os("CROSS_COMPILE").is_none() && env::var_os("LLVM").is_none() {
        match arch {
            DEFAULT_KERNEL_ARCH => {
                command.env("CROSS_COMPILE", "aarch64-linux-gnu-");
            }
            "arm" => {
                command.env("LLVM", "1");
                if !path_command_exists("ld.lld")
                    && let Some(tool_dir) = arm_llvm_tool_dir(out_dir)?
                {
                    prepend_command_path(command, &tool_dir)?;
                }
                if env::var_os("LLVM_IAS").is_none() {
                    command.env("LLVM_IAS", "1");
                }
            }
            _ => {}
        }
    }
    if let Some(out_dir) = out_dir {
        prepend_compiler_cache_wrappers(command, out_dir, &[])?;
    }
    Ok(())
}

/// Bare compiler command names wrapped with the selected compilation cache.
/// The musl names let BusyBox's default toolchains hit the cache; bare custom
/// names are supplied per command by the caller. ccache and sccache are
/// deliberately absent: a cache program is never itself a compiler to wrap.
const CACHE_WRAPPED_COMPILERS: &[&str] = &[
    "gcc",
    "g++",
    "cc",
    "c++",
    "aarch64-linux-gnu-gcc",
    "aarch64-linux-musl-gcc",
    "arm-linux-gnueabihf-gcc",
    "arm-linux-musleabihf-gcc",
    "clang",
    "clang++",
];

/// Directory names under which distro packages install ccache and sccache
/// masquerade compilers. Selecting one of those as the "real" compiler would
/// make our wrapper invoke a cache on a cache.
const CACHE_MASQUERADE_DIRS: &[&str] = &["ccache", "sccache"];

/// The compilation cache the developer opted into: CCACHE_DIR selects ccache
/// (existing precedence), otherwise SCCACHE_DIR selects sccache. Only these
/// two variables opt in: Rust builds keep following RUSTC_WRAPPER on their
/// own, and Rust wrapper settings must never enable C caching here.
fn compiler_cache_backend(
    ccache_dir: Option<&OsStr>,
    sccache_dir: Option<&OsStr>,
) -> Option<&'static str> {
    if ccache_dir.is_some() {
        Some("ccache")
    } else if sccache_dir.is_some() {
        Some("sccache")
    } else {
        None
    }
}

fn prepend_compiler_cache_wrappers(
    command: &mut Command,
    out_dir: &Path,
    extra_compilers: &[String],
) -> Result<()> {
    prepend_compiler_cache_wrappers_with_backend(
        command,
        out_dir,
        extra_compilers,
        compiler_cache_backend(
            env::var_os("CCACHE_DIR").as_deref(),
            env::var_os("SCCACHE_DIR").as_deref(),
        ),
    )
}

/// Write `exec <cache> <compiler>` shims for every wrapped compiler name found
/// on PATH, then prepend their directory to the command's PATH. Wrappers
/// invoke the cache program in its stable `<cache> <compiler> <args...>` form
/// (supported by ccache and sccache releases including 0.10.x) and set no
/// cache-specific flags, environment, or job settings.
fn prepend_compiler_cache_wrappers_with_backend(
    command: &mut Command,
    out_dir: &Path,
    extra_compilers: &[String],
    backend: Option<&str>,
) -> Result<()> {
    // Old wrappers must not override a newly selected script or a removed
    // compiler. Also clean up on opt-out: ARM's linker fallback can put this
    // directory on PATH even without a compiler cache.
    remove_cache_wrappers(&out_dir.join("pocketboot-toolchain"))?;
    let Some(backend) = backend else {
        return Ok(());
    };
    let tool_dir = generated_tool_dir(out_dir)?;
    let mut compilers = CACHE_WRAPPED_COMPILERS.to_vec();
    for extra in extra_compilers {
        if !compilers.contains(&extra.as_str()) {
            compilers.push(extra.as_str());
        }
    }
    for compiler in compilers {
        if let Some(real_compiler) = compiler_path_command(compiler, &tool_dir) {
            write_cache_wrapper(&tool_dir.join(compiler), &real_compiler, backend)?;
        }
    }
    prepend_command_path(command, &tool_dir)
}

fn remove_cache_wrappers(tool_dir: &Path) -> Result<()> {
    let entries = match fs::read_dir(tool_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("read {}: {err}", tool_dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|err| format!("read {} entry: {err}", tool_dir.display()))?;
        if !entry
            .file_type()
            .map_err(|err| format!("stat {}: {err}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        let contents = fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
        // Preserve other tools in this directory, notably the ld.lld fallback.
        if contents.starts_with(b"#!/bin/sh\nexec ccache '")
            || contents.starts_with(b"#!/bin/sh\nexec sccache '")
        {
            fs::remove_file(&path).map_err(|err| format!("remove {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

/// Resolve a compiler command without ever selecting a compilation cache
/// masquerade, which would make our own `<cache> <compiler>` wrapper recurse
/// into the cache again: bare names are searched in PATH order, skipping the
/// generated wrapper directory, ccache/sccache masquerade directories, and
/// candidates which resolve to either cache binary. A shebang candidate is
/// refused as well; see `is_shebang_script`.
fn compiler_path_command(command: &str, wrapper_dir: &Path) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let search_paths = env::split_paths(&paths).collect::<Vec<_>>();
    let cache_binaries = [path_command("ccache"), path_command("sccache")];
    resolve_compiler_command(
        OsStr::new(command),
        &search_paths,
        &cache_binaries,
        wrapper_dir,
    )
}

fn resolve_compiler_command(
    command: &OsStr,
    search_paths: &[PathBuf],
    cache_binaries: &[Option<PathBuf>],
    wrapper_dir: &Path,
) -> Option<PathBuf> {
    if !is_bare_command_name(command) {
        // An explicit path is the user's compiler selection: return it exactly
        // as supplied. Such paths are documented bypasses of wrapper
        // resolution and are never second-guessed.
        let requested = Path::new(command);
        return requested.is_file().then(|| requested.to_path_buf());
    }

    let selected = search_paths
        .iter()
        .map(|dir| dir.join(command))
        .find(|candidate| {
            candidate.is_file()
                && !candidate.starts_with(wrapper_dir)
                && !is_cache_shim(candidate, cache_binaries)
        });
    match selected {
        // Check the real compiler, not a script's identity. Leave scripts on
        // PATH so they can call a wrapped compiler (as Fedora's musl GCC does).
        // Paths our UTF-8 wrapper writer cannot represent also pass through.
        // Do not select a later candidate: that would change the compiler.
        Some(candidate) if candidate.to_str().is_none() || is_shebang_script(&candidate) => None,
        selected => selected,
    }
}

/// A bare command name is resolved through PATH; anything else (absolute or
/// multi-component) is an explicit selection.
fn is_bare_command_name(command: &OsStr) -> bool {
    let requested = Path::new(command);
    !requested.as_os_str().is_empty()
        && !requested.is_absolute()
        && requested.components().count() == 1
}

/// A shebang file is an interpreter script, not a compiler binary. Wrapping
/// one would make the cache's compiler check identify the script rather than
/// the compiler it execs, so such candidates are never selected.
fn is_shebang_script(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut prefix = [0; 2];
    file.read_exact(&mut prefix).is_ok() && prefix == *b"#!"
}

fn is_cache_shim(candidate: &Path, cache_binaries: &[Option<PathBuf>]) -> bool {
    if candidate
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| {
            CACHE_MASQUERADE_DIRS
                .iter()
                .any(|dir| name == OsStr::new(dir))
        })
    {
        return true;
    }

    let Ok(resolved) = fs::canonicalize(candidate) else {
        return false;
    };
    cache_binaries
        .iter()
        .flatten()
        .any(|cache| fs::canonicalize(cache).is_ok_and(|binary| binary == resolved))
}

/// Directory for generated tool wrappers inside a build output tree.
fn generated_tool_dir(out_dir: &Path) -> Result<PathBuf> {
    let tool_dir = out_dir.join("pocketboot-toolchain");
    fs::create_dir_all(&tool_dir).map_err(|err| format!("create {}: {err}", tool_dir.display()))?;
    Ok(tool_dir)
}

fn write_cache_wrapper(path: &Path, compiler: &Path, backend: &str) -> Result<()> {
    let compiler = compiler
        .to_str()
        .ok_or_else(|| format!("compiler path is not valid UTF-8: {}", compiler.display()))?;
    if path.is_symlink() {
        fs::remove_file(path).map_err(|err| format!("remove {}: {err}", path.display()))?;
    }
    fs::write(
        path,
        format!(
            "#!/bin/sh\nexec {backend} '{}' \"$@\"\n",
            sh_quote(compiler)
        ),
    )
    .map_err(|err| format!("write {}: {err}", path.display()))?;
    make_executable(path)
}

fn sh_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn arm_llvm_tool_dir(out_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(out_dir) = out_dir else {
        return Ok(None);
    };
    if !path_command_exists("rust-lld") {
        return Ok(None);
    }
    let tool_dir = generated_tool_dir(out_dir)?;
    let ld_lld = tool_dir.join("ld.lld");
    write_ld_lld_wrapper(&ld_lld)?;
    Ok(Some(tool_dir))
}

fn write_ld_lld_wrapper(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        fs::remove_file(path).map_err(|err| format!("remove {}: {err}", path.display()))?;
    }
    fs::write(path, b"#!/bin/sh\nexec rust-lld -flavor gnu \"$@\"\n")
        .map_err(|err| format!("write {}: {err}", path.display()))?;
    make_executable(path)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .map_err(|err| format!("chmod {}: {err}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn prepend_command_path(command: &mut Command, dir: &Path) -> Result<()> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(paths).map_err(|err| format!("join PATH: {err}"))?;
    command.env("PATH", path);
    Ok(())
}

fn path_command(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(OsStr::new(name)))
        .find(|path| path.is_file())
}

fn path_command_exists(name: &str) -> bool {
    path_command(name).is_some()
}

fn run_command(mut command: Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|err| format!("spawn {action}: {err}"))?;
    if !status.success() {
        return Err(format!("{action} failed with {status}"));
    }
    Ok(())
}

fn parallel_jobs() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

fn ensure_file(path: &Path, description: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("missing {description}: {}", path.display()))
    }
}

fn kconfig_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    Ok(escaped)
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask manifest directory has no parent".to_string())
}

fn target_dir(workspace_root: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_resolution_skips_fedora_ccache_shim_directory() {
        let root = test_dir("ccache-fedora");
        let shim_dir = root.join("usr/lib64/ccache");
        let real_dir = root.join("usr/bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::write(shim_dir.join("aarch64-linux-gnu-gcc"), b"ccache shim").unwrap();
        fs::write(real_dir.join("aarch64-linux-gnu-gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("aarch64-linux-gnu-gcc"),
            &[shim_dir, real_dir.clone()],
            &[],
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("aarch64-linux-gnu-gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compiler_resolution_skips_symlink_to_ccache_outside_named_shim_directory() {
        use std::os::unix::fs::symlink;

        let root = test_dir("ccache-symlink");
        let shim_dir = root.join("wrappers");
        let real_dir = root.join("bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        let ccache = real_dir.join("ccache");
        fs::write(&ccache, b"ccache").unwrap();
        symlink(&ccache, shim_dir.join("gcc")).unwrap();
        fs::write(real_dir.join("gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("gcc"),
            &[shim_dir, real_dir.clone()],
            &[Some(ccache.clone())],
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_compiler_path_and_shell_quoting_are_preserved() {
        let root = test_dir("ccache-explicit");
        let wrapper_dir = root.join("wrapper output");
        let compiler = root.join("toolchain with spaces/compiler's gcc");
        fs::create_dir_all(compiler.parent().unwrap()).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::write(&compiler, b"real compiler").unwrap();

        let selected =
            resolve_compiler_command(compiler.as_os_str(), &[], &[], &wrapper_dir).unwrap();
        assert_eq!(selected, compiler);

        let wrapper = wrapper_dir.join("gcc");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&selected, &wrapper).unwrap();
        write_cache_wrapper(&wrapper, &selected, "ccache").unwrap();
        assert_eq!(fs::read(&selected).unwrap(), b"real compiler");
        let contents = fs::read_to_string(&wrapper).unwrap();
        let compiler = selected.to_str().unwrap();
        assert_eq!(
            contents,
            format!("#!/bin/sh\nexec ccache '{}' \"$@\"\n", sh_quote(compiler))
        );

        write_cache_wrapper(&wrapper, &selected, "sccache").unwrap();
        let contents = fs::read_to_string(&wrapper).unwrap();
        assert_eq!(
            contents,
            format!("#!/bin/sh\nexec sccache '{}' \"$@\"\n", sh_quote(compiler))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_resolution_does_not_reuse_generated_wrapper() {
        let root = test_dir("ccache-wrapper-loop");
        let wrapper_dir = root.join("pocketboot-toolchain");
        let real_dir = root.join("bin");
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(wrapper_dir.join("clang"), b"old generated wrapper").unwrap();
        fs::write(real_dir.join("clang"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("clang"),
            &[wrapper_dir.clone(), real_dir.clone()],
            &[],
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("clang"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_cache_backend_precedence_and_no_opt_in() {
        assert_eq!(
            compiler_cache_backend(Some(OsStr::new("/cache")), Some(OsStr::new("/sccache"))),
            Some("ccache")
        );
        assert_eq!(
            compiler_cache_backend(Some(OsStr::new("/cache")), None),
            Some("ccache")
        );
        assert_eq!(
            compiler_cache_backend(None, Some(OsStr::new("/sccache"))),
            Some("sccache")
        );
        assert_eq!(compiler_cache_backend(None, None), None);
    }

    #[test]
    fn cache_wrapper_refresh_removes_stale_compilers_but_preserves_linker() {
        let root = test_dir("cache-wrapper-refresh");
        let tool_dir = generated_tool_dir(&root).unwrap();
        let stale = tool_dir.join("pocketboot-stale-compiler");
        let linker = tool_dir.join("ld.lld");
        write_ld_lld_wrapper(&linker).unwrap();
        let linker_contents = fs::read(&linker).unwrap();

        for backend in [Some("sccache"), None] {
            write_cache_wrapper(&stale, Path::new("/old/compiler"), "ccache").unwrap();
            let mut command = Command::new("make");
            prepend_compiler_cache_wrappers_with_backend(&mut command, &root, &[], backend)
                .unwrap();
            assert!(!stale.exists());
            assert_eq!(fs::read(&linker).unwrap(), linker_contents);
            if backend.is_none() {
                assert!(command.get_envs().next().is_none());
                assert!(!tool_dir.join("gcc").exists());
            }
        }

        let fresh = root.join("no-opt-in");
        let mut command = Command::new("make");
        prepend_compiler_cache_wrappers_with_backend(&mut command, &fresh, &[], None).unwrap();
        assert!(!fresh.exists());
        assert!(command.get_envs().next().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_resolution_skips_sccache_masquerade_directory() {
        let root = test_dir("sccache-fedora");
        let shim_dir = root.join("usr/lib64/sccache");
        let real_dir = root.join("usr/bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::write(
            shim_dir.join("aarch64-linux-musl-gcc"),
            b"sccache masquerade",
        )
        .unwrap();
        fs::write(real_dir.join("aarch64-linux-musl-gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("aarch64-linux-musl-gcc"),
            &[shim_dir, real_dir.clone()],
            &[],
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("aarch64-linux-musl-gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compiler_resolution_skips_symlink_to_sccache_outside_named_masquerade_directory() {
        use std::os::unix::fs::symlink;

        let root = test_dir("sccache-symlink");
        let shim_dir = root.join("wrappers");
        let real_dir = root.join("bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        let sccache = real_dir.join("sccache");
        fs::write(&sccache, b"sccache").unwrap();
        symlink(&sccache, shim_dir.join("gcc")).unwrap();
        fs::write(real_dir.join("gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("gcc"),
            &[shim_dir, real_dir.clone()],
            &[Some(sccache)],
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shebang_compiler_script_passes_through_and_underlying_gcc_is_resolved() {
        let root = test_dir("musl-shebang");
        let bin = root.join("usr/bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        // Fedora ships /usr/bin/aarch64-linux-musl-gcc as exactly such a script.
        fs::write(
            bin.join("aarch64-linux-musl-gcc"),
            b"#!/bin/sh\nexec aarch64-linux-gnu-gcc \"$@\"\n",
        )
        .unwrap();
        fs::write(bin.join("aarch64-linux-gnu-gcc"), b"\x7fELF real compiler").unwrap();

        // The script is never wrapped: the cache's compiler check would
        // identify the script, so the name resolves unwrapped through PATH at
        // run time while its inner invocations hit wrapped real compilers.
        assert_eq!(
            resolve_compiler_command(
                OsStr::new("aarch64-linux-musl-gcc"),
                std::slice::from_ref(&bin),
                &[],
                &wrapper_dir
            ),
            None
        );
        // The real compiler the script execs is selected and cached safely.
        assert_eq!(
            resolve_compiler_command(
                OsStr::new("aarch64-linux-gnu-gcc"),
                std::slice::from_ref(&bin),
                &[],
                &wrapper_dir
            ),
            Some(bin.join("aarch64-linux-gnu-gcc"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compiler_in_non_utf8_path_remains_unwrapped() {
        use std::os::unix::ffi::OsStrExt;

        let root = test_dir("compiler-path-encoding");
        let bin = root.join(OsStr::from_bytes(b"bin-\xff"));
        let fallback = root.join("fallback");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&fallback).unwrap();
        fs::write(bin.join("gcc"), b"\x7fELF first compiler").unwrap();
        fs::write(fallback.join("gcc"), b"\x7fELF different compiler").unwrap();
        assert_eq!(
            resolve_compiler_command(
                OsStr::new("gcc"),
                &[bin, fallback],
                &[],
                &root.join("pocketboot-toolchain")
            ),
            None
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "pocketboot-xtask-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
