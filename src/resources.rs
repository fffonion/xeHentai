//! Resolve RSS source files from installed overrides or compiled defaults.

use std::io;
use std::path::Path;

/// Read an installed RSS override, or use the compile-time default when the
/// file is absent. Other I/O errors remain visible to the caller.
pub(crate) fn read_rss_source(
    resource_root: &Path,
    relative: &str,
    embedded: &str,
) -> io::Result<String> {
    let path = resource_root.join(relative);
    match std::fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(embedded.to_owned()),
        Err(error) => Err(error),
    }
}
