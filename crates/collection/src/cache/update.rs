use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use notify::DebouncedEvent;

use crate::{util::get_modified, AudioFolderShort};

use super::CacheInner;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub(crate) enum UpdateAction {
    RefreshFolder(PathBuf),
    RefreshFolderRecursive(PathBuf),
    RemoveFolder(PathBuf),
    RenameFolder { from: PathBuf, to: PathBuf },
}

impl UpdateAction {
    fn is_covered_by(&self, other: &UpdateAction) -> bool {
        match self {
            UpdateAction::RefreshFolder(my_path) => match other {
                UpdateAction::RefreshFolder(other_path) => my_path == other_path,
                UpdateAction::RefreshFolderRecursive(other_path) => my_path.starts_with(other_path),
                UpdateAction::RemoveFolder(other_path) => my_path.starts_with(other_path),
                UpdateAction::RenameFolder { .. } => false,
            },
            UpdateAction::RefreshFolderRecursive(my_path) => match other {
                UpdateAction::RefreshFolder(_) => false,
                UpdateAction::RefreshFolderRecursive(other_path) => my_path.starts_with(other_path),
                UpdateAction::RemoveFolder(other_path) => my_path.starts_with(other_path),
                UpdateAction::RenameFolder { .. } => false,
            },
            UpdateAction::RemoveFolder(my_path) => match other {
                UpdateAction::RefreshFolder(_) => false,
                UpdateAction::RefreshFolderRecursive(_) => false,
                UpdateAction::RemoveFolder(other_path) => my_path.starts_with(other_path),
                UpdateAction::RenameFolder { .. } => false,
            },
            UpdateAction::RenameFolder {
                from: my_from,
                to: my_to,
            } => match other {
                UpdateAction::RefreshFolder(_) => false,
                UpdateAction::RefreshFolderRecursive(_) => false,
                UpdateAction::RemoveFolder(_) => false,
                UpdateAction::RenameFolder {
                    from: other_from,
                    to: other_to,
                } => my_from == other_from && my_to == other_to,
            },
        }
    }
}

impl AsRef<Path> for UpdateAction {
    fn as_ref(&self) -> &Path {
        match self {
            UpdateAction::RefreshFolder(folder) => folder.as_path(),
            UpdateAction::RefreshFolderRecursive(folder) => folder.as_path(),
            UpdateAction::RemoveFolder(folder) => folder.as_path(),
            UpdateAction::RenameFolder { from, .. } => from.as_path(),
        }
    }
}

pub(super) struct OngoingUpdater {
    queue: Receiver<Option<UpdateAction>>,
    inner: Arc<CacheInner>,
    pending: HashMap<UpdateAction, SystemTime>,
    interval: Duration,
}

impl OngoingUpdater {
    pub(super) fn new(queue: Receiver<Option<UpdateAction>>, inner: Arc<CacheInner>) -> Self {
        OngoingUpdater {
            queue,
            inner,
            pending: HashMap::new(),
            interval: Duration::from_secs(10),
        }
    }

    fn send_actions(&mut self) {
        if self.pending.len() > 0 {
            // TODO: Would replacing map with empty be more efficient then iterating?
            let done = std::mem::replace(&mut self.pending, HashMap::new());
            let mut ready = done.into_iter().collect::<Vec<_>>();
            ready.sort_unstable_by(|a, b| b.1.cmp(&a.1));

            while let Some((a, _)) = ready.pop() {
                if !ready
                    .iter()
                    .skip(ready.len().saturating_sub(100)) // This is speculative optimalization, not be O(n^2) for large sets, but work for decent changes
                    .any(|(other, _)| a.is_covered_by(other))
                {
                    self.inner.proceed_update(a.clone())
                }
            }
        }
    }

    pub(super) fn run(mut self) {
        loop {
            match self.queue.recv_timeout(self.interval) {
                Ok(Some(action)) => {
                    self.pending.insert(action, SystemTime::now());
                    if self.pending.len() >= 10_000 {
                        // should not grow too big
                        self.send_actions();
                    }
                }
                Ok(None) => {
                    self.send_actions();
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    error!("OngoingUpdater channel disconnected preliminary");
                    self.send_actions();
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    //every pending action is older then limit
                    self.send_actions();
                }
            }
        }
    }
}

pub(crate) enum FilteredEvent {
    Pass(DebouncedEvent),
    Error(notify::Error, Option<PathBuf>),
    Rescan,
    Ignore,
}

pub(crate) fn filter_event(evt: DebouncedEvent) -> FilteredEvent {
    use FilteredEvent::*;
    match evt {
        DebouncedEvent::NoticeWrite(_) => Ignore,
        DebouncedEvent::NoticeRemove(_) => Ignore,
        evt @ DebouncedEvent::Create(_) => Pass(evt),
        evt @ DebouncedEvent::Write(_) => Pass(evt),
        DebouncedEvent::Chmod(_) => Ignore,
        evt @ DebouncedEvent::Remove(_) => Pass(evt),
        evt @ DebouncedEvent::Rename(_, _) => Pass(evt),
        DebouncedEvent::Rescan => Rescan,
        DebouncedEvent::Error(e, p) => Error(e, p),
    }
}
