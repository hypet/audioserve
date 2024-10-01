use crate::{
    audio_meta::{AudioFolderInner, TimeStamp},
    cache::CollectionCache,
    error::{invalid_option, invalid_option_err, Error, Result},
    position::PositionsCollector,
    AudioFolderShort, FoldersOrdering, Position, AudioFileInner,
};
use enum_dispatch::enum_dispatch;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// Minimum chapter duration for splitting - in minutes
pub const MINIMUM_CHAPTER_DURATION: u32 = 10;

pub enum PositionsData {
    Legacy(()),
    V1(Map<String, Value>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionOptions {
    #[serde(skip)]
    pub no_cache: bool,
    pub ignore_dirs: Option<HashSet<String>>,
    pub flatten_dirs: Option<HashSet<String>>,
    pub chapters_duration: u32,
    pub chapters_from_duration: u32,
    pub ignore_chapters_meta: bool,
    pub allow_symlinks: bool,
    pub no_dir_collaps: bool,
    pub tags: Option<HashSet<String>>,
    #[cfg(feature = "tags-encoding")]
    pub tags_encoding: Option<String>,
    pub cd_folder_regex_str: Option<String>,
    #[serde(skip)]
    pub force_cache_update_on_init: bool,
    #[serde(skip)]
    pub(crate) cd_folder_regex: Option<Regex>,
}

impl PartialEq for CollectionOptions {
    fn eq(&self, other: &Self) -> bool {
        let res = self.chapters_duration == other.chapters_duration
            && self.chapters_from_duration == other.chapters_from_duration
            && self.ignore_chapters_meta == other.ignore_chapters_meta
            && self.allow_symlinks == other.allow_symlinks
            && self.no_dir_collaps == other.no_dir_collaps
            && self.tags == other.tags
            && self.cd_folder_regex_str == other.cd_folder_regex_str;

        #[cfg(feature = "tags-encoding")]
        let res = res && self.tags_encoding == other.tags_encoding;
        res
    }
}

impl Default for CollectionOptions {
    fn default() -> Self {
        Self {
            no_cache: false,
            ignore_dirs: None,
            flatten_dirs: None,
            force_cache_update_on_init: false,
            chapters_duration: 0,
            chapters_from_duration: 30,
            ignore_chapters_meta: false,
            allow_symlinks: false,
            no_dir_collaps: false,
            tags: None,
            #[cfg(feature = "tags-encoding")]
            tags_encoding: None,
            cd_folder_regex_str: None,
            cd_folder_regex: None,
        }
    }
}

impl CollectionOptions {
    pub fn update_from_str_options(&mut self, s: &str) -> Result<()> {
        let options = s.split(',');
        for option in options {
            let mut expr_iter = option.splitn(2, '=').map(|s| s.trim());
            if let Some(tag) = expr_iter.next() {
                let val = expr_iter.next();
                let bool_val = || {
                    val.map(|s| match s.to_ascii_lowercase().as_str() {
                        "true" => Ok(true),
                        "false" => Ok(false),
                        _ => invalid_option!("Invalid value {} for option {}", s, tag),
                    })
                    .unwrap_or(Ok(true))
                };

                let u32_val = || {
                    val.map(|s| {
                        s.parse::<u32>().map_err(|_| {
                            invalid_option_err!("NonInteger value {} for option {}", s, tag)
                        })
                    })
                    .unwrap_or_else(|| invalid_option!("Value is required for option: {}", tag))
                };
                match tag {
                    "flatten-dirs" => {
                        if let Some(dirs) = val {
                            let flatten_dirs = dirs
                                .split('+')
                                .map(|s| { Ok(s.to_string()) })
                                .collect::<Result<HashSet<_>>>()?;
                            self.flatten_dirs = Some(flatten_dirs);
                        } else {
                            invalid_option!("Some dirs are required for {}", tag);
                        }
                    },
                    "ignore-dirs" => {
                        if let Some(dirs) = val {
                            let ignored_dirs = dirs
                                .split('+')
                                .map(|s| { Ok(s.to_string()) })
                                .collect::<Result<HashSet<_>>>()?;
                            self.ignore_dirs = Some(ignored_dirs);
                        } else {
                            invalid_option!("Some dirs are required for {}", tag);
                        }
                    },
                    "nc" | "no-cache" => self.no_cache = bool_val()?,
                    "force-cache-update" => self.force_cache_update_on_init = bool_val()?,
                    "ignore-chapters-meta" => self.ignore_chapters_meta = bool_val()?,
                    "allow-symlinks" => self.allow_symlinks = bool_val()?,
                    "no-dir-collaps" => self.no_dir_collaps = bool_val()?,
                    "chapters-duration" => {
                        let val = u32_val()?;
                        if val < MINIMUM_CHAPTER_DURATION {
                            invalid_option!("Option {} has invalid value - value {} is below limit for reasonable chapter size", tag, val);
                        }
                        self.chapters_duration = val;
                    }
                    "chapters-from-duration" => {
                        let val = u32_val()?;
                        if val > 0 && val < MINIMUM_CHAPTER_DURATION {
                            invalid_option!("Option {} has invalid value - value {} is below limit for reasonable chapter size", tag, val);
                        }
                        self.chapters_from_duration = val;
                    }
                    "tags" => {
                        if let Some(tags) = val {
                            let tags = tags
                                .split('+')
                                .map(|s| s.trim().to_ascii_lowercase())
                                .map(|s| { Ok(s) })
                                //     if ALLOWED_TAGS.contains(&s.as_str()) {
                                //         Ok(s)
                                //     } else {
                                //         invalid_option!("This tag {} is not allowed", s);
                                //     }
                                // })
                                .collect::<Result<HashSet<_>>>()?;
                            self.tags = Some(tags);
                        } else {
                            invalid_option!("Some tags are required for {}", tag);
                        }
                    }
                    "default-tags" => {
                        if bool_val()? {
                            self.tags = None
                            // self.tags = Some(BASIC_TAGS.iter().map(|i| i.to_string()).collect())
                        } else {
                            self.tags = None
                        }
                    }
                    #[cfg(feature = "tags-encoding")]
                    tag @ "tags-encoding" => {
                        if let Some(v) = val {
                            self.tags_encoding = Some(v.into())
                        } else {
                            invalid_option!("Encoding name is required for {}", tag);
                        }
                    }

                    opt => invalid_option!("Unknown option: {}", opt),
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct CollectionOptionsMap {
    cols: HashMap<PathBuf, CollectionOptions>,
    default: CollectionOptions,
}

impl CollectionOptionsMap {
    pub fn new(mut default: CollectionOptions) -> Result<Self> {
        if let Some(ref re) = default.cd_folder_regex_str {
            default.cd_folder_regex = Some(
                RegexBuilder::new(re)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| (Error::InvalidCDFolderRegex(re.into(), e)))?,
            );
        }
        Ok(CollectionOptionsMap {
            cols: HashMap::new(),
            default,
        })
    }

    pub fn add_col_options(&mut self, path: impl Into<PathBuf>, col_options: &str) -> Result<()> {
        let mut col_opt = self.default.clone();
        col_opt.update_from_str_options(col_options)?;
        self.cols.insert(path.into(), col_opt);
        Ok(())
    }

    pub fn get_col_options(&mut self, path: impl AsRef<Path>) -> CollectionOptions {
        self.cols
            .remove(path.as_ref())
            .unwrap_or_else(|| self.default.clone())
    }
}

#[enum_dispatch(CollectionTrait, PositionsTrait)]
pub(crate) enum Collection {
    CollectionCache,
}

#[enum_dispatch]
pub(crate) trait PositionsTrait {
    fn insert_position<S, P>(
        &self,
        group: S,
        path: P,
        position: f32,
        finished: bool,
        ts: Option<TimeStamp>,
    ) -> Result<()>
    where
        S: AsRef<str>,
        P: AsRef<str>;

    fn get_position<S, P>(&self, group: S, folder: Option<P>) -> Option<Position>
    where
        S: AsRef<str>,
        P: AsRef<str>;

    fn get_positions_recursive<S, P>(
        &self,
        group: S,
        folder: P,
        collection_no: usize,
        res: &mut PositionsCollector,
    ) where
        S: AsRef<str>,
        P: AsRef<str>;

    fn get_all_positions_for_group<S>(
        &self,
        group: S,
        collection_no: usize,
        res: &mut PositionsCollector,
    ) where
        S: AsRef<str>;

    fn write_json_positions<F: std::io::Write>(&self, file: &mut F) -> Result<()>;

    fn read_json_positions(&self, data: PositionsData) -> Result<()>;
}

#[enum_dispatch]
pub(crate) trait CollectionTrait {
    fn list_dir<P>(
        &self,
        dir_path: P,
        ordering: FoldersOrdering,
        group: Option<String>,
    ) -> Result<AudioFolderInner>
    where
        P: AsRef<Path>;

    fn list_all(
        &self,
    ) -> Result<AudioFolderInner>;

    fn count_tracks(
        &self,
    ) -> Result<u32>;

    fn get_audio_track(
        &self,
        track_id: u32,
    ) -> Result<AudioFileInner>;

    fn count_files_in_dir<P>(
        &self,
        dir_path: P
    ) -> Result<usize>
    where
        P: AsRef<Path>;

    fn flush(&self) -> Result<()>;

    fn search<S: AsRef<str>>(&self, q: S, group: Option<String>) -> Result<AudioFolderInner>;

    fn recent(&self, limit: usize, group: Option<String>) -> Vec<AudioFolderShort>;

    fn signal_rescan(&self);

    fn base_dir(&self) -> &Path;

    fn increase_played_times(
        &self,
        track_id: u32,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_col_options() {
        let mut opt = CollectionOptions::default();
        opt.update_from_str_options("no-cache,force-cache-update=true,ignore-chapters-meta=false,allow-symlinks,no-dir-collaps=TRUE,ignore-dirs=SomeMusic").expect("good options");
        assert!(opt.no_cache);
        assert!(opt.force_cache_update_on_init);
        assert!(!opt.ignore_chapters_meta);
        assert!(opt.allow_symlinks);
        assert!(opt.no_dir_collaps);

        opt.update_from_str_options("tags=title+album+composer")
            .expect("valid tags");
        assert_eq!(3, opt.tags.as_ref().unwrap().len());

        opt.update_from_str_options("chapters-duration=44,chapters-from-duration=200")
            .expect("correct options");
        assert_eq!(44, opt.chapters_duration);
        assert_eq!(200, opt.chapters_from_duration);
    }
}
