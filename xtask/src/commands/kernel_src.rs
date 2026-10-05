use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use crate::Result;

use super::{
    KernelDevice,
    config::{self, KernelSource, KernelSourceIdentity},
    has_linux_readme, kernel_tree, run_command, target_dir, workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct KernelSrcArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: Option<PathBuf>,
}

struct KernelSourceTree {
    path: PathBuf,
    sha: String,
    status: KernelSourceStatus,
}

enum KernelSourceStatus {
    Current,
    Updated,
}

pub(crate) fn run(args: KernelSrcArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    resolve_kernel_tree(&workspace_root, &args.device, args.kernel_tree.as_deref())?;
    Ok(())
}

pub(super) fn resolve_kernel_tree(
    workspace_root: &Path,
    device: &KernelDevice,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    let cdpath = env::var_os("DELTA_DATABASE_DIR").and_then(|_| env::var_os("CDPATH"));
    if let Some((origin, path)) = override_kernel_tree(explicit, cdpath.as_deref())? {
        println!(
            "kernel source {} ({origin}; configured patches bypassed)",
            path.display()
        );
        return Ok(path);
    }

    let tree = ensure_device_kernel_source(workspace_root, device)?;
    let status = match tree.status {
        KernelSourceStatus::Current => "current",
        KernelSourceStatus::Updated => "updated",
    };
    println!(
        "kernel source {status} {} (configured)",
        tree.path.display()
    );
    println!("sha {}", tree.sha);
    kernel_tree(&tree.path)
}

fn override_kernel_tree(
    explicit: Option<&Path>,
    delta_cdpath: Option<&OsStr>,
) -> Result<Option<(&'static str, PathBuf)>> {
    if let Some(path) = explicit {
        return Ok(Some(("explicit", kernel_tree(path)?)));
    }
    let Some(cdpath) = delta_cdpath else {
        return Ok(None);
    };

    // Delta exports attached checkout parents in CDPATH. Ignore the shell's
    // empty/relative entries; inspect only direct children, regardless of name.
    let mut candidates = BTreeSet::new();
    for parent in env::split_paths(cdpath).filter(|path| path.is_absolute()) {
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("read Delta CDPATH {}: {err}", parent.display())),
        };
        for entry in entries {
            let path = entry
                .map_err(|err| format!("read Delta CDPATH {} entry: {err}", parent.display()))?
                .path();
            if has_linux_readme(&path) {
                candidates.insert(kernel_tree(&path)?);
            }
        }
    }

    if candidates.len() > 1 {
        return Err(format!(
            "multiple kernel trees in Delta CDPATH: {}; pass KERNEL_TREE explicitly",
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(candidates
        .into_iter()
        .next()
        .map(|path| ("Delta CDPATH", path)))
}

fn ensure_device_kernel_source(
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
    let source_tree = target_dir(workspace_root)
        .join("kernel")
        .join("src")
        .join(&source.identity.tree_path);
    let mut status = ensure_kernel_source(workspace_root, &source_tree, &source.identity, source)?;
    if ensure_kernel_patches(workspace_root, &source_tree, &source.patches)? {
        status = KernelSourceStatus::Updated;
    }

    Ok(KernelSourceTree {
        path: source_tree,
        sha: source.sha.clone(),
        status,
    })
}

// Feed the series to one git-apply invocation as a single patch stream. Separate
// file arguments do not share dry-run state and can partially modify the tree
// if a later patch fails. One stream supports dependent patches and Git checks
// all of its changes before writing any files. Never reset local source edits.
fn ensure_kernel_patches(
    workspace_root: &Path,
    source_tree: &Path,
    patches: &[PathBuf],
) -> Result<bool> {
    if patches.is_empty() {
        return Ok(false);
    }
    let root = fs::canonicalize(workspace_root)
        .map_err(|err| format!("resolve kernel patch root: {err}"))?;
    let mut series = Vec::new();
    for path in patches {
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(format!(
                "kernel patch must be relative to workspace: {}",
                path.display()
            ));
        }
        let resolved = fs::canonicalize(root.join(path))
            .map_err(|err| format!("resolve kernel patch {}: {err}", path.display()))?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err(format!(
                "kernel patch is not a file inside workspace: {}",
                path.display()
            ));
        }
        let contents = fs::read(&resolved)
            .map_err(|err| format!("read kernel patch {}: {err}", path.display()))?;
        if contents.is_empty() {
            return Err(format!("kernel patch is empty: {}", path.display()));
        }
        series.extend_from_slice(&contents);
        if series.last() != Some(&b'\n') {
            series.push(b'\n');
        }
    }

    let forward = git_apply_series(source_tree, &series, &["--check"])?;
    if forward.status.success() {
        let applied = git_apply_series(source_tree, &series, &[])?;
        if !applied.status.success() {
            return Err(format!(
                "apply configured kernel patches at {}: {}",
                source_tree.display(),
                String::from_utf8_lossy(&applied.stderr)
            ));
        }
        return Ok(true);
    }
    // Git reverses successive changes to the same file within one stream.
    if git_apply_series(source_tree, &series, &["--check", "--reverse"])?
        .status
        .success()
    {
        return Ok(false);
    }
    Err(format!(
        "configured kernel patches neither apply nor match the existing source at {}; resolve the source edits or use a fresh tree:\n{}",
        source_tree.display(),
        String::from_utf8_lossy(&forward.stderr)
    ))
}

fn git_apply_series(source_tree: &Path, series: &[u8], flags: &[&str]) -> Result<Output> {
    let mut command = git_at(source_tree);
    command
        .arg("apply")
        .args(flags)
        .args(["--", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| format!("spawn git apply for kernel patches: {err}"))?;
    let write_result = child.stdin.take().unwrap().write_all(series);
    let output = child
        .wait_with_output()
        .map_err(|err| format!("wait for git apply for kernel patches: {err}"))?;
    if output.status.success() {
        write_result.map_err(|err| format!("write kernel patch stream: {err}"))?;
    }
    Ok(output)
}

fn ensure_kernel_source(
    workspace_root: &Path,
    source_tree: &Path,
    identity: &KernelSourceIdentity,
    source: &KernelSource,
) -> Result<KernelSourceStatus> {
    if let Some(head) = existing_source_head(source_tree)?
        && head.eq_ignore_ascii_case(&source.sha)
    {
        return Ok(KernelSourceStatus::Current);
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
    let remote_name = remote_name(&identity.tree_name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct PatchFixture {
        root: PathBuf,
        source: PathBuf,
    }

    impl PatchFixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "pocketboot-kernel-patches-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let source = root.join("source");
            fs::create_dir_all(&source).unwrap();
            assert!(
                Command::new("git")
                    .args(["init", "--quiet"])
                    .arg(&source)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(source.join("core"), "one\n").unwrap();
            fs::write(source.join("unrelated"), "original\n").unwrap();
            assert!(
                git_at(&source)
                    .args(["add", "."])
                    .status()
                    .unwrap()
                    .success()
            );
            // Preserve both the user's dirty worktree and existing index.
            fs::write(source.join("unrelated"), "local work\n").unwrap();
            Self { root, source }
        }

        fn patch(&self, name: &str, file: &str, before: &str, after: &str) -> PathBuf {
            let path = PathBuf::from(name);
            fs::write(
                self.root.join(&path),
                format!(
                    "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-{before}\n+{after}\n"
                ),
            )
            .unwrap();
            path
        }

        fn read(&self, file: &str) -> String {
            fs::read_to_string(self.source.join(file)).unwrap()
        }

        fn kernel(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::create_dir_all(path.join("scripts/kconfig")).unwrap();
            fs::write(path.join("README"), "Linux kernel\n============\n").unwrap();
            fs::write(path.join("Makefile"), "").unwrap();
            fs::write(path.join("scripts/kconfig/merge_config.sh"), "").unwrap();
            fs::canonicalize(path).unwrap()
        }
    }

    impl Drop for PatchFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn absent_delta_cdpath_has_no_override() {
        assert!(override_kernel_tree(None, None).unwrap().is_none());
    }

    #[test]
    fn delta_discovers_kernel_without_modifying_source_or_writing_marker() {
        let f = PatchFixture::new();
        let tree = f.kernel("source");
        let index = fs::read(tree.join(".git/index")).unwrap();
        assert_eq!(
            override_kernel_tree(None, Some(f.root.as_os_str())).unwrap(),
            Some(("Delta CDPATH", tree.clone()))
        );
        assert_eq!(f.read("unrelated"), "local work\n");
        assert_eq!(fs::read(tree.join(".git/index")).unwrap(), index);
        assert!(!f.root.join(".localkernel").exists());
    }

    #[test]
    fn delta_deduplicates_search_roots_and_preserves_spaces() {
        let f = PatchFixture::new();
        let tree = f.kernel("parent with spaces/kernel with spaces");
        let parent = tree.parent().unwrap();
        let cdpath = env::join_paths([parent, parent]).unwrap();
        assert_eq!(
            override_kernel_tree(None, Some(&cdpath)).unwrap(),
            Some(("Delta CDPATH", tree))
        );
    }

    #[test]
    fn delta_ignores_empty_relative_missing_and_non_kernel_entries() {
        let f = PatchFixture::new();
        // A generic Makefile and nested build caches are not kernel attachments.
        fs::write(f.source.join("Makefile"), "").unwrap();
        f.kernel("target/cache/kernel");
        let cdpath = env::join_paths([
            Path::new(""),
            Path::new("."),
            Path::new("relative"),
            &f.root.join("missing"),
            &f.root,
        ])
        .unwrap();
        assert!(override_kernel_tree(None, Some(&cdpath)).unwrap().is_none());
    }

    #[test]
    fn uboot_is_not_a_linux_kernel() {
        let f = PatchFixture::new();
        let uboot = f.kernel("u-boot");
        fs::write(
            uboot.join("README"),
            "U-Boot\nUses interfaces from the Linux kernel.\n",
        )
        .unwrap();
        let cdpath = Some(f.root.as_os_str());
        assert!(override_kernel_tree(None, cdpath).unwrap().is_none());
        assert!(override_kernel_tree(Some(&uboot), cdpath).is_err());
        let linux = f.kernel("linux");
        assert_eq!(
            override_kernel_tree(None, cdpath).unwrap(),
            Some(("Delta CDPATH", linux))
        );
    }

    #[test]
    fn modern_and_legacy_linux_readme_headers_are_recognized() {
        let f = PatchFixture::new();
        let tree = f.kernel("linux");
        for readme in [
            "Linux kernel\n============\n",
            "\tLinux kernel release 4.x <http://kernel.org/>\r\n",
        ] {
            fs::write(tree.join("README"), readme).unwrap();
            assert_eq!(
                override_kernel_tree(None, Some(f.root.as_os_str())).unwrap(),
                Some(("Delta CDPATH", tree.clone()))
            );
            assert_eq!(kernel_tree(&tree).unwrap(), tree);
        }
    }

    #[test]
    fn missing_or_invalid_readme_is_not_a_kernel() {
        let f = PatchFixture::new();
        let tree = f.kernel("linux");
        // The old file-layout heuristic must not rescue an unidentified tree.
        fs::create_dir(tree.join("init")).unwrap();
        fs::write(tree.join("init/main.c"), "").unwrap();
        for contents in [b"".as_slice(), b"\xff".as_slice()] {
            fs::write(tree.join("README"), contents).unwrap();
            assert!(
                override_kernel_tree(None, Some(f.root.as_os_str()))
                    .unwrap()
                    .is_none()
            );
            assert!(kernel_tree(&tree).unwrap_err().contains("README"));
        }
        fs::remove_file(tree.join("README")).unwrap();
        assert!(
            override_kernel_tree(None, Some(f.root.as_os_str()))
                .unwrap()
                .is_none()
        );
        assert!(kernel_tree(&tree).unwrap_err().contains("README"));
    }

    #[test]
    fn incomplete_linux_tree_is_an_error() {
        let f = PatchFixture::new();
        let tree = f.kernel("linux");
        fs::remove_file(tree.join("scripts/kconfig/merge_config.sh")).unwrap();
        assert!(override_kernel_tree(None, Some(f.root.as_os_str())).is_err());
    }

    #[test]
    fn ambiguous_delta_kernels_require_an_explicit_choice() {
        let f = PatchFixture::new();
        let first = f.kernel("first");
        let second = f.kernel("second");
        let cdpath = Some(f.root.as_os_str());
        let err = override_kernel_tree(None, cdpath).unwrap_err();
        assert!(err.contains(first.to_str().unwrap()), "{err}");
        assert!(err.contains(second.to_str().unwrap()), "{err}");
        assert!(err.contains("pass KERNEL_TREE explicitly"), "{err}");
        assert_eq!(
            override_kernel_tree(Some(&second), cdpath).unwrap(),
            Some(("explicit", second))
        );
    }

    #[test]
    fn invalid_explicit_source_does_not_fall_back_to_delta() {
        let f = PatchFixture::new();
        f.kernel("kernel");
        assert!(
            override_kernel_tree(Some(&f.root.join("missing")), Some(f.root.as_os_str())).is_err()
        );
    }

    #[test]
    fn unreadable_delta_search_root_is_an_error() {
        let f = PatchFixture::new();
        let file = f.source.join("core");
        assert!(override_kernel_tree(None, Some(file.as_os_str())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn delta_deduplicates_symlinked_kernels() {
        let f = PatchFixture::new();
        let tree = f.kernel("kernel");
        std::os::unix::fs::symlink(&tree, f.root.join("alias")).unwrap();
        assert_eq!(
            override_kernel_tree(None, Some(f.root.as_os_str())).unwrap(),
            Some(("Delta CDPATH", tree))
        );
    }

    #[test]
    fn kernel_patch_applies_once_preserving_local_work_and_index() {
        let f = PatchFixture::new();
        let patch = f.patch("one.patch", "core", "one", "two");
        let index = fs::read(f.source.join(".git/index")).unwrap();
        assert!(ensure_kernel_patches(&f.root, &f.source, std::slice::from_ref(&patch)).unwrap());
        assert_eq!(f.read("core"), "two\n");
        assert!(!ensure_kernel_patches(&f.root, &f.source, &[patch]).unwrap());
        assert_eq!(f.read("unrelated"), "local work\n");
        assert_eq!(fs::read(f.source.join(".git/index")).unwrap(), index);
    }

    #[test]
    fn dependent_kernel_patch_series_applies_and_is_idempotent() {
        let f = PatchFixture::new();
        let patches = [
            f.patch("one.patch", "core", "one", "two"),
            f.patch("two.patch", "core", "two", "three"),
        ];
        assert!(ensure_kernel_patches(&f.root, &f.source, &patches).unwrap());
        assert_eq!(f.read("core"), "three\n");
        assert!(!ensure_kernel_patches(&f.root, &f.source, &patches).unwrap());
        assert_eq!(f.read("core"), "three\n");
    }

    #[test]
    fn kernel_patch_series_can_create_then_modify_a_file() {
        let f = PatchFixture::new();
        let create = PathBuf::from("create.patch");
        fs::write(
            f.root.join(&create),
            "diff --git a/created b/created\nnew file mode 100644\n\
             --- /dev/null\n+++ b/created\n@@ -0,0 +1 @@\n+first\n",
        )
        .unwrap();
        let patches = [
            create,
            f.patch("modify.patch", "created", "first", "second"),
        ];
        assert!(ensure_kernel_patches(&f.root, &f.source, &patches).unwrap());
        assert_eq!(f.read("created"), "second\n");
        assert!(!ensure_kernel_patches(&f.root, &f.source, &patches).unwrap());
        assert_eq!(f.read("created"), "second\n");
    }

    #[test]
    fn kernel_patch_conflict_and_partial_series_preserve_source() {
        let f = PatchFixture::new();
        let patches = [
            f.patch("one.patch", "core", "one", "two"),
            f.patch("two.patch", "core", "two", "three"),
        ];
        for original in ["user edit\n", "two\n"] {
            fs::write(f.source.join("core"), original).unwrap();
            assert!(ensure_kernel_patches(&f.root, &f.source, &patches).is_err());
            assert_eq!(f.read("core"), original);
            assert_eq!(f.read("unrelated"), "local work\n");
        }
    }

    #[test]
    fn failing_later_patch_is_atomic_even_during_apply() {
        let f = PatchFixture::new();
        let first = f.patch("one.patch", "core", "one", "two");
        let second = f.patch("two.patch", "core", "not-two", "three");
        let mut stream = fs::read(f.root.join(first)).unwrap();
        stream.extend_from_slice(&fs::read(f.root.join(second)).unwrap());
        // Exercise the mutation invocation itself, not only its prior dry-run.
        assert!(
            !git_apply_series(&f.source, &stream, &[])
                .unwrap()
                .status
                .success()
        );
        assert_eq!(f.read("core"), "one\n");
        assert_eq!(f.read("unrelated"), "local work\n");
    }

    #[test]
    fn kernel_patch_path_escape_is_rejected_before_any_apply() {
        let f = PatchFixture::new();
        let patch = f.patch("one.patch", "core", "one", "two");
        for escape in [PathBuf::from("../escape.patch"), f.root.join(&patch)] {
            assert!(ensure_kernel_patches(&f.root, &f.source, &[patch.clone(), escape]).is_err());
            assert_eq!(f.read("core"), "one\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn kernel_patch_symlink_outside_workspace_is_rejected() {
        let f = PatchFixture::new();
        let external = PatchFixture::new();
        let patch = external.patch("external.patch", "core", "one", "two");
        std::os::unix::fs::symlink(external.root.join(patch), f.root.join("link.patch")).unwrap();
        assert!(ensure_kernel_patches(&f.root, &f.source, &[PathBuf::from("link.patch")]).is_err());
        assert_eq!(f.read("core"), "one\n");
    }

    #[test]
    fn kernel_patch_cannot_modify_files_outside_source() {
        let f = PatchFixture::new();
        fs::write(f.root.join("outside"), "original\n").unwrap();
        let patches = [
            f.patch("one.patch", "core", "one", "two"),
            f.patch("escape.patch", "../outside", "original", "damaged"),
        ];
        assert!(ensure_kernel_patches(&f.root, &f.source, &patches).is_err());
        assert_eq!(f.read("core"), "one\n");
        assert_eq!(
            fs::read_to_string(f.root.join("outside")).unwrap(),
            "original\n"
        );
    }
}
