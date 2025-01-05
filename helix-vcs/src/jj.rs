use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use futures::{executor::block_on_stream, StreamExt};
use jj_lib::{
    backend::{BackendResult, CommitId, CopyRecord, TreeValue},
    commit::Commit,
    config::StackedConfig,
    copies::{CopiesTreeDiffEntry, CopiesTreeDiffEntryPath, CopyRecords},
    matchers::EverythingMatcher,
    merge::Merge,
    op_store::WorkspaceId,
    repo::{ReadonlyRepo, Repo as _, StoreFactories},
    repo_path::RepoPathBuf,
    rewrite::merge_commit_trees,
    settings::UserSettings,
    store::Store,
    workspace::{default_working_copy_factories, Workspace},
};
use pollster::FutureExt;

use crate::FileChange;

#[derive(Clone)]
pub struct Repo {
    workspace_id: WorkspaceId,
    base: PathBuf,
    repo: Arc<ReadonlyRepo>,
}

pub fn open_repo(repo_path: &Path) -> Result<Repo> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())?;
    let workspace = Workspace::load(
        &settings,
        repo_path,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )?;
    Ok(Repo {
        base: repo_path.to_path_buf(),
        repo: workspace.repo_loader().load_at_head()?,
        workspace_id: workspace.workspace_id().clone(),
    })
}

pub fn get_diff_base(repo: &Repo, file: &Path) -> Result<Vec<u8>> {
    let file = file
        .strip_prefix(&repo.base)
        .context("failed to strip JJ repo root path from file")?;
    let file = RepoPathBuf::from_relative_path(file)?;

    let wc = get_working_copy(repo)?;

    let parents = wc.parents().collect::<Result<Vec<_>, _>>()?;
    let from_tree = merge_commit_trees(repo.repo.as_ref(), &parents)?;

    let merged = from_tree.path_value(&file)?.into_resolved().ok().flatten();
    let merged = merged.context("could not resolve working copy parents")?;
    let id = match merged {
        TreeValue::File { id, .. } => id,
        _ => bail!("unexpected non-file tree value"),
    };
    let mut reader = repo.repo.store().read_file(&file, &id)?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    Ok(buf)
}

fn get_working_copy(repo: &Repo) -> Result<Commit> {
    Ok(repo.repo.store().get_commit(
        repo.repo
            .view()
            .get_wc_commit_id(&repo.workspace_id)
            .context("could not get working copy id")?,
    )?)
}

pub fn get_current_head_name(repo: &Repo) -> Result<Arc<ArcSwap<Box<str>>>> {
    let wc = get_working_copy(repo)?;

    let change_id = wc.change_id().reverse_hex();
    let bookmarks = repo.repo.view().local_bookmarks_for_commit(wc.id());
    let mut bookmarks = bookmarks.map(|b| b.0).collect::<Vec<_>>().join(", ");
    if !bookmarks.is_empty() {
        bookmarks.insert_str(0, " (");
        bookmarks.push(')');
    }

    Ok(Arc::new(ArcSwap::from_pointee(
        format!("{change_id}{bookmarks}").into_boxed_str(),
    )))
}

pub fn for_each_changed_file(
    repo: &Repo,
    callback: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    let wc = get_working_copy(repo)?;
    let parents = wc.parents().collect::<Result<Vec<_>, _>>()?;
    let base = merge_commit_trees(repo.repo.as_ref(), &parents)?;

    let mut copy_records = CopyRecords::default();
    for p in &parents {
        let records = get_copy_records(repo.repo.store(), p.id(), wc.id())?;
        copy_records.add_records(records)?;
    }

    let mut tree_diff =
        base.diff_stream_with_copies(&wc.tree()?, &EverythingMatcher, &copy_records);

    async {
        while let Some(CopiesTreeDiffEntry { path, values }) = tree_diff.next().await {
            let (before, after) = values?;
            if !callback(to_change(repo, path, before, after)) {
                break;
            }
        }
        Ok(())
    }
    .block_on()
}

#[rustfmt::skip]
fn to_change(
    repo: &Repo,
    path: CopiesTreeDiffEntryPath,
    before: Merge<Option<TreeValue>>,
    after: Merge<Option<TreeValue>>,
) -> Result<FileChange, anyhow::Error> {
    let source = path.source().to_fs_path(&repo.base)?;
    if path.source() != path.target() {
        return Ok(FileChange::Renamed {
            from_path: source,
            to_path: path.target().to_fs_path(&repo.base)?,
        });
    }
    let path = source;

    // None       => conflicted
    // Some(None) => not present (deleted)
    // Some(Some) => present
    Ok(match (before.as_resolved(), after.as_resolved()) {
        // ___ -> conflicted
        (Some(_),       None)          => FileChange::Conflict  { path },

        // normal edits
        (Some(Some(_)), Some(Some(_))) => FileChange::Modified  { path },
        (Some(Some(_)), Some(None))    => FileChange::Deleted   { path },
        (Some(None),    Some(Some(_))) => FileChange::Untracked { path },

        // conflicted -> ___
        (None,          Some(Some(_))) => FileChange::Modified  { path },
        (None,          Some(None))    => FileChange::Deleted   { path },
        (None,          None)          => FileChange::Modified  { path },

        // deleted -> deleted
        // unsure if possible, final case
        (Some(None),    Some(None))    => FileChange::Modified  { path },
    })
}

fn get_copy_records<'a>(
    store: &'a Store,
    root: &CommitId,
    head: &CommitId,
) -> BackendResult<impl Iterator<Item = BackendResult<CopyRecord>> + 'a> {
    let stream = store.get_copy_records(None, root, head)?;
    Ok(block_on_stream(stream))
}
