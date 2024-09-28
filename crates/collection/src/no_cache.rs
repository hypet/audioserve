use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(PartialEq, Eq)]
struct DirEntry {
    path: PathBuf,
    created: SystemTime,
}

// need reverse ordering for heap, oldest will be on top
use std::cmp::Ordering;
impl PartialOrd for DirEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(match self.created.cmp(&other.created) {
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
            Ordering::Equal => self.path.cmp(&other.path),
        })
    }
}
impl Ord for DirEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}
