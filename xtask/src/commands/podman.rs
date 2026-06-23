use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

use crate::Result;

use super::run_command;

const WORKSPACE_MOUNT: &str = "/work";
const EXTRA_MOUNT_ROOT: &str = "/mnt/pocketboot";
const DEFAULT_REMOTE_IMAGE_REPO: &str = "ghcr.io/samcday/pocketboot-ci";
const LOCAL_IMAGE_REPO: &str = "localhost/pocketboot-ci";

#[derive(Debug)]
pub(super) struct BindMount {
    source: PathBuf,
    target: PathBuf,
    read_only: bool,
}

#[derive(Debug)]
pub(super) struct PathMapper {
    workspace_root: PathBuf,
    mounts: Vec<BindMount>,
}

impl PathMapper {
    pub(super) fn new(workspace_root: &Path) -> Result<Self> {
        let workspace_root = fs::canonicalize(workspace_root).map_err(|err| {
            format!(
                "canonicalize workspace root {}: {err}",
                workspace_root.display()
            )
        })?;
        Ok(Self {
            workspace_root,
            mounts: Vec::new(),
        })
    }

    pub(super) fn map_existing_dir(&mut self, path: &Path, description: &str) -> Result<PathBuf> {
        let path = fs::canonicalize(path)
            .map_err(|err| format!("canonicalize {description} {}: {err}", path.display()))?;
        if !path.is_dir() {
            return Err(format!(
                "{description} is not a directory: {}",
                path.display()
            ));
        }
        self.map_canonical_path(&path, false)
    }

    pub(super) fn map_existing_file(&mut self, path: &Path, description: &str) -> Result<PathBuf> {
        let path = fs::canonicalize(path)
            .map_err(|err| format!("canonicalize {description} {}: {err}", path.display()))?;
        if !path.is_file() {
            return Err(format!("{description} is not a file: {}", path.display()));
        }
        self.map_canonical_file_path(&path, true)
    }

    pub(super) fn map_output_path(&mut self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        if let Ok(relative) = path.strip_prefix(&self.workspace_root) {
            return Ok(Path::new(WORKSPACE_MOUNT).join(relative));
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| format!("output path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
        let parent = fs::canonicalize(parent)
            .map_err(|err| format!("canonicalize output parent {}: {err}", parent.display()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| format!("output path has no file name: {}", path.display()))?;
        Ok(self.add_mount(parent, false).join(file_name))
    }

    pub(super) fn into_mounts(self) -> Vec<BindMount> {
        self.mounts
    }

    fn map_canonical_path(&mut self, path: &Path, read_only: bool) -> Result<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.workspace_root) {
            return Ok(Path::new(WORKSPACE_MOUNT).join(relative));
        }
        Ok(self.add_mount(path.to_path_buf(), read_only))
    }

    fn map_canonical_file_path(&mut self, path: &Path, read_only: bool) -> Result<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.workspace_root) {
            return Ok(Path::new(WORKSPACE_MOUNT).join(relative));
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| format!("path has no file name: {}", path.display()))?;
        Ok(self
            .add_mount(parent.to_path_buf(), read_only)
            .join(file_name))
    }

    fn add_mount(&mut self, source: PathBuf, read_only: bool) -> PathBuf {
        if let Some(mount) = self.mounts.iter_mut().find(|mount| mount.source == source) {
            if !read_only {
                mount.read_only = false;
            }
            return mount.target.clone();
        }

        let target = Path::new(EXTRA_MOUNT_ROOT).join(self.mounts.len().to_string());
        self.mounts.push(BindMount {
            source,
            target: target.clone(),
            read_only,
        });
        target
    }
}

pub(super) fn run_xtask(
    workspace_root: &Path,
    args: impl IntoIterator<Item = OsString>,
    mounts: Vec<BindMount>,
) -> Result<()> {
    let image = ensure_image(workspace_root)?;
    let workspace_root = fs::canonicalize(workspace_root).map_err(|err| {
        format!(
            "canonicalize workspace root {}: {err}",
            workspace_root.display()
        )
    })?;
    let target_dir = workspace_root.join("target");
    fs::create_dir_all(target_dir.join("podman/home"))
        .map_err(|err| format!("create podman home: {err}"))?;
    fs::create_dir_all(target_dir.join("podman/cargo-home"))
        .map_err(|err| format!("create podman cargo home: {err}"))?;

    let mut command = Command::new(podman());
    command
        .args(["run", "--rm"])
        .arg("--security-opt")
        .arg("label=disable")
        .arg("--workdir")
        .arg(WORKSPACE_MOUNT)
        .arg("--mount")
        .arg(bind_mount_arg(
            &workspace_root,
            Path::new(WORKSPACE_MOUNT),
            false,
        ))
        .args([
            "--env",
            "CARGO_TERM_COLOR=always",
            "--env",
            "CARGO_TARGET_DIR=/work/target",
            "--env",
            "CARGO_HOME=/work/target/podman/cargo-home",
            "--env",
            "HOME=/work/target/podman/home",
            "--env",
            "CCACHE_BASEDIR=/work",
            "--env",
            "CCACHE_COMPILERCHECK=content",
            "--env",
            "CCACHE_DIR=/work/target/ccache",
            "--env",
            "CCACHE_MAXSIZE=2G",
            "--env",
            "HOSTCC=ccache gcc",
        ]);

    for name in ["SOURCE_DATE_EPOCH", "RUST_LOG"] {
        if env::var_os(name).is_some() {
            command.arg("--env").arg(name);
        }
    }

    for mount in mounts {
        command.arg("--mount").arg(bind_mount_arg(
            &mount.source,
            &mount.target,
            mount.read_only,
        ));
    }

    command.arg(image).arg("cargo").arg("xtask").args(args);
    run_command(command, "podman run xtask")
}

fn ensure_image(workspace_root: &Path) -> Result<String> {
    if let Some(image) = env_string("POCKETBOOT_PODMAN_IMAGE") {
        println!("podman image {image}");
        return Ok(image);
    }

    let hash = dockerfile_hash(workspace_root)?;
    let remote_repo = env_string("POCKETBOOT_PODMAN_REMOTE_REPO")
        .unwrap_or_else(|| DEFAULT_REMOTE_IMAGE_REPO.to_string());
    let remote_image = format!("{remote_repo}:df-{hash}");
    let local_image = format!("{LOCAL_IMAGE_REPO}:df-{hash}");

    if image_exists(&local_image)? {
        println!("podman image {local_image}");
        return Ok(local_image);
    }

    if image_exists(&remote_image)? || pull_image(&remote_image)? {
        println!("podman image {remote_image}");
        return Ok(remote_image);
    }

    build_image(workspace_root, &local_image)?;
    println!("podman image {local_image}");
    Ok(local_image)
}

fn dockerfile_hash(workspace_root: &Path) -> Result<String> {
    let path = workspace_root.join(".github/Dockerfile");
    let mut file = File::open(&path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn image_exists(image: &str) -> Result<bool> {
    let status = Command::new(podman())
        .args(["image", "exists", image])
        .status()
        .map_err(|err| format!("spawn podman image exists: {err}"))?;
    Ok(status.success())
}

fn pull_image(image: &str) -> Result<bool> {
    println!("pulling podman image {image}");
    let status = Command::new(podman())
        .args(["pull", image])
        .status()
        .map_err(|err| format!("spawn podman pull: {err}"))?;
    Ok(status.success())
}

fn build_image(workspace_root: &Path, image: &str) -> Result<()> {
    println!("building podman image {image}");
    let mut command = Command::new(podman());
    command.current_dir(workspace_root).args([
        "build",
        "--layers",
        "--tag",
        image,
        "--file",
        ".github/Dockerfile",
    ]);
    for cache in env_list("POCKETBOOT_PODMAN_CACHE_FROM") {
        command.arg("--cache-from").arg(cache);
    }
    command.arg(".");
    run_command(command, "podman build CI image")
}

fn bind_mount_arg(source: &Path, target: &Path, read_only: bool) -> OsString {
    let mut arg = OsString::from("type=bind,src=");
    arg.push(source.as_os_str());
    arg.push(OsStr::new(",target="));
    arg.push(target.as_os_str());
    if read_only {
        arg.push(OsStr::new(",readonly"));
    }
    arg
}

fn env_string(name: &str) -> Option<String> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
}

fn env_list(name: &str) -> Vec<String> {
    env_string(name)
        .map(|value| {
            value
                .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn podman() -> OsString {
    env::var_os("PODMAN").unwrap_or_else(|| "podman".into())
}
