use std::{path::Path, process::Command};

use anyhow::Result;

use crate::FileChange;

pub fn get_diff_base(file: &Path, revset: &str) -> Result<Vec<u8>> {
    let mut cmd = Command::new("jj");
    cmd.args(["--ignore-working-copy", "file", "show", "--revision"])
        .arg(revset)
        .arg(file);
    Ok(cmd.output()?.stdout)
}

pub fn for_each_changed_file(
    cwd: &Path,
    f: impl Fn(Result<FileChange>) -> bool,
    revset: &str,
) -> Result<()> {
    todo!()
}
