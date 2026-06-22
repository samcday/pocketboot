use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{
    KernelDevice,
    config::{self, KernelSource, KernelSourceScope},
    run_command, target_dir, workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct KernelSrcArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
}

#[derive(Debug)]
struct KernelSourceIdentity {
    name: String,
    path: PathBuf,
}

pub(super) struct KernelSourceTree {
    pub(super) path: PathBuf,
    pub(super) sha: String,
    pub(super) status: KernelSourceStatus,
}

pub(super) enum KernelSourceStatus {
    Current,
    Updated,
}

pub(crate) fn run(args: KernelSrcArgs) -> Result<()> {
    kernel_src(args)
}

fn kernel_src(args: KernelSrcArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let tree = ensure_device_kernel_source(&workspace_root, &args.device)?;

    match tree.status {
        KernelSourceStatus::Current => println!("kernel source current {}", tree.path.display()),
        KernelSourceStatus::Updated => println!("kernel source updated {}", tree.path.display()),
    }
    println!("sha {}", tree.sha);
    Ok(())
}

pub(super) fn ensure_device_kernel_source(
    workspace_root: &Path,
    device: &KernelDevice,
) -> Result<KernelSourceTree> {
    let config = config::load_device_config(workspace_root, device)?;
    let source = config.kernel_source.as_ref().ok_or_else(|| {
        format!(
            "no [kernel-source] configured for {}/{}",
            device.vendor, device.stem
        )
    })?;
    let identity = kernel_source_identity(device, source);
    let source_tree = target_dir(workspace_root)
        .join("kernel")
        .join("src")
        .join(&identity.path);
    let status = ensure_kernel_source(&workspace_root, &source_tree, &identity, source)?;

    Ok(KernelSourceTree {
        path: source_tree,
        sha: source.sha.clone(),
        status,
    })
}

pub(super) fn ensure_named_kernel_source(
    workspace_root: &Path,
    name: &str,
    path: PathBuf,
    source: &KernelSource,
) -> Result<KernelSourceTree> {
    let identity = KernelSourceIdentity {
        name: name.to_string(),
        path,
    };
    let source_tree = target_dir(workspace_root)
        .join("kernel")
        .join("src")
        .join(&identity.path);
    let status = ensure_kernel_source(&workspace_root, &source_tree, &identity, source)?;

    Ok(KernelSourceTree {
        path: source_tree,
        sha: source.sha.clone(),
        status,
    })
}

fn kernel_source_identity(device: &KernelDevice, source: &KernelSource) -> KernelSourceIdentity {
    match source.scope {
        KernelSourceScope::Default => KernelSourceIdentity {
            name: "pocketboot".to_string(),
            path: PathBuf::from("pocketboot"),
        },
        KernelSourceScope::Soc => KernelSourceIdentity {
            name: device.soc.clone(),
            path: PathBuf::from(&device.soc),
        },
        KernelSourceScope::Device => KernelSourceIdentity {
            name: device.stem.clone(),
            path: PathBuf::from(&device.soc).join(&device.stem),
        },
    }
}

fn ensure_kernel_source(
    workspace_root: &Path,
    source_tree: &Path,
    identity: &KernelSourceIdentity,
    source: &KernelSource,
) -> Result<KernelSourceStatus> {
    if let Some(head) = existing_source_head(source_tree)? {
        if head.eq_ignore_ascii_case(&source.sha) {
            return Ok(KernelSourceStatus::Current);
        }
    }

    match kernel_repo(workspace_root)? {
        Some(repo) => setup_worktree_source(&repo, source_tree, identity, source)?,
        None => setup_direct_source(source_tree, source)?,
    }
    verify_source_head(source_tree, &source.sha)?;
    Ok(KernelSourceStatus::Updated)
}

fn setup_worktree_source(
    repo: &Path,
    source_tree: &Path,
    identity: &KernelSourceIdentity,
    source: &KernelSource,
) -> Result<()> {
    let remote_name = remote_name(&identity.name);
    ensure_remote(repo, &remote_name, &source.remote, false)?;
    fetch_remote(repo, &remote_name, &source.sha)?;

    if path_exists(source_tree)? {
        ensure_clean_source(source_tree)?;
        fetch_url(source_tree, &source.remote, &source.sha)?;
        checkout_fetch_head(source_tree)?;
        return Ok(());
    }

    create_parent_dir(source_tree)?;
    let mut command = git_at(repo);
    command
        .args(["worktree", "add", "--detach"])
        .arg(source_tree)
        .arg(&source.sha);
    run_command(command, "add kernel source worktree")
}

fn setup_direct_source(source_tree: &Path, source: &KernelSource) -> Result<()> {
    if path_exists(source_tree)? {
        ensure_git_work_tree(source_tree)?;
        ensure_clean_source(source_tree)?;
    } else {
        create_parent_dir(source_tree)?;
        let mut command = Command::new("git");
        command.arg("init").arg(source_tree);
        run_command(command, "init kernel source repository")?;
    }

    ensure_remote(source_tree, "origin", &source.remote, true)?;
    fetch_remote(source_tree, "origin", &source.sha)?;
    checkout_fetch_head(source_tree)
}

fn existing_source_head(source_tree: &Path) -> Result<Option<String>> {
    if !path_exists(source_tree)? {
        return Ok(None);
    }
    ensure_git_work_tree(source_tree)?;
    current_head(source_tree)
}

fn verify_source_head(source_tree: &Path, expected: &str) -> Result<()> {
    let head = current_head(source_tree)?.ok_or_else(|| {
        format!(
            "kernel source has no HEAD after setup: {}",
            source_tree.display()
        )
    })?;
    if head.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "kernel source HEAD is {head}, expected {expected}: {}",
            source_tree.display()
        ))
    }
}

fn kernel_repo(workspace_root: &Path) -> Result<Option<PathBuf>> {
    let repo = workspace_root.join("kernel");
    if !path_exists(&repo)? {
        return Ok(None);
    }
    if is_git_repo(&repo)? {
        Ok(Some(repo))
    } else {
        Err(format!(
            "top-level kernel path exists but is not a git repository: {}",
            repo.display()
        ))
    }
}

fn remote_name(name: &str) -> String {
    if name == "pocketboot" {
        "pocketboot".to_string()
    } else {
        format!("pocketboot-{name}")
    }
}

fn ensure_remote(repo: &Path, name: &str, url: &str, update_existing: bool) -> Result<()> {
    match remote_url(repo, name)? {
        Some(existing) if existing == url => Ok(()),
        Some(_) if update_existing => {
            let mut command = git_at(repo);
            command.args(["remote", "set-url", name, url]);
            run_command(command, "update kernel source remote")
        }
        Some(existing) => Err(format!(
            "git remote {name} already points at {existing}, expected {url}: {}",
            repo.display()
        )),
        None => {
            let mut command = git_at(repo);
            command.args(["remote", "add", name, url]);
            run_command(command, "add kernel source remote")
        }
    }
}

fn remote_url(repo: &Path, name: &str) -> Result<Option<String>> {
    let mut command = git_at(repo);
    command.args(["remote", "get-url", name]);
    let output = command
        .output()
        .map_err(|err| format!("spawn git remote get-url: {err}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(stdout(output.stdout, "git remote get-url")?))
}

fn fetch_remote(repo: &Path, remote: &str, sha: &str) -> Result<()> {
    let mut command = git_at(repo);
    command.args(["fetch", "--depth=1", remote, sha]);
    run_command(command, "fetch kernel source")
}

fn fetch_url(repo: &Path, url: &str, sha: &str) -> Result<()> {
    let mut command = git_at(repo);
    command.args(["fetch", "--depth=1", url, sha]);
    run_command(command, "fetch kernel source")
}

fn checkout_fetch_head(repo: &Path) -> Result<()> {
    let mut command = git_at(repo);
    command.args(["checkout", "--detach", "FETCH_HEAD"]);
    run_command(command, "checkout kernel source")
}

fn ensure_clean_source(repo: &Path) -> Result<()> {
    let mut command = git_at(repo);
    command.args(["status", "--porcelain"]);
    let output = command
        .output()
        .map_err(|err| format!("spawn git status: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed with {}: {}",
            output.status,
            repo.display()
        ));
    }
    let status = stdout(output.stdout, "git status")?;
    if status.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "kernel source has uncommitted changes; refusing to update: {}",
            repo.display()
        ))
    }
}

fn current_head(repo: &Path) -> Result<Option<String>> {
    let mut command = git_at(repo);
    command.args(["rev-parse", "--verify", "HEAD"]);
    let output = command
        .output()
        .map_err(|err| format!("spawn git rev-parse HEAD: {err}"))?;
    if output.status.success() {
        Ok(Some(stdout(output.stdout, "git rev-parse HEAD")?))
    } else {
        Ok(None)
    }
}

fn ensure_git_work_tree(repo: &Path) -> Result<()> {
    if is_git_work_tree(repo)? {
        Ok(())
    } else {
        Err(format!(
            "kernel source exists but is not a git worktree: {}",
            repo.display()
        ))
    }
}

fn is_git_repo(repo: &Path) -> Result<bool> {
    let mut command = git_at(repo);
    command.args(["rev-parse", "--git-dir"]);
    let output = command
        .output()
        .map_err(|err| format!("spawn git rev-parse --git-dir: {err}"))?;
    Ok(output.status.success())
}

fn is_git_work_tree(repo: &Path) -> Result<bool> {
    let mut command = git_at(repo);
    command.args(["rev-parse", "--is-inside-work-tree"]);
    let output = command
        .output()
        .map_err(|err| format!("spawn git rev-parse --is-inside-work-tree: {err}"))?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(stdout(output.stdout, "git rev-parse --is-inside-work-tree")? == "true")
}

fn git_at(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("stat {}: {err}", path.display())),
    }
}

fn create_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    Ok(())
}

fn stdout(bytes: Vec<u8>, action: &str) -> Result<String> {
    String::from_utf8(bytes)
        .map(|value| value.trim().to_string())
        .map_err(|err| format!("decode {action} output: {err}"))
}
