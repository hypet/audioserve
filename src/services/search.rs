use super::types::SearchResult;
use collection::FoldersOrdering;
use std::sync::Arc;

pub trait SearchTrait<S> {
    fn search(
        &self,
        collection: usize,
        query: S,
        ordering: FoldersOrdering,
        group: Option<String>,
    ) -> SearchResult;

    fn recent(&self, collection: usize, group: Option<String>) -> SearchResult;
}

#[derive(Clone)]
pub struct Search<S> {
    inner: Arc<dyn SearchTrait<S> + Send + Sync>,
}

impl<S: AsRef<str>> SearchTrait<S> for Search<S> {
    fn search(
        &self,
        collection: usize,
        query: S,
        ordering: FoldersOrdering,
        group: Option<String>,
    ) -> SearchResult {
        self.inner.search(collection, query, ordering, group)
    }

    fn recent(&self, collection: usize, group: Option<String>) -> SearchResult {
        self.inner.recent(collection, group)
    }
}

impl<S: AsRef<str>> Search<S> {
    pub fn new(collections: Option<Arc<collection::Collections>>) -> Self {
        Search {
            inner: Arc::new(col_db::CollectionsSearch::new(collections.unwrap())),
        }
    }
}

mod col_db {
    use collection::{audio_meta::{AudioFile, TrackMeta}, Collections};

    use crate::services::{subs::pathbuf_to_str_vec, types::ScoredAudioFile};

    use super::*;

    pub struct CollectionsSearch {
        collections: Arc<Collections>,
    }

    impl CollectionsSearch {
        pub fn new(collections: Arc<Collections>) -> Self {
            CollectionsSearch { collections }
        }
    }

    impl<T: AsRef<str>> SearchTrait<T> for CollectionsSearch {
        fn search(
            &self,
            collection: usize,
            query: T,
            ordering: FoldersOrdering,
            group: Option<String>,
        ) -> SearchResult {
            let collections_track_meta = self.collections.clone();
            SearchResult {
                subfolders: vec![],
                files: self
                    .collections
                    .search(collection, query, ordering, group)
                    .map_err(|e| error!("Error in collections search: {}", e))
                    .map(|res| res.iter().map(|scored_af| ScoredAudioFile {
                        score: scored_af.score,
                        item: AudioFile {
                            id: scored_af.item.id,
                            name: scored_af.item.name.clone(),
                            path: pathbuf_to_str_vec(&scored_af.item.path),
                            meta: scored_af.item.meta.clone(),
                            mime: scored_af.item.mime.clone(),
                            track_meta: collections_track_meta.get_track_meta(collection, scored_af.item.id).map_or(TrackMeta::default() , |v| v),
                        }
                    }).collect())
                    .unwrap_or_else(|_| vec![]),
            }
        }

        fn recent(&self, collection: usize, group: Option<String>) -> SearchResult {
            let res = self
                .collections
                .recent(collection, 100, group)
                .map_err(|e| error!("Cannot get recents from coolection db: {}", e))
                .unwrap_or_else(|_| vec![]);
            SearchResult {
                files: vec![],
                subfolders: res,
            }
        }
    }
}
