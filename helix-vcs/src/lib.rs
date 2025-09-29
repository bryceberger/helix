//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Currently `git` is the only supported provider for diffs, but this architecture allows
//! for other providers to be added in the future.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "git")]
mod git;
#[cfg(feature = "jj")]
mod jj;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    diff: HashMap<String, ProviderConfig>,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct ProviderConfig {
    provider: DiffProviderRaw,
    args: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let providers = HashMap::from([
            #[cfg(feature = "jj")]
            (
                "jj-trunk".into(),
                ProviderConfig {
                    provider: DiffProviderRaw::Jj,
                    args: HashMap::from([("revset".into(), "trunk() ~ @-".into())]),
                },
            ),
            #[cfg(feature = "jj")]
            (
                "jj-head".into(),
                ProviderConfig {
                    provider: DiffProviderRaw::Jj,
                    args: HashMap::from([("revset".into(), "@-".into())]),
                },
            ),
            #[cfg(feature = "git")]
            (
                "git".into(),
                ProviderConfig {
                    provider: DiffProviderRaw::Git,
                    args: HashMap::new(),
                },
            ),
            (
                "none".into(),
                ProviderConfig {
                    provider: DiffProviderRaw::None,
                    args: HashMap::new(),
                },
            ),
        ]);
        Self { diff: providers }
    }
}

/// Contains all active diff providers. Diff providers are compiled in via features. Currently
/// only `git` is supported.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<(Arc<str>, DiffProvider)>,
}

impl DiffProviderRegistry {
    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path) -> HashMap<Arc<str>, Vec<u8>> {
        self.providers
            .iter()
            .flat_map(|(name, provider)| match provider.get_diff_base(file) {
                Ok(res) => Some((name.clone(), res)),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
            .collect()
    }

    /// Get the current name of the current [HEAD](https://stackoverflow.com/questions/2304087/what-is-head-in-git).
    pub fn get_current_head_name(&self, file: &Path) -> Option<Arc<ArcSwap<Box<str>>>> {
        self.providers.iter().find_map(|(_name, provider)| {
            match provider.get_current_head_name(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            }
        })
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            if self
                .providers
                .iter()
                .find_map(|(_name, provider)| provider.for_each_changed_file(&cwd, &f).ok())
                .is_none()
            {
                f(Err(anyhow!("no diff provider returns success")));
            }
        });
    }
}

impl DiffProviderRegistry {
    pub fn from_config(config: &Config) -> Self {
        let providers = config
            .diff
            .iter()
            .map(|(name, provider)| (name.as_str().into(), DiffProvider::from_raw(provider)))
            .collect();
        DiffProviderRegistry { providers }
    }
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DiffProviderRaw {
    #[cfg(feature = "git")]
    Git,
    #[cfg(feature = "jj")]
    Jj,
    #[default]
    None,
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
#[derive(Clone, Debug)]
enum DiffProvider {
    #[cfg(feature = "git")]
    Git,
    #[cfg(feature = "jj")]
    Jj {
        revset: Arc<str>,
    },
    None,
}

impl DiffProvider {
    fn get_diff_base(&self, file: &Path) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_diff_base(file),
            #[cfg(feature = "jj")]
            Self::Jj { revset } => jj::get_diff_base(file, revset),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_current_head_name(&self, file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_current_head_name(file),
            #[cfg(feature = "jj")]
            Self::Jj { .. } => bail!("no head name for jj"),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn for_each_changed_file(
        &self,
        cwd: &Path,
        f: impl Fn(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::for_each_changed_file(cwd, f),
            #[cfg(feature = "jj")]
            Self::Jj { revset } => jj::for_each_changed_file(cwd, f, revset),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn from_raw(provider: &ProviderConfig) -> Self {
        match provider.provider {
            DiffProviderRaw::Git => Self::Git,
            DiffProviderRaw::Jj => Self::Jj {
                revset: provider
                    .args
                    .get("revset")
                    .map(|s| s.as_str())
                    .unwrap_or("@-")
                    .into(),
            },
            DiffProviderRaw::None => Self::None,
        }
    }
}
