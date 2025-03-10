use std::{
    collections::HashMap,
    ops::Deref,
    path::{Path, PathBuf},
    time::SystemTime,
    vec,
};

use crate::{
    audio_folder::{DirType, FolderLister},
    audio_meta::{AudioFolderInner, ScoredAudioFile, TimeStamp, TrackMeta},
    cache::util::{split_path, update_path},
    common::PositionsData,
    error::{Error, Result, TreeType},
    position::{PositionItem, PositionRecord, PositionsCollector, MAX_GROUPS},
    util::get_modified,
    AudioFileInner, AudioFolderShort, FoldersOrdering, Position,
};
use crossbeam_channel::Sender;
use notify::DebouncedEvent;
use serde_json::Value;
use sled::{
    transaction::{self, TransactionError, Transactional},
    Batch, Db, IVec, Tree,
};
use tantivy::{query::{BooleanQuery, FuzzyTermQuery, Occur, Query, RegexQuery, TermQuery}, schema::IndexRecordOption};
use tantivy::{
    collector::TopDocs,
    query::QueryParser,
    schema::{Field, OwnedValue, Schema},
    Document, Index, Searcher, TantivyDocument, Term,
};

use super::{
    update::UpdateAction,
    util::{deser_audiofile, deser_audiofolder, deser_trackmeta, parent_path},
};

#[derive(Clone)]
pub(crate) struct SearchEngine {
    pub search_fields: Vec<Field>,
    pub id_field: Field,
    pub schema: Schema,
    pub index: Index,
    pub searcher: Searcher,
}

#[derive(Clone)]
pub(crate) struct CacheInner {
    db: Db,
    meta_tree: Tree,
    pos_latest: Tree,
    pos_folder: Tree,
    lister: FolderLister,
    base_dir: PathBuf,
    update_sender: Sender<Option<UpdateAction>>,
    search: Option<SearchEngine>,
}

impl CacheInner {
    pub(crate) fn new(
        db: Db,
        lister: FolderLister,
        base_dir: PathBuf,
        update_sender: Sender<Option<UpdateAction>>,
        search: Option<SearchEngine>,
    ) -> Result<Self> {
        let pos_latest = db.open_tree("pos_latest")?;
        let pos_folder = db.open_tree("pos_folder")?;
        let meta_tree = db.open_tree("meta")?;
        Ok(CacheInner {
            db,
            meta_tree,
            pos_latest,
            pos_folder,
            lister,
            base_dir,
            update_sender,
            search,
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
    ) -> Result<AudioFolderInner> {
        self.lister
            .list_dir(&self.base_dir, dir_path, ordering)
            .map_err(Error::from)
    }

    pub(crate) fn list_all(&self) -> Result<AudioFolderInner> {
        let files: Vec<AudioFileInner> = self.db.iter()
            .filter_map(|r| r.ok())
            .filter_map(|(_, val)| {
                match deser_audiofile(val) {
                    Some(af) => Some(af),
                    None => None,
                }
            })
            .collect();
        let af = AudioFolderInner {
            modified: Some(TimeStamp::now()),
            total_time: Some(100),
            files: files,
            subfolders: Vec::new(),
            cover: None,
            description: None,
            tags: None,
        };
        Ok(af)
    }

    pub(crate) fn search_collection<S: AsRef<str>>(
        &self,
        query: S,
    ) -> Result<Vec<ScoredAudioFile>> {
        let mut result: Vec<ScoredAudioFile> = Vec::new();
        self.search.clone().map(|s| {
            let mut boolean_query: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            boolean_query.push((
                Occur::Should,
                Box::new(
                    QueryParser::for_index(&s.index, s.search_fields.clone())
                        .parse_query(query.as_ref())
                        .unwrap(),
                ),
            ));

            s.search_fields.iter().for_each(|field| {
                boolean_query.push((
                    Occur::Should,
                    Box::new(
                        RegexQuery::from_pattern(
                            format!(".*{}.*", query.as_ref()).as_str(),
                            *field,
                        )
                        .unwrap(),
                    ),
                ));
                boolean_query.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new(
                        Term::from_field_text(*field, query.as_ref()),
                        2,
                        true,
                    )),
                ));
            });
            let search_query = BooleanQuery::new(boolean_query);

            let searcher = &s.searcher;
            let search_result = searcher.search(&search_query, &TopDocs::with_limit(100));

            match search_result {
                Ok(found_items) => {
                    for (score, doc_address) in found_items {
                        match searcher.doc::<TantivyDocument>(doc_address) {
                            Ok(retrieved_doc) => {
                                debug!("Found: {} : {}", score, retrieved_doc.to_json(&s.schema));
                                retrieved_doc.get_first(s.id_field).map(|id_owned| {
                                    match id_owned {
                                        // Find track by id from search result and put in result vec
                                        OwnedValue::U64(id) =>
                                            match self.get_audio_track(*id as u32) {
                                                Ok(af) => result.push(ScoredAudioFile {
                                                    score: score,
                                                    item: af.clone(),
                                                }),
                                                Err(e) => error!("Error while getting audio track: {}", e),
                                            },
                                        _ => (),
                                    }
                                });
                            },
                            Err(_) => todo!(),
                        }
                    }
                }
                Err(e) => error!("Error while searching over tantivy index: {}", e),
            }
        });
        Ok(result)
    }

    pub(crate) fn count_files<P: AsRef<Path>>(&self, dir_path: P) -> Result<usize> {
        let dir = self
            .lister
            .list_dir(&self.base_dir, &dir_path, FoldersOrdering::Alphabetical)
            .map_err(Error::from)
            .unwrap();
        debug!(
            "inner count_files_in_dir {:?}: {}",
            &dir_path.as_ref().display(),
            dir.files.len()
        );
        Ok(dir.files.len())
    }

    pub(crate) fn iter_folders(&self) -> sled::Iter {
        self.db.iter()
    }

    pub(crate) fn clean_up_folders(&self) {
        // for key in self.iter_folders().filter_map(|e| e.ok()).map(|(k, _)| k) {
        //     if let Ok(rel_path) = std::str::from_utf8(&key) {
        //         let full_path = self.base_dir.join(rel_path);
        //         if !full_path.exists() || self.lister.is_collapsable_folder(&full_path) {
        //             debug!("Removing {:?} from collection cache db", full_path);
        //             self.remove(rel_path)
        //                 .map_err(|e| error!("cannot remove record from db: {}", e))
        //                 .ok();
        //         }
        //     }
        // }
    }

    pub(crate) fn get_audio_track(&self, track_id: u32) -> Result<AudioFileInner> {
        match self.db.get(track_id.to_be_bytes()) {
            Ok(ivec) => {
                match ivec {
                    Some(af_ivec) => {
                        match deser_audiofile(af_ivec) {
                            Some(af) => Ok(af),
                            None => Err(Error::CouldNotDeserialize(track_id, TreeType::Root)),
                        }
                    },
                    None => Err(Error::NoIVecForTrackInTree(track_id, TreeType::Root)),
                }
            },
            Err(_) => Err(Error::NoTrackInTree(track_id, TreeType::Root)),
        }
    }

    pub(crate) fn get_track_meta(&self, track_id: u32) -> Result<TrackMeta> {
        match self.meta_tree.get(track_id.to_be_bytes()) {
            Ok(ivec) => {
                match ivec {
                    Some(meta_ivec) => {
                        match deser_trackmeta(meta_ivec) {
                            Some(meta) => Ok(meta),
                            None => Err(Error::CouldNotDeserialize(track_id, TreeType::Meta)),
                        }
                    },
                    None => Err(Error::NoIVecForTrackInTree(track_id, TreeType::Meta)),
                }
            },
            Err(e) => {
                error!("No track {} in tree, {}", track_id, e);
                Err(Error::NoTrackInTree(track_id, TreeType::Meta))
            }
        }
    }

    pub(crate) fn count_tracks(&self) -> Result<u32> {
        Ok(self.db.len() as u32)
    }

    fn increment_inner(old: Option<&[u8]>) -> Option<Vec<u8>> {
        match old {
            Some(bytes) => {
                match deser_trackmeta(bytes) {
                    Some(mut meta) => {
                        meta.increase_played_times();
                        match bincode::serialize(&meta) {
                            Ok(data) => Some(data),
                            Err(e) => {
                                error!("Could not serialize, {}", e);
                                None
                            },
                        }
                    },
                    None => {
                        error!("No data on increment_inner");
                        None
                    }
                }
            }
            None => {
                let track_meta = TrackMeta { played_times: 1, like: None };
                match bincode::serialize(&track_meta) {
                    Ok(data) => Some(data),
                    Err(e) => {
                        error!("Could not serialize: {}", e);
                        None
                    },
                }
            },
        }
    }

    pub(crate) fn increase_played_times(&self, track_id: u32) {
        match self.meta_tree.fetch_and_update(track_id.to_be_bytes(), Self::increment_inner) {
            Ok(_) => {},
            Err(e) => error!("Error while increase_played_times, {}", e),
        }
    }

    pub(crate) fn like(&self, track_id: u32) {
        match self.meta_tree.fetch_and_update(track_id.to_be_bytes(), Self::increment_inner) {
            Ok(_) => {},
            Err(e) => error!("Error while increase_played_times, {}", e),
        }
    }

    pub(crate) fn dislike(&self, track_id: u32) {
        match self.meta_tree.fetch_and_update(track_id.to_be_bytes(), Self::increment_inner) {
            Ok(_) => {},
            Err(e) => error!("Error while increase_played_times, {}", e),
        }
    }

    pub(crate) fn reset_like(&self, track_id: u32) {
        match self.meta_tree.fetch_and_update(track_id.to_be_bytes(), Self::increment_inner) {
            Ok(_) => {},
            Err(e) => error!("Error while increase_played_times, {}", e),
        }
    }

    fn get_audio_file_doc_by_term(search_engine: &SearchEngine, term: &Term) -> Option<TantivyDocument>{
        let term_query = TermQuery::new(term.clone(), IndexRecordOption::Basic);
        let search_result = search_engine.searcher.search(&term_query, &TopDocs::with_limit(1));
        match search_result {
            Ok(result_vec) => {
                if let Some((_score, doc_address)) = result_vec.first() {
                    match search_engine.searcher.doc(*doc_address) {
                        Ok(doc) => Some(doc),
                        Err(_) => None,
                    }
                } else {
                    None
                }
            },
            Err(_) => None,
        }
    }
}

impl CacheInner {
    pub(crate) fn get<P: AsRef<Path>>(&self, dir: P) -> Option<AudioFolderInner> {
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
    ) -> Option<AudioFolderInner> {
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

    pub(crate) fn update<P: AsRef<Path>>(&self, dir: P, af: AudioFolderInner) -> Result<()> {
        let dir = dir.as_ref().to_str().ok_or(Error::InvalidCollectionPath)?;
        bincode::serialize(&af)
            .map_err(Error::from)
            .and_then(|data| self.db.insert(dir, data).map_err(Error::from))
            .map(|_| debug!("Cache updated for {:?}", dir))
    }

    pub(crate) fn force_update<P: AsRef<Path>>(
        &self,
        dir_path: P,
        ret: bool,
    ) -> Result<Option<AudioFolderInner>> {
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
    fn force_update_recursive<P: Into<PathBuf>>(&self, folder: P) {}

    fn update_recursive_after_rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut delete_batch = Batch::default();
        let mut insert_batch = Batch::default();

        let mut updated = get_modified(self.base_dir.join(to));
        debug!("Renamed root modified for {:?}", updated);
        for item in self.db.scan_prefix(from.to_str().unwrap()) {
            // safe to unwrap as we insert only valid strings
            let (k, v) = item?;
            let mut folder_rec: AudioFolderInner = bincode::deserialize(&v)?;
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
}
