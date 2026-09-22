//! Minimal read-only file system interface shared by the UDF and ISO 9660 readers.

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct DirEntry<N> {
    pub name: String,
    pub is_dir: bool,
    pub node: N,
}

/// Upper bound on the entries visited in one directory.
pub const MAX_DIR_ENTRIES: usize = 16_384;

pub trait FileSystem {
    /// Opaque handle to a file or directory.
    type Node: Clone;

    /// Short human-readable description ("UDF 2.50", "ISO 9660 (Joliet)").
    fn description(&self) -> String;

    fn root(&mut self) -> Result<Self::Node>;

    /// Visits the entries of `dir` in directory order until `visit` returns `false`.
    fn walk(
        &mut self,
        dir: &Self::Node,
        visit: &mut dyn FnMut(DirEntry<Self::Node>) -> bool,
    ) -> Result<()>;

    /// Size in bytes of a file.
    fn file_size(&mut self, file: &Self::Node) -> Result<u64>;

    /// Reads a whole file, failing with `TooLarge` above `max_len` bytes.
    fn read(&mut self, file: &Self::Node, max_len: usize) -> Result<Vec<u8>>;

    /// Finds the first entry called `name` (ASCII case-insensitive) of the
    /// requested kind, without materialising the rest of the directory.
    fn lookup(&mut self, dir: &Self::Node, name: &str, want_dir: bool) -> Result<Option<Self::Node>> {
        let mut found = None;
        self.walk(dir, &mut |e| {
            if e.is_dir == want_dir && e.name.eq_ignore_ascii_case(name) {
                found = Some(e.node);
                false
            } else {
                true
            }
        })?;
        Ok(found)
    }

    /// Collects only the entries accepted by `keep`.
    fn list_where(
        &mut self,
        dir: &Self::Node,
        keep: &mut dyn FnMut(&DirEntry<Self::Node>) -> bool,
    ) -> Result<Vec<DirEntry<Self::Node>>> {
        let mut out = Vec::new();
        self.walk(dir, &mut |e| {
            if keep(&e) {
                out.push(e);
            }
            true
        })?;
        Ok(out)
    }
}
