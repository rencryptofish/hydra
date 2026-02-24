/// A single file's diff stats from `git diff --numstat` or untracked listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
    pub untracked: bool,
}
