use std::path::PathBuf;

use crate::Result;

use super::{
    config::{self, KernelSourceScope, SourceConfig},
    kernel, kernel_src, kernel_tree, validate_device_component, workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct KernelPrimeArgs {
    #[arg(value_name = "default|VENDOR/SOC")]
    target: KernelPrimeTarget,
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct KernelPrimeTarget {
    vendor: Option<String>,
    name: String,
}

impl KernelPrimeTarget {
    fn id(&self) -> String {
        match &self.vendor {
            Some(vendor) => format!("{vendor}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

impl std::str::FromStr for KernelPrimeTarget {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value == "default" {
            return Ok(Self {
                vendor: None,
                name: value.to_string(),
            });
        }

        let parts = value.split('/').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(format!(
                "kernel prime target must be default or VENDOR/SOC, e.g. qcom/msm8916: {value}"
            ));
        }
        validate_device_component("vendor", parts[0])?;
        validate_device_component("SoC", parts[1])?;
        Ok(Self {
            vendor: Some(parts[0].to_string()),
            name: parts[1].to_string(),
        })
    }
}

pub(crate) fn run(args: KernelPrimeArgs) -> Result<()> {
    kernel_prime(args)
}

fn kernel_prime(args: KernelPrimeArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let config = load_prime_config(&workspace_root, &args.target)?;
    let source = config.kernel_source.as_ref().ok_or_else(|| {
        format!(
            "no [kernel-source] configured for prime target {}",
            args.target.id()
        )
    })?;
    if source.scope == KernelSourceScope::Device {
        return Err(format!(
            "device-scoped [kernel-source] cannot be primed by {}",
            args.target.id()
        ));
    }

    let kernel_tree = match args.kernel_tree {
        Some(kernel_tree_arg) => kernel_tree(&kernel_tree_arg)?,
        None => {
            let tree = kernel_src::ensure_named_kernel_source(
                &workspace_root,
                &args.target.name,
                source_tree_path(&args.target),
                source,
            )?;
            println!("kernel source {}", tree.path.display());
            kernel_tree(&tree.path)?
        }
    };

    let build =
        kernel::build_prime_kernel(&workspace_root, &kernel_tree, &args.target.id(), &config)?;
    println!("image {}", build.image.display());
    println!("config {}", build.config.display());
    Ok(())
}

fn load_prime_config(
    workspace_root: &std::path::Path,
    target: &KernelPrimeTarget,
) -> Result<SourceConfig> {
    match &target.vendor {
        Some(vendor) => config::load_soc_config(workspace_root, vendor, &target.name),
        None => config::load_default_config(workspace_root),
    }
}

fn source_tree_path(target: &KernelPrimeTarget) -> PathBuf {
    match &target.vendor {
        Some(_) => PathBuf::from(&target.name),
        None => PathBuf::from("pocketboot"),
    }
}
