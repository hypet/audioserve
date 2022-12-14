use std::{
    collections::HashMap,
    ops::Deref,
    path::{Path, PathBuf},
    time::SystemTime, sync::{atomic::{AtomicUsize, Ordering}, Arc},
};

use crossbeam_channel::Sender;
use dashmap::DashMap;
use notify::DebouncedEvent;
use serde_json::Value;
use sled::{
    transaction::{self, TransactionError, Transactional},
    Batch, Db, IVec, Tree,
};

use crate::{
    audio_folder::{DirType, FolderLister},
    audio_meta::{AudioFolder, TimeStamp},
    cache::{
        update::RecursiveUpdater,
        util::{split_path, update_path},
    },
    common::PositionsData,
    error::{Error, Result},
    position::{PositionItem, PositionRecord, PositionsCollector, MAX_GROUPS},
    util::{get_file_name, get_modified},
    AudioFolderShort, FoldersOrdering, Position,
};

use super::{
    update::UpdateAction,
    util::{deser_audiofolder, parent_path},
};

#[derive(Clone)]
pub(crate) struct CacheInner {
    db: Db,
    pos_latest: Tree,
    pos_folder: Tree,
    lister: FolderLister,
    base_dir: PathBuf,
    update_sender: Sender<Option<UpdateAction>>,
    shuffle_idx_map: DashMap<usize, Vec<u8>>,
    file_counter: Arc<AtomicUsize>,
}

impl CacheInner {
    pub(crate) fn new(
        db: Db,

        lister: FolderLister,
        base_dir: PathBuf,
        update_sender: Sender<Option<UpdateAction>>,
        shuffle_idx_map: DashMap<usize, Vec<u8>>,
    ) -> Result<Self> {
        let pos_latest = db.open_tree("pos_latest")?;
        let pos_folder = db.open_tree("pos_folder")?;
        let file_counter = Arc::new(AtomicUsize::new(0));
        Ok(CacheInner {
            db,
            pos_latest,
            pos_folder,
            lister,
            base_dir,
            update_sender,
            shuffle_idx_map: shuffle_idx_map,
            file_counter,
        })
    }
}

// access methods
impl CacheInner {
    pub(crate) fn base_dir(&self) -> &Path {
        self.base_dir.as_path()
    }

    pub(crate) fn list_dir<P: AsRef<Path>>(
        &self,
        dir_path: P,
        ordering: FoldersOrdering,
    ) -> Result<AudioFolder> {
        self.lister
            .list_dir(&self.base_dir, dir_path, ordering)
            .map_err(Error::from)
    }

    pub(crate) fn count_files<P: AsRef<Path>>(
        &self,
        dir_path: P,
    ) -> Result<usize> {
        let dir = self.lister
            .list_dir(&self.base_dir, &dir_path, FoldersOrdering::Alphabetical)
            .map_err(Error::from).unwrap();
        debug!("inner count_files: {:?}: {}", &dir_path.as_ref().display(), dir.files.len());
        Ok(dir.files.len())
    }

    pub(crate) fn iter_folders(&self) -> sled::Iter {
        self.db.iter()
    }

    pub(crate) fn clean_up_folders(&self) {
        for key in self.iter_folders().filter_map(|e| e.ok()).map(|(k, _)| k) {
            if let Ok(rel_path) = std::str::from_utf8(&key) {
                let full_path = self.base_dir.join(rel_path);
                if !full_path.exists() || self.lister.is_collapsable_folder(&full_path) {
                    debug!("Removing {:?} from collection cache db", full_path);
                    self.remove(rel_path)
                        .map_err(|e| error!("cannot remove record from db: {}", e))
                        .ok();
                }
            }
        }
    }

    pub(crate) fn dir_count(&self) -> usize {
        self.shuffle_idx_map.len()
    }

    pub(crate) fn path_by_index(&self, idx: usize) -> Option<String> {
        match self.shuffle_idx_map.get(&idx) {
            Some(path) => Some(String::from_utf8(path.clone()).unwrap()),
            None => None,
        }
    }
}

impl CacheInner {
    pub(crate) fn get<P: AsRef<Path>>(&self, dir: P) -> Option<AudioFolder> {
        dir.as_ref()
            .to_str()
            .and_then(|p| {
                self.db
                    .get(p)
                    .map_err(|e| error!("Cannot get record for db: {}", e))
                    .ok()
                    .flatten()
            })
            .and_then(deser_audiofolder)
    }

    pub(crate) fn has_key<P: AsRef<Path>>(&self, dir: P) -> bool {
        dir.as_ref()
            .to_str()
            .and_then(|p| self.db.contains_key(p.as_bytes()).ok())
            .unwrap_or(false)
    }

    pub(crate) fn get_if_actual<P: AsRef<Path>>(
        &self,
        dir: P,
        ts: Option<SystemTime>,
    ) -> Option<AudioFolder> {
        let af = self.get(dir);
        af.as_ref()
            .and_then(|af| af.modified)
            .and_then(|cached_time| ts.map(|actual_time| cached_time >= actual_time))
            .and_then(|actual| if actual { af } else { None })
    }

    fn get_last_file<P: AsRef<Path>>(&self, dir: P) -> Option<(String, Option<u32>)> {
        self.get(dir).and_then(|d| {
            d.files.last().and_then(|p| {
                p.path.file_name().map(|n| {
                    (
                        n.to_str().unwrap().to_owned(),
                        p.meta.as_ref().map(|m| m.duration),
                    )
                })
            })
        })
    }

    pub(crate) fn update<P: AsRef<Path>>(&self, dir: P, af: AudioFolder) -> Result<()> {
        let dir = dir.as_ref().to_str().ok_or(Error::InvalidCollectionPath)?;
        debug!("dir: {}", &dir);
        bincode::serialize(&af)
            .map_err(Error::from)
            .and_then(|data| {
                let file_counter = self.file_counter.load(Ordering::Relaxed);
                let _res = self.file_counter.compare_exchange(file_counter, file_counter + 1, Ordering::Relaxed, Ordering::Relaxed);
                self.shuffle_idx_map.insert(file_counter, dir.into());
                self.db.insert(dir, data).map_err(Error::from)
            })
            .map(|_| debug!("Cache updated for {:?}, file_counter: {}", dir, self.file_counter.load(Ordering::Relaxed)))
    }

    pub(crate) fn force_update<P: AsRef<Path>>(
        &self,
        dir_path: P,
        ret: bool,
    ) -> Result<Option<AudioFolder>> {
        let af = self.lister.list_dir(
            &self.base_dir,
            dir_path.as_ref(),
            FoldersOrdering::Alphabetical,
        )?;
        let rv = if ret { Some(af.clone()) } else { None };
        self.update(dir_path, af)?;
        Ok(rv)
    }

    pub(crate) fn full_path<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.base_dir.join(rel_path.as_ref())
    }

    pub(crate) fn remove<P: AsRef<Path>>(&self, dir_path: P) -> Result<Option<IVec>> {
        let path = dir_path.as_ref().to_str().ok_or(Error::InvalidPath)?;
        self.db.remove(path).map_err(Error::from)
    }

    pub(crate) fn remove_tree<P: AsRef<Path>>(&self, dir_path: P) -> Result<()> {
        let path = dir_path.as_ref().to_str().ok_or(Error::InvalidPath)?;
        let pos_batch = self.remove_positions_batch(&dir_path)?;
        let mut batch = Batch::default();
        self.db
            .scan_prefix(path)
            .filter_map(|r| r.ok())
            .for_each(|(key, _)| batch.remove(key));
        (self.db.deref(), &self.pos_folder)
            .transaction(|(db, pos_folder)| {
                db.apply_batch(&batch)?;
                pos_folder.apply_batch(&pos_batch)?;
                Ok(())
            })
            .map_err(Error::from)
    }

    pub fn flush(&self) -> Result<()> {
        let res = vec![
            self.db.flush(),
            self.pos_folder.flush(),
            self.pos_latest.flush(),
        ];
        res.into_iter()
            .find(|r| r.is_err())
            .unwrap_or(Ok(0))
            .map(|_| ())
            .map_err(Error::from)
    }
}

// positions
impl CacheInner {
    const EOB_LIMIT: u32 = 10;

    pub(crate) fn insert_position<S, P>(
        &self,
        group: S,
        path: P,
        position: f32,
        finished: bool,
        ts: Option<TimeStamp>,
        use_ts: bool,
    ) -> Result<()>
    where
        S: AsRef<str>,
        P: AsRef<str>,
    {
        let (path, file) = split_path(&path);
        if let Some((last_file, last_file_duration)) = self.get_last_file(&path) {
            (&self.pos_latest, &self.pos_folder)
                .transaction(move |(pos_latest, pos_folder)| {
                    let mut folder_rec = pos_folder
                        .get(&path)
                        .map_err(|e| error!("Db get error: {}", e))
                        .ok()
                        .flatten()
                        .and_then(|data| {
                            bincode::deserialize::<PositionRecord>(&data)
                                .map_err(|e| error!("Db item deserialization error: {}", e))
                                .ok()
                        })
                        .unwrap_or_else(HashMap::new);

                    if let Some(ts) = ts {
                        if let Some(current_record) = folder_rec.get(group.as_ref()) {
                            if current_record.timestamp > ts {
                                info!(
                                    "Position not inserted for folder {} because it's outdated. It has timestamp {:?}, but we have ts {:?}",
                                    path,
                                    ts,
                                    current_record.timestamp
                                );
                                return transaction::abort(Error::IgnoredPosition);
                            } else {
                                debug!(
                                    "Updating position record {} dated {:?} with new from {:?}",
                                    path, current_record.timestamp, ts
                                );
                            }
                        }
                    }
                    let this_pos = PositionItem {
                        folder_finished: finished
                            || (file.eq(&last_file)
                                && last_file_duration
                                    .map(|d| d.checked_sub(position.round() as u32).unwrap_or(0))
                                    .map(|dif| dif < CacheInner::EOB_LIMIT)
                                    .unwrap_or(false)),
                        file: file.into(),
                        timestamp: if use_ts && ts.is_some() {
                            ts.unwrap()
                        } else {
                            TimeStamp::now()
                        },
                        position,
                    };

                    if !folder_rec.contains_key(group.as_ref()) && folder_rec.len() >= MAX_GROUPS {
                        return transaction::abort(Error::TooManyGroups);
                    }

                    folder_rec.insert(group.as_ref().into(), this_pos);
                    let rec = match bincode::serialize(&folder_rec) {
                        Err(e) => return transaction::abort(Error::from(e)),
                        Ok(res) => res,
                    };

                    pos_folder.insert(path.as_bytes(), rec)?;
                    pos_latest.insert(group.as_ref(), path.as_bytes())?;
                    Ok(())
                })
                .map_err(Error::from)
        } else {
            // folder does not have playable file or does not exist in cache
            warn!(
                "Trying to insert position for unknown or empty folder {}",
                path
            );
            Err(Error::IgnoredPosition)
        }
    }

    pub(crate) fn get_position<S, P>(&self, group: S, folder: Option<P>) -> Option<Position>
    where
        S: AsRef<str>,
        P: AsRef<str>,
    {
        (&self.pos_latest, &self.pos_folder)
            .transaction(|(pos_latest, pos_folder)| {
                let fld = match folder.as_ref().map(|f| f.as_ref().to_string()).or_else(|| {
                    pos_latest
                        .get(group.as_ref())
                        .map_err(|e| error!("Get last pos db error: {}", e))
                        .ok()
                        .flatten()
                        // it's safe because we know for sure we inserted string here
                        .map(|data| unsafe { String::from_utf8_unchecked(data.as_ref().into()) })
                }) {
                    Some(s) => s,
                    None => return Ok(None),
                };

                Ok(pos_folder
                    .get(&fld)
                    .map_err(|e| error!("Error reading position folder record in db: {}", e))
                    .ok()
                    .flatten()
                    .and_then(|r| {
                        bincode::deserialize::<PositionRecord>(&r)
                            .map_err(|e| error!("Error deserializing position record {}", e))
                            .ok()
                    })
                    .and_then(|m| m.get(group.as_ref()).map(|p| p.to_position(fld, 0))))
            })
            .map_err(|e: TransactionError<Error>| error!("Db transaction error: {}", e))
            .ok()
            .flatten()
    }

    pub(crate) fn get_positions_recursive<S, P>(
        &self,
        group: S,
        folder: P,
        collection_no: usize,
        res: &mut PositionsCollector,
    ) where
        S: AsRef<str>,
        P: AsRef<str>,
    {
        CacheInner::positions_from_iter(
            self.pos_folder.scan_prefix(folder.as_ref()),
            group,
            collection_no,
            res,
        )
    }

    pub(crate) fn is_finished<S, P>(&self, group: S, dir: P) -> bool
    where
        S: AsRef<str>,
        P: AsRef<str>,
    {
        self.pos_folder
            .get(dir.as_ref())
            .map_err(|e| error!("Error reading position folder record in db: {}", e))
            .ok()
            .flatten()
            .and_then(|r| {
                bincode::deserialize::<PositionRecord>(&r)
                    .map_err(|e| error!("Error deserializing position record {}", e))
                    .ok()
            })
            .and_then(|m| m.get(group.as_ref()).map(|p| p.folder_finished))
            .unwrap_or(false)
    }

    pub(crate) fn update_subfolder<S: AsRef<str>>(&self, group: S, sf: &mut AudioFolderShort) {
        sf.finished = sf
            .path
            .to_str()
            .map(|path| self.is_finished(&group, path))
            .unwrap_or(false)
    }

    pub(crate) fn update_subfolders<S: AsRef<str>>(
        &self,
        group: S,
        subfolders: &mut Vec<AudioFolderShort>,
    ) {
        subfolders
            .iter_mut()
            .for_each(|sf| self.update_subfolder(&group, sf))
    }

    fn positions_from_iter<I, S>(
        iter: I,
        group: S,
        collection_no: usize,
        res: &mut PositionsCollector,
    ) where
        I: Iterator<Item = std::result::Result<(sled::IVec, sled::IVec), sled::Error>>,
        S: AsRef<str>,
    {
        iter.filter_map(|res| {
            res.map_err(|e| error!("Error reading from positions db: {}", e))
                .ok()
                .and_then(|(folder, rec)| {
                    let rec: PositionRecord = bincode::deserialize(&rec)
                        .map_err(|e| error!("Position deserialization error: {}", e))
                        .ok()?;
                    let folder = String::from_utf8(folder.as_ref().into()).unwrap(); // known to be valid UTF8
                    rec.get(group.as_ref())
                        .map(|p| p.to_position(folder, collection_no))
                })
        })
        .for_each(|p| res.add(p))
    }

    pub(crate) fn get_all_positions_for_group<S>(
        &self,
        group: S,
        collection_no: usize,
        res: &mut PositionsCollector,
    ) where
        S: AsRef<str>,
    {
        CacheInner::positions_from_iter(self.pos_folder.iter(), group, collection_no, res)
    }

    fn remove_positions_batch<P: AsRef<Path>>(&self, path: P) -> Result<Batch> {
        let mut batch = Batch::default();
        self.pos_folder
            .scan_prefix(path.as_ref().to_str().ok_or(Error::InvalidPath)?)
            .filter_map(|r| {
                r.map_err(|e| error!("Cannot read positions db: {}", e))
                    .ok()
            })
            .for_each(|(k, _)| batch.remove(k));

        Ok(batch)
    }

    fn remove_positions<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let batch = self.remove_positions_batch(path)?;
        self.pos_folder
            .transaction(|pos_folder| pos_folder.apply_batch(&batch).map_err(|e| e.into()))
            .map_err(Error::from)
    }

    fn rename_positions(&self, from: &Path, to: &Path) -> Result<()> {
        let mut delete_batch = Batch::default();
        let mut insert_batch = Batch::default();
        let mut group_batch = Batch::default();

        let iter = self
            .pos_folder
            .scan_prefix(from.to_str().ok_or(Error::InvalidPath)?)
            .filter_map(|r| {
                r.map_err(|e| error!("Cannot read positions db: {}", e))
                    .ok()
            });
        for (k, v) in iter {
            delete_batch.remove(k.clone());
            let new_key = update_path(from, to, Path::new(std::str::from_utf8(&k)?))?;
            let new_key = new_key.to_str().unwrap();
            insert_batch.insert(new_key, v);
        }

        for (k, v) in self.pos_latest.iter().filter_map(|r| {
            r.map_err(|e| error!("Error reading latest position db: {}", e))
                .ok()
        }) {
            let fld = Path::new(std::str::from_utf8(&v)?);
            if fld.starts_with(from) {
                let new_path = update_path(from, to, fld)?;
                let new_path = new_path.to_str().unwrap(); // was created from UTF8 few lines above
                group_batch.insert(k, new_path);
            }
        }

        (&self.pos_folder, &self.pos_latest)
            .transaction(|(pos_folder, pos_latest)| {
                pos_folder.apply_batch(&delete_batch)?;
                pos_folder.apply_batch(&insert_batch)?;
                pos_latest.apply_batch(&group_batch)?;
                Ok(())
            })
            .map_err(Error::from)
    }

    pub(crate) fn clean_up_positions(&self) {
        let mut batch = Batch::default();
        self.pos_folder
            .iter()
            .filter_map(|r| match r {
                Ok((k, _)) => {
                    if !self.db.contains_key(&k).unwrap_or(false) {
                        Some(k)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    error!("Error reading from db: {}", e);
                    None
                }
            })
            .for_each(|k| {
                debug!(
                    "Removing positions for directory {:?} as it does not exists",
                    std::str::from_utf8(&k)
                );
                batch.remove(k);
            });
        self.pos_folder
            .apply_batch(batch)
            .map_err(|e| error!("Cannot remove positions: {}", e))
            .ok();
    }

    pub(crate) fn write_json_positions<F: std::io::Write>(&self, mut file: &mut F) -> Result<()> {
        write!(file, "{{")?;
        for (idx, res) in self.pos_folder.iter().enumerate() {
            match res {
                Ok((k, v)) => {
                    let folder = std::str::from_utf8(&k)?;
                    let res: PositionRecord = bincode::deserialize(&v)?;
                    write!(file, "\"{}\":", folder)?;
                    serde_json::to_writer(&mut file, &res)?;
                    if idx < self.pos_folder.len() - 1 {
                        writeln!(file, ",")?;
                    } else {
                        writeln!(file)?;
                    }
                }
                Err(e) => error!("Error when reading from position db: {}", e),
            }
        }
        write!(file, "}}")?;
        Ok(())
    }

    // It may not be much efficient, but it's simple and it's ok, as restore from will be rarely used
    pub(crate) fn read_json_positions(&self, data: PositionsData) -> Result<()> {
        match data {
            PositionsData::Legacy(_) => todo!(),
            PositionsData::V1(json) => {
                for (folder, rec) in json.into_iter() {
                    if let Value::Object(map) = rec {
                        for (group, v) in map.into_iter() {
                            let item: PositionItem = serde_json::from_value(v)?;
                            let path = if folder.is_empty() {
                                item.file
                            } else {
                                folder.clone() + "/" + &item.file
                            };
                            trace!("Inserting position {} ts {:?}", path, item.timestamp);
                            self.insert_position(
                                group,
                                path,
                                item.position,
                                item.folder_finished,
                                Some(item.timestamp),
                                true,
                            )
                            .or_else(|e| {
                                if matches!(e, Error::IgnoredPosition) {
                                    Ok(())
                                } else {
                                    Err(e)
                                }
                            })?;
                        }
                    } else {
                        return Err(Error::JsonSchemaError(format!(
                            "Expected object for key {}",
                            folder
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

// Updating based on fs events
impl CacheInner {
    fn force_update_recursive<P: Into<PathBuf>>(&self, folder: P) {
        let folder = folder.into();
        let af: AudioFolderShort = AudioFolderShort {
            name: get_file_name(&folder).into(),
            modified: None,
            path: folder,
            is_file: false,
            finished: false,
        };
        let updater = RecursiveUpdater::new(self, Some(af), false);
        updater.process();
    }

    fn update_recursive_after_rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut delete_batch = Batch::default();
        let mut insert_batch = Batch::default();

        let mut updated = get_modified(self.base_dir.join(to));
        debug!("Renamed root modified for {:?}", updated);
        for item in self.db.scan_prefix(from.to_str().unwrap()) {
            // safe to unwrap as we insert only valid strings
            let (k, v) = item?;
            let mut folder_rec: AudioFolder = bincode::deserialize(&v)?;
            let p: &Path = Path::new(unsafe { std::str::from_utf8_unchecked(&k) }); // we insert only valid strings as keys
            let new_key = update_path(from, to, p)?;
            let new_key = new_key.to_str().ok_or(Error::InvalidPath)?;
            trace!(
                "Processing path {} from key {} in to {:?}",
                new_key,
                std::str::from_utf8(&k).unwrap(),
                to
            );
            if let Some(mod_time) = updated {
                if to.to_str() == Some(new_key) {
                    // this is root renamed folder, for which mod time has changed
                    folder_rec.modified = Some(mod_time.into());
                    debug!(
                        "Updating modified for {} to {:?}",
                        new_key, folder_rec.modified
                    );
                    updated.take();
                }
            }
            delete_batch.remove(k);
            for sf in folder_rec.subfolders.iter_mut() {
                let new_path = update_path(from, to, &sf.path)?;
                sf.path = new_path;
            }
            for f in folder_rec.files.iter_mut() {
                let new_path = update_path(from, to, &f.path)?;
                f.path = new_path;
            }
            if let Some(mut d) = folder_rec.description.take() {
                d.path = update_path(from, to, &d.path)?;
                folder_rec.description = Some(d);
            }

            if let Some(mut c) = folder_rec.cover.take() {
                c.path = update_path(from, to, &c.path)?;
                folder_rec.cover = Some(c);
            }

            insert_batch.insert(new_key, bincode::serialize(&folder_rec)?);
        }

        self.db
            .transaction(|db| {
                db.apply_batch(&delete_batch)?;
                db.apply_batch(&insert_batch)?;
                Ok(())
            })
            .map_err(Error::from)
    }

    pub(crate) fn proceed_update(&self, update: UpdateAction) {
        debug!("Update action: {:?}", update);
        match update {
            UpdateAction::RefreshFolder(folder) => {
                self.force_update(&folder, false)
                    .map_err(|e| warn!("Error updating folder in cache: {}", e))
                    .ok();
            }
            UpdateAction::RefreshFolderRecursive(folder) => {
                self.force_update_recursive(folder);
            }
            UpdateAction::RemoveFolder(folder) => {
                self.remove_tree(&folder)
                    .map_err(|e| warn!("Error removing folder from cache: {}", e))
                    .ok();
            }
            UpdateAction::RenameFolder { from, to } => {
                if let Err(e) = self.update_recursive_after_rename(&from, &to) {
                    error!("Failed to do recursive rename, error: {}, we will have to do rescan of {:?}", e, &to);
                    self.remove_tree(&from)
                        .map_err(|e| warn!("Error removing folder from cache: {}", e))
                        .ok();
                    self.force_update_recursive(&to);
                }
                if let Err(e) = self.rename_positions(&from, &to) {
                    error!(
                        "Error when renaming positions, will try at least to delete them: {}",
                        e
                    );
                    self.remove_positions(&from)
                        .map_err(|e| error!("Even deleting positions failed: {}", e))
                        .ok();
                }
            }
        }
    }

    pub(crate) fn proceed_event(&self, evt: DebouncedEvent) {
        let snd = |a| {
            self.update_sender
                .send(Some(a))
                .map_err(|e| error!("Error sending update {}", e))
                .ok()
                .unwrap_or(())
        };
        match evt {
            DebouncedEvent::Create(p) => {
                let col_path = self.strip_base(&p);
                if self.is_dir(&p).is_dir() {
                    snd(UpdateAction::RefreshFolderRecursive(col_path.into()));
                }
                snd(UpdateAction::RefreshFolder(
                    self.get_true_parent(col_path, &p),
                ));
            }
            DebouncedEvent::Write(p) => {
                let col_path = self.strip_base(&p);
                // AFAIK you cannot get write on directory
                if self.is_dir(&p).is_dir() {
                    // should be single file folder
                    snd(UpdateAction::RefreshFolder(col_path.into()));
                } else {
                    snd(UpdateAction::RefreshFolder(
                        self.get_true_parent(col_path, &p),
                    ));
                }
            }
            DebouncedEvent::Remove(p) => {
                let col_path = self.strip_base(&p);
                if self.is_dir(&p).is_dir() {
                    snd(UpdateAction::RemoveFolder(col_path.into()));
                    snd(UpdateAction::RefreshFolder(parent_path(col_path)));
                } else {
                    snd(UpdateAction::RefreshFolder(
                        self.get_true_parent(col_path, &p),
                    ))
                }
            }
            DebouncedEvent::Rename(p1, p2) => {
                let col_path = self.strip_base(&p1);
                match (p2.starts_with(&self.base_dir), self.is_dir(&p1).is_dir()) {
                    (true, true) => {
                        if self.lister.is_collapsable_folder(&p2) {
                            snd(UpdateAction::RemoveFolder(col_path.into()));
                        } else {
                            let dest_path = self.strip_base(&p2).into();
                            snd(UpdateAction::RenameFolder {
                                from: col_path.into(),
                                to: dest_path,
                            });
                        }
                        let orig_parent = parent_path(col_path);
                        let new_parent = parent_path(self.strip_base(&p2));
                        let diff = new_parent != orig_parent;
                        snd(UpdateAction::RefreshFolder(orig_parent));
                        if diff {
                            snd(UpdateAction::RefreshFolder(new_parent));
                        }
                    }
                    (true, false) => {
                        snd(UpdateAction::RefreshFolder(
                            self.get_true_parent(col_path, &p1),
                        ));
                        let dest_path = self.strip_base(&p2);
                        if self.is_dir(&p2).is_dir() {
                            snd(UpdateAction::RefreshFolder(parent_path(dest_path)));
                            snd(UpdateAction::RefreshFolderRecursive(dest_path.into()))
                        } else {
                            snd(UpdateAction::RefreshFolder(
                                self.get_true_parent(dest_path, &p2),
                            ))
                        }
                    }
                    (false, true) => {
                        snd(UpdateAction::RemoveFolder(col_path.into()));
                        snd(UpdateAction::RefreshFolder(parent_path(col_path)));
                    }
                    (false, false) => snd(UpdateAction::RefreshFolder(
                        self.get_true_parent(col_path, &p1),
                    )),
                }
            }
            other => {
                error!("This event {:?} should not get here", other);
            }
        };
    }

    fn get_true_parent(&self, rel_path: &Path, full_path: &Path) -> PathBuf {
        let parent = parent_path(rel_path);
        if self.lister.collapse_cd_enabled()
            && full_path
                .parent()
                .map(|p| self.is_dir(&p).is_collapsed())
                .unwrap_or(false)
        {
            parent_path(parent)
        } else {
            parent
        }
    }

    /// must be used only on paths with this collection
    fn strip_base<'a, P>(&self, full_path: &'a P) -> &'a Path
    where
        P: AsRef<Path>,
    {
        full_path.as_ref().strip_prefix(&self.base_dir).unwrap() // Should be safe as is used only with this collection
    }

    /// only for absolute paths
    fn is_dir<P: AsRef<Path>>(&self, full_path: P) -> FolderType {
        let full_path: &Path = full_path.as_ref();
        assert!(full_path.is_absolute(), "path {:?}", full_path);
        let col_path = self.strip_base(&full_path);
        match self.lister.get_dir_type(full_path) {
            Ok(DirType::Dir) => {
                if self.has_key(col_path) {
                    FolderType::RegularDir
                } else {
                    if self.lister.is_collapsable_folder(col_path) {
                        return FolderType::CollapsedDir;
                    } else {
                        return FolderType::NewDir;
                    }
                }
            }
            Ok(DirType::File { .. }) => {
                if self.has_key(col_path) {
                    FolderType::FileDir
                } else {
                    FolderType::NewFileDir
                }
            }
            Ok(DirType::Other) => FolderType::RegularFile,
            Err(e) => {
                if !matches!(e.kind(), std::io::ErrorKind::NotFound) {
                    error!("Error determining dir type: {}", e);
                    FolderType::Unknown
                } else {
                    if self.has_key(col_path) {
                        FolderType::DeletedDir
                    } else {
                        FolderType::Unknown
                    }
                }
            }
        }
    }
}

enum FolderType {
    RegularDir,
    DeletedDir,
    FileDir,
    RegularFile,
    CollapsedDir,
    NewDir,
    NewFileDir,
    Unknown,
}

impl FolderType {
    fn is_dir(&self) -> bool {
        use FolderType::*;
        match self {
            RegularFile | Unknown | CollapsedDir => false,
            _ => true,
        }
    }

    fn is_collapsed(&self) -> bool {
        matches!(self, FolderType::CollapsedDir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trailing_slash() {
        let p1 = Path::new("kulisak");
        let p2 = p1.join("");
        assert_eq!(p1, p2);
        assert_ne!(p1.to_str(), p2.to_str());
        assert_eq!(p2.to_str().unwrap(), "kulisak/");
    }

    #[test]
    fn test_bytes() {
        let v = vec![0, 0, 1, 112, 66, 198, 183, 124, 1, 0, 0, 1, 130, 10, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 39, 0, 0, 0, 0, 0, 0, 0, 48, 49, 46, 32, 
        71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 89, 111, 117, 32, 67, 97, 110, 39, 116, 32, 83, 116, 111, 112, 32, 77, 101, 46, 102, 108, 
        97, 99, 96, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 
        110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 49, 46, 32, 71, 117, 97, 
        110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 89, 111, 117, 32, 67, 97, 110, 39, 116, 32, 83, 116, 111, 112, 32, 77, 101, 46, 102, 108, 97, 99, 1, 191, 0, 
        0, 0, 4, 4, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 17, 0, 0, 0, 0, 0, 0, 0, 89, 111, 117, 32, 67, 97, 110, 39, 
        116, 32, 83, 116, 111, 112, 32, 77, 101, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 26, 0, 0, 0, 0, 0, 0, 0, 48, 50, 46, 32, 
        71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 68, 105, 99, 107, 46, 102, 108, 97, 99, 83, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 
        112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 
        56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 50, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 68, 105, 99, 107, 46, 
        102, 108, 97, 99, 1, 163, 0, 0, 0, 14, 4, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 4, 0, 0, 0, 0, 0, 0, 0, 68, 105, 
        99, 107, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 35, 0, 0, 0, 0, 0, 0, 0, 48, 51, 46, 32, 71, 117, 97, 110, 111, 32, 65, 
        112, 101, 115, 32, 45, 32, 75, 105, 115, 115, 32, 84, 104, 101, 32, 68, 97, 119, 110, 46, 102, 108, 97, 99, 92, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 
        32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 
        32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 51, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 75, 105, 115, 
        115, 32, 84, 104, 101, 32, 68, 97, 119, 110, 46, 102, 108, 97, 99, 1, 64, 1, 0, 0, 154, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 
        116, 108, 101, 13, 0, 0, 0, 0, 0, 0, 0, 75, 105, 115, 115, 32, 84, 104, 101, 32, 68, 97, 119, 110, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 
        108, 97, 99, 0, 39, 0, 0, 0, 0, 0, 0, 0, 48, 52, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 80, 114, 101, 116, 116, 121, 32, 73, 110, 
        32, 83, 99, 97, 114, 108, 101, 116, 46, 102, 108, 97, 99, 96, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 
        32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 
        32, 50, 41, 47, 48, 52, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 80, 114, 101, 116, 116, 121, 32, 73, 110, 32, 83, 99, 97, 114, 108, 
        101, 116, 46, 102, 108, 97, 99, 1, 246, 0, 0, 0, 223, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 17, 0, 0, 0, 0, 0, 
        0, 0, 80, 114, 101, 116, 116, 121, 32, 73, 110, 32, 83, 99, 97, 114, 108, 101, 116, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 
        29, 0, 0, 0, 0, 0, 0, 0, 48, 53, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 68, 105, 111, 107, 104, 97, 110, 46, 102, 108, 97, 99, 86, 
        0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 
        84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 53, 46, 32, 71, 117, 97, 110, 111, 32, 65, 
        112, 101, 115, 32, 45, 32, 68, 105, 111, 107, 104, 97, 110, 46, 102, 108, 97, 99, 1, 214, 0, 0, 0, 243, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 
        0, 0, 84, 105, 116, 108, 101, 7, 0, 0, 0, 0, 0, 0, 0, 68, 105, 111, 107, 104, 97, 110, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 
        0, 29, 0, 0, 0, 0, 0, 0, 0, 48, 54, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 81, 117, 105, 101, 116, 108, 121, 46, 102, 108, 97, 99, 
        86, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 
        32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 54, 46, 32, 71, 117, 97, 110, 111, 32, 
        65, 112, 101, 115, 32, 45, 32, 81, 117, 105, 101, 116, 108, 121, 46, 102, 108, 97, 99, 1, 218, 0, 0, 0, 208, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 
        0, 0, 0, 0, 84, 105, 116, 108, 101, 7, 0, 0, 0, 0, 0, 0, 0, 81, 117, 105, 101, 116, 108, 121, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 
        97, 99, 0, 26, 0, 0, 0, 0, 0, 0, 0, 48, 55, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 72, 105, 103, 104, 46, 102, 108, 97, 99, 83, 0, 
        0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 
        104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 55, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 
        101, 115, 32, 45, 32, 72, 105, 103, 104, 46, 102, 108, 97, 99, 1, 204, 0, 0, 0, 236, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 
        108, 101, 4, 0, 0, 0, 0, 0, 0, 0, 72, 105, 103, 104, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 36, 0, 0, 0, 0, 0, 0, 0, 48, 56, 
        46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 105, 110, 103, 32, 84, 104, 97, 116, 32, 83, 111, 110, 103, 46, 102, 108, 97, 99, 93, 0, 
        0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 
        104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 56, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 
        101, 115, 32, 45, 32, 83, 105, 110, 103, 32, 84, 104, 97, 116, 32, 83, 111, 110, 103, 46, 102, 108, 97, 99, 1, 183, 0, 0, 0, 216, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 
        0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 14, 0, 0, 0, 0, 0, 0, 0, 83, 105, 110, 103, 32, 84, 104, 97, 116, 32, 83, 111, 110, 103, 10, 0, 0, 0, 0, 
        0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 39, 0, 0, 0, 0, 0, 0, 0, 48, 57, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 
        99, 114, 97, 116, 99, 104, 32, 84, 104, 101, 32, 80, 105, 116, 99, 104, 46, 102, 108, 97, 99, 96, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 
        115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 
        54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 48, 57, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 99, 114, 97, 116, 99, 104, 32, 84, 104, 
        101, 32, 80, 105, 116, 99, 104, 46, 102, 108, 97, 99, 1, 227, 0, 0, 0, 233, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101,
         17, 0, 0, 0, 0, 0, 0, 0, 83, 99, 114, 97, 116, 99, 104, 32, 84, 104, 101, 32, 80, 105, 116, 99, 104, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 
         108, 97, 99, 0, 35, 0, 0, 0, 0, 0, 0, 0, 49, 48, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 80, 108, 97, 115, 116, 105, 99, 32, 77, 111, 
         117, 116, 104, 46, 102, 108, 97, 99, 92, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 
         105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 49, 48, 46, 
         32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 80, 108, 97, 115, 116, 105, 99, 32, 77, 111, 117, 116, 104, 46, 102, 108, 97, 99, 1, 244, 0, 0, 0,
          199, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 13, 0, 0, 0, 0, 0, 0, 0, 80, 108, 97, 115, 116, 105, 99, 32, 77, 111, 117, 116, 104, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 27, 0, 0, 0, 0, 0, 0, 0, 49, 49, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 116, 111, 114, 109, 46, 102, 108, 97, 99, 84, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 49, 49, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 116, 111, 114, 109, 46, 102, 108, 97, 99, 1, 227, 0, 0, 0, 240, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 5, 0, 0, 0, 0, 0, 0, 0, 83, 116, 111, 114, 109, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 32, 0, 0, 0, 0, 0, 0, 0, 49, 50, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 117, 103, 97, 114, 32, 83, 107, 105, 110, 46, 102, 108, 97, 99, 89, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 49, 50, 46, 32, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 83, 117, 103, 97, 114, 32, 83, 107, 105, 110, 46, 102, 108, 97, 99, 1, 253, 0, 0, 0, 235, 3, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 84, 105, 116, 108, 101, 10, 0, 0, 0, 0, 0, 0, 0, 83, 117, 103, 97, 114, 32, 83, 107, 105, 110, 10, 0, 0, 0, 0, 0, 0, 0, 97, 117, 100, 105, 111, 47, 102, 108, 97, 99, 0, 1, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 67, 111, 118, 101, 114, 115, 1, 73, 167, 96, 157, 105, 1, 0, 0, 63, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 67, 111, 118, 101, 114, 115, 0, 0, 1, 67, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 102, 111, 108, 100, 101, 114, 46, 106, 112, 103, 10, 0, 0, 0, 0, 0, 0, 0, 105, 109, 97, 103, 101, 47, 106, 112, 101, 103, 1, 96, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 47, 50, 48, 48, 51, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 32, 40, 56, 50, 56, 55, 54, 32, 53, 48, 55, 57, 50, 32, 50, 41, 47, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 32, 45, 32, 87, 97, 108, 107, 105, 110, 103, 32, 79, 110, 32, 65, 32, 84, 104, 105, 110, 32, 76, 105, 110, 101, 46, 108, 111, 103, 10, 0, 0, 0, 0, 0, 0, 0, 116, 101, 120, 116, 47, 112, 108, 97, 105, 110, 1, 3, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 71, 101, 110, 114, 101, 4, 0, 0, 0, 0, 0, 0, 0, 82, 111, 99, 107, 6, 0, 0, 0, 0, 0, 0, 0, 65, 114, 116, 105, 115, 116, 10, 0, 0, 0, 0, 0, 0, 0, 71, 117, 97, 110, 111, 32, 65, 112, 101, 115, 4, 0, 0, 0, 0, 0, 0, 0, 89, 101, 97, 114, 4, 0, 0, 0, 0, 0, 0, 0, 78, 111, 110, 101];
        let s = String::from_utf8_lossy(&v);
        println!("str: {}", s);          

    }
}
