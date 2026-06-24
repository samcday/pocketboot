use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{
    FeatureSet, KernelDevice,
    busybox::{BusyBoxInstall, build as build_busybox},
    config::{BootMenuConfig, CpioConfig},
    target_dir, workspace_root,
};

pub(super) const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
pub(super) const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";

const INIT_BINARY: &str = "pocketboot";
const BOOTMENU_MODULE_MANIFEST: &str = "etc/pocketboot/modules/bootmenu.list";

#[derive(clap::Args, Debug)]
pub(crate) struct InitrdArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
    #[arg(long, value_name = "TARGET")]
    target: Option<String>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
}

pub(crate) fn run(args: InitrdArgs) -> Result<()> {
    initrd(args)
}

fn initrd(args: InitrdArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = target_dir(&workspace_root);
    let config = super::config::load_device_config(&workspace_root, &args.device)?;
    let mut cpio_config = config.cpio.clone();
    if let Some(target) = args.target {
        cpio_config.target = Some(target);
    }
    let output = build_device_initrd(
        &workspace_root,
        &target_dir,
        &args.device,
        &cpio_config,
        &config.features,
        &config.bootmenu,
        args.output,
        true,
    )?;
    println!("wrote {}", output.display());
    Ok(())
}

pub(super) fn device_initrd_path(target_dir: &Path, device: &KernelDevice) -> PathBuf {
    target_dir
        .join("initrd")
        .join(&device.vendor)
        .join(&device.stem)
        .join(DEFAULT_INITRD)
}

pub(super) fn device_initrd_target(config: &CpioConfig) -> Result<String> {
    let target = config.target.as_deref().unwrap_or(DEFAULT_TARGET);
    if target.is_empty() {
        return Err("initrd target must not be empty".to_string());
    }
    Ok(target.to_string())
}

pub(super) fn build_device_initrd(
    workspace_root: &Path,
    target_dir: &Path,
    device: &KernelDevice,
    config: &CpioConfig,
    features: &FeatureSet,
    bootmenu: &BootMenuConfig,
    output: Option<PathBuf>,
    build_binary: bool,
) -> Result<PathBuf> {
    let target = device_initrd_target(config)?;
    let module_archive = ModuleArchive::for_bootmenu(target_dir, device, bootmenu)?;
    build_initrd_with_options(
        workspace_root,
        &target,
        output.or_else(|| Some(device_initrd_path(target_dir, device))),
        features.contains("busybox"),
        features,
        module_archive.as_ref(),
        build_binary,
    )
}

fn build_initrd_with_options(
    workspace_root: &Path,
    target: &str,
    output: Option<PathBuf>,
    include_busybox: bool,
    features: &FeatureSet,
    modules: Option<&ModuleArchive>,
    build_binary: bool,
) -> Result<PathBuf> {
    if build_binary {
        build_release(workspace_root, target, features)?;
    }

    let target_dir = target_dir(workspace_root);
    let init = target_dir.join(target).join("release").join(INIT_BINARY);
    if !init.is_file() {
        return Err(format!("release binary not found: {}", init.display()));
    }

    let output = output.unwrap_or_else(|| target_dir.join(DEFAULT_INITRD));
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let busybox = include_busybox
        .then(|| build_busybox(&target_dir, target, features))
        .transpose()?;
    write_initrd(&init, busybox.as_ref(), modules, &output)?;
    Ok(output)
}

pub(super) fn build_release(
    workspace_root: &Path,
    target: &str,
    features: &FeatureSet,
) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(workspace_root).args([
        "build",
        "--release",
        "--target",
        target,
        "-p",
        INIT_BINARY,
    ]);
    if !features.is_empty() {
        command.arg("--features").arg(features.cargo_value());
    }

    let status = command
        .status()
        .map_err(|err| format!("spawn cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }
    Ok(())
}

fn write_initrd(
    init: &Path,
    busybox: Option<&BusyBoxInstall>,
    modules: Option<&ModuleArchive>,
    output: &Path,
) -> Result<()> {
    let tmp = output.with_extension("cpio.tmp");
    let mut writer = NewcWriter::create(&tmp)?;
    let mut dirs = BTreeSet::new();
    writer.dir("dev", 0o755)?;
    dirs.insert("dev".to_string());
    writer.char_dev("dev/console", 0o600, 5, 1)?;
    writer.char_dev("dev/kmsg", 0o600, 1, 11)?;
    writer.char_dev("dev/null", 0o666, 1, 3)?;
    writer.dir("etc", 0o755)?;
    dirs.insert("etc".to_string());
    writer.dir("proc", 0o755)?;
    dirs.insert("proc".to_string());
    writer.dir("run", 0o755)?;
    dirs.insert("run".to_string());
    writer.dir("sys", 0o755)?;
    dirs.insert("sys".to_string());
    writer.dir("tmp", 0o1777)?;
    dirs.insert("tmp".to_string());
    if let Some(busybox) = busybox {
        writer.tree(&busybox.root)?;
        println!(
            "busybox {} ({} bytes)",
            busybox.binary.display(),
            fs::metadata(&busybox.binary)
                .map_err(|err| format!("stat {}: {err}", busybox.binary.display()))?
                .len()
        );
    }
    if let Some(modules) = modules {
        writer.modules(modules, &mut dirs)?;
    }
    writer.file("init", init, 0o755)?;
    writer.finish()?;

    if same_contents(&tmp, output)? {
        fs::remove_file(&tmp).map_err(|err| format!("remove {}: {err}", tmp.display()))?;
    } else {
        fs::rename(&tmp, output)
            .map_err(|err| format!("rename {} to {}: {err}", tmp.display(), output.display()))?;
    }
    Ok(())
}

#[derive(Debug)]
struct ModuleArchive {
    entries: Vec<ModuleEntry>,
    manifest: String,
}

#[derive(Debug)]
struct ModuleEntry {
    source: PathBuf,
    cpio_name: String,
}

impl ModuleArchive {
    fn for_bootmenu(
        target_dir: &Path,
        device: &KernelDevice,
        bootmenu: &BootMenuConfig,
    ) -> Result<Option<Self>> {
        if bootmenu.modules.is_empty() {
            return Ok(None);
        }

        let installed = InstalledModules::load(target_dir, device)?;
        let modules = installed.resolve_modules(&bootmenu.modules)?;
        let mut manifest = String::new();
        let mut entries = Vec::new();
        for module in modules {
            let module_name = module
                .to_str()
                .ok_or_else(|| format!("module path is not valid UTF-8: {}", module.display()))?;
            let cpio_name = format!("lib/modules/{}/{module_name}", installed.release);
            manifest.push('/');
            manifest.push_str(&cpio_name);
            manifest.push('\n');
            entries.push(ModuleEntry {
                source: installed.release_dir.join(module),
                cpio_name,
            });
        }

        Ok(Some(Self { entries, manifest }))
    }
}

#[derive(Debug)]
struct InstalledModules {
    release: String,
    release_dir: PathBuf,
    deps: BTreeMap<PathBuf, Vec<PathBuf>>,
    names: BTreeMap<String, PathBuf>,
}

impl InstalledModules {
    fn load(target_dir: &Path, device: &KernelDevice) -> Result<Self> {
        let modules_root = target_dir
            .join("kernel")
            .join(&device.vendor)
            .join(&device.stem)
            .join("modules/lib/modules");
        let releases = module_release_dirs(&modules_root)?;
        let [(release, release_dir)] = releases.as_slice() else {
            return Err(format!(
                "expected exactly one installed kernel module release under {}, found {}",
                modules_root.display(),
                releases.len()
            ));
        };
        let release = release.clone();
        let release_dir = release_dir.clone();
        let modules_dep = release_dir.join("modules.dep");
        let contents = fs::read_to_string(&modules_dep)
            .map_err(|err| format!("read {}: {err}", modules_dep.display()))?;
        let mut deps = BTreeMap::new();
        let mut names = BTreeMap::new();

        for (line_index, line) in contents.lines().enumerate() {
            let (module, module_deps) = line.split_once(':').ok_or_else(|| {
                format!(
                    "{}:{}: invalid modules.dep line",
                    modules_dep.display(),
                    line_index + 1
                )
            })?;
            let module = module_path(module, &release_dir)?;
            let module_deps = module_deps
                .split_whitespace()
                .map(|dep| module_path(dep, &release_dir))
                .collect::<Result<Vec<_>>>()?;
            let name = module_name(&module).ok_or_else(|| {
                format!(
                    "installed module path has no .ko suffix: {}",
                    module.display()
                )
            })?;
            if let Some(existing) = names.insert(name.clone(), module.clone()) {
                return Err(format!(
                    "duplicate installed module name {name}: {} and {}",
                    existing.display(),
                    module.display()
                ));
            }
            deps.insert(module, module_deps);
        }

        Ok(Self {
            release,
            release_dir,
            deps,
            names,
        })
    }

    fn resolve_modules(&self, modules: &[String]) -> Result<Vec<PathBuf>> {
        let mut resolved = Vec::new();
        let mut visited = BTreeSet::new();
        for module in modules {
            let path = self.module_by_name(module)?;
            self.resolve_module_path(&path, &mut visited, &mut resolved)?;
        }
        Ok(resolved)
    }

    fn module_by_name(&self, module: &str) -> Result<PathBuf> {
        if let Some(path) = self.names.get(module) {
            return Ok(path.clone());
        }

        let normalized = normalized_module_name(module);
        let matches = self
            .names
            .iter()
            .filter(|(name, _)| normalized_module_name(name) == normalized)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [(_, path)] => Ok((*path).clone()),
            [] => Err(format!(
                "requested bootmenu module was not installed: {module}"
            )),
            matches => Err(format!(
                "requested bootmenu module name is ambiguous: {module} matches {} modules",
                matches.len()
            )),
        }
    }

    fn resolve_module_path(
        &self,
        module: &PathBuf,
        visited: &mut BTreeSet<PathBuf>,
        resolved: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if visited.contains(module) {
            return Ok(());
        }
        let deps = self.deps.get(module).ok_or_else(|| {
            format!(
                "installed module is missing from modules.dep: {}",
                module.display()
            )
        })?;
        for dep in deps {
            self.resolve_module_path(dep, visited, resolved)?;
        }
        visited.insert(module.clone());
        resolved.push(module.clone());
        Ok(())
    }
}

fn module_release_dirs(modules_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = fs::read_dir(modules_root)
        .map_err(|err| format!("read {}: {err}", modules_root.display()))?;
    let mut releases = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("read {} entry: {err}", modules_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let release = entry
            .file_name()
            .into_string()
            .map_err(|name| format!("kernel release is not valid UTF-8: {name:?}"))?;
        releases.push((release, path));
    }
    releases.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(releases)
}

fn module_path(value: &str, release_dir: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("invalid module path in modules.dep: {value}"));
    }
    let source = release_dir.join(&path);
    if !source.is_file() {
        return Err(format!(
            "missing installed module file: {}",
            source.display()
        ));
    }
    Ok(path)
}

fn module_name(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    for suffix in [".ko.xz", ".ko.gz", ".ko.zst", ".ko"] {
        if let Some(name) = file_name.strip_suffix(suffix) {
            return Some(name.to_string());
        }
    }
    None
}

fn normalized_module_name(name: &str) -> String {
    name.replace('-', "_")
}

fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::read(left).map_err(|err| format!("read {}: {err}", left.display()))?;
    let right = match fs::read(right) {
        Ok(right) => right,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("read {}: {err}", right.display())),
    };
    Ok(left == right)
}

struct NewcWriter {
    file: File,
    offset: u64,
    ino: u32,
}

impl NewcWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|err| format!("create {}: {err}", path.display()))?;
        Ok(Self {
            file,
            offset: 0,
            ino: 1,
        })
    }

    fn dir(&mut self, name: &str, mode: u32) -> Result<()> {
        self.entry(name, 0o040000 | mode, 2, 0, 0, &[])
    }

    fn file(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.file_bytes(name, &contents, mode)
    }

    fn file_with_mode(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.file_bytes(name, &contents, mode)
    }

    fn file_bytes(&mut self, name: &str, contents: &[u8], mode: u32) -> Result<()> {
        self.entry(name, 0o100000 | mode, 1, 0, 0, contents)
    }

    fn symlink(&mut self, name: &str, target: &Path) -> Result<()> {
        self.entry(
            name,
            0o120000 | 0o777,
            1,
            0,
            0,
            target.as_os_str().as_bytes(),
        )
    }

    fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) -> Result<()> {
        self.entry(name, 0o020000 | mode, 1, major, minor, &[])
    }

    fn tree(&mut self, root: &Path) -> Result<()> {
        self.tree_dir(root, root)
    }

    fn modules(&mut self, modules: &ModuleArchive, dirs: &mut BTreeSet<String>) -> Result<()> {
        for entry in &modules.entries {
            self.ensure_parent_dirs(&entry.cpio_name, dirs)?;
            self.file(&entry.cpio_name, &entry.source, 0o644)?;
        }

        self.ensure_parent_dirs(BOOTMENU_MODULE_MANIFEST, dirs)?;
        self.file_bytes(BOOTMENU_MODULE_MANIFEST, modules.manifest.as_bytes(), 0o644)?;
        Ok(())
    }

    fn ensure_parent_dirs(&mut self, name: &str, dirs: &mut BTreeSet<String>) -> Result<()> {
        let mut current = String::new();
        let mut parts = name.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            if dirs.insert(current.clone()) {
                self.dir(&current, 0o755)?;
            }
        }
        Ok(())
    }

    fn tree_dir(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .map_err(|err| format!("read directory {}: {err}", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| format!("read directory entry under {}: {err}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|err| {
                format!(
                    "strip {} prefix from {}: {err}",
                    root.display(),
                    path.display()
                )
            })?;
            let name = cpio_name(relative)?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|err| format!("stat {}: {err}", path.display()))?;
            let mode = metadata.permissions().mode() & 0o7777;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                self.dir(&name, mode)?;
                self.tree_dir(root, &path)?;
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|err| format!("readlink {}: {err}", path.display()))?;
                self.symlink(&name, &target)?;
            } else if file_type.is_file() {
                self.file_with_mode(&name, &path, mode)?;
            } else {
                return Err(format!(
                    "unsupported file type in initrd tree: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.entry("TRAILER!!!", 0, 1, 0, 0, &[])?;
        self.file
            .flush()
            .map_err(|err| format!("flush initrd: {err}"))
    }

    fn entry(
        &mut self,
        name: &str,
        mode: u32,
        nlink: u32,
        rdevmajor: u32,
        rdevminor: u32,
        contents: &[u8],
    ) -> Result<()> {
        if name.starts_with('/') {
            return Err(format!("cpio entry must be relative: {name}"));
        }

        let namesize = name.len() + 1;
        let filesize =
            u32::try_from(contents.len()).map_err(|_| format!("cpio entry too large: {name}"))?;
        let header = format!(
            "070701{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
            ino = self.ino,
            mode = mode,
            uid = 0,
            gid = 0,
            nlink = nlink,
            mtime = source_date_epoch(),
            filesize = filesize,
            devmajor = 0,
            devminor = 0,
            rdevmajor = rdevmajor,
            rdevminor = rdevminor,
            namesize = namesize,
            check = 0,
        );

        self.write_all(header.as_bytes())?;
        self.write_all(name.as_bytes())?;
        self.write_all(&[0])?;
        self.pad_to_4()?;
        self.write_all(contents)?;
        self.pad_to_4()?;
        self.ino = self.ino.wrapping_add(1);
        Ok(())
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .map_err(|err| format!("write initrd: {err}"))?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn pad_to_4(&mut self) -> Result<()> {
        let pad = (4 - (self.offset % 4)) % 4;
        if pad != 0 {
            self.write_all(&vec![0; pad as usize])?;
        }
        Ok(())
    }
}

fn source_date_epoch() -> u32 {
    env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn cpio_name(path: &Path) -> Result<String> {
    let name = path
        .to_str()
        .ok_or_else(|| format!("cpio path is not valid UTF-8: {}", path.display()))?;
    if name.is_empty()
        || name.starts_with('/')
        || name.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        Err(format!("invalid cpio path: {name}"))
    } else {
        Ok(name.to_string())
    }
}
