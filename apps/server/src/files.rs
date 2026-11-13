// SPDX-License-Identifier: EUPL-1.2
//! Files the server creates with a fixed Unix mode: keys, passwords, exports
//! and the SQLite database. Elsewhere the mode is ignored.

use std::{
    fs::OpenOptions,
    io::{self, Write as _},
    path::Path,
};

/// Options to write a file that is created with `mode`.
pub(crate) fn with_mode(mode: u32) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    options
}

/// Replace `path` with `contents` through a synced `.tmp` sibling created
/// with `mode`.
pub(crate) fn write_atomically(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = with_mode(mode)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}
