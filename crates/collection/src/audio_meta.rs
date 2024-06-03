use crate::collator::Collate;
use crate::error::{Error, Result};
use crate::position::PositionShort;
use crate::util::{get_file_name, get_modified, guess_mime_type};
use mime_guess::Mime;
use serde_derive::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    time::SystemTime,
};
use unicase::UniCase;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, Ord)]
/// This is timestamp is miliseconds from start of Unix epoch
pub struct TimeStamp(u64);

impl TimeStamp {
    pub fn now() -> Self {
        SystemTime::now().into()
    }
}

impl From<SystemTime> for TimeStamp {
    fn from(t: SystemTime) -> Self {
        let milis = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0) as u64;
        TimeStamp(milis)
    }
}

impl From<u64> for TimeStamp {
    fn from(n: u64) -> Self {
        TimeStamp(n)
    }
}

impl<T> PartialEq<T> for TimeStamp
where
    T: Into<TimeStamp> + Copy,
{
    fn eq(&self, other: &T) -> bool {
        self.0 == (*other).into().0
    }
}

impl<T> PartialOrd<T> for TimeStamp
where
    T: Into<TimeStamp> + Copy,
{
    fn partial_cmp(&self, other: &T) -> Option<Ordering> {
        self.0.partial_cmp(&(*other).into().0)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TypedFile {
    pub path: PathBuf,
    pub mime: String,
}

impl TypedFile {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        let path = path.into();
        let mime = guess_mime_type(&path);
        TypedFile {
            path,
            mime: mime.as_ref().into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct FileSection {
    pub start: u64,
    pub duration: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct AudioFileInner {
    pub id: u32,
    #[serde(with = "unicase_serde::unicase")]
    pub name: UniCase<String>,
    pub path: PathBuf,
    pub meta: Option<AudioMeta>,
    pub mime: String,
    pub section: Option<FileSection>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct AudioFile {
    pub id: u32,
    #[serde(with = "unicase_serde::unicase")]
    pub name: UniCase<String>,
    pub parent_dir: Option<String>,
    pub root_subfolder: Option<String>,
    pub meta: Option<AudioMeta>,
    pub mime: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFolderInner {
    pub is_file: bool,
    #[serde(default)]
    pub is_collapsed: bool,
    pub modified: Option<TimeStamp>, // last modification time of this folder
    pub total_time: Option<u32>,     // total playback time of contained audio files
    pub files: Vec<AudioFileInner>,
    pub subfolders: Vec<AudioFolderShort>,
    pub cover: Option<TypedFile>, // cover is file in folder - either jpg or png
    pub description: Option<TypedFile>, // description is file in folder - either txt, html, md
    pub tags: Option<HashMap<String, String>>, // metadata tags, which are applicable for whole folder
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFolder {
    pub is_file: bool,
    #[serde(default)]
    pub is_collapsed: bool,
    pub modified: Option<TimeStamp>, // last modification time of this folder
    pub total_time: Option<u32>,     // total playback time of contained audio files
    pub files: Vec<AudioFile>,
    pub cover: Option<TypedFile>, // cover is file in folder - either jpg or png
    pub description: Option<TypedFile>, // description is file in folder - either txt, html, md
    pub tags: Option<HashMap<String, String>>, // metadata tags, which are applicable for whole folder
}

#[derive(Clone, Copy)]
pub enum FoldersOrdering {
    Alphabetical,
    RecentFirst,
}

impl FoldersOrdering {
    pub fn from_letter(l: &str) -> Self {
        match l {
            "m" => FoldersOrdering::RecentFirst,
            _ => FoldersOrdering::Alphabetical,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct AudioMeta {
    pub duration: u32, // duration in seconds, if available
    pub bitrate: u32,  // bitrate in kB/s
    pub tags: Option<HashMap<String, String>>,
}

#[derive(Clone, Copy, Debug)]
pub struct TimeSpan {
    pub start: u64,
    pub duration: Option<u64>,
}

impl std::fmt::Display for TimeSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        match self.duration {
            Some(d) => write!(f, "{}-{}", self.start, d),
            None => write!(f, "{}", self.start),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct AudioFolderShort {
    #[serde(with = "unicase_serde::unicase")]
    pub name: UniCase<String>,
    pub modified: Option<TimeStamp>,
    pub path: PathBuf,
    pub is_file: bool,
    #[serde(default)]
    pub finished: bool,
}

impl AudioFolderShort {
    pub fn from_path<P: AsRef<Path>>(base_path: &Path, p: P) -> Self {
        let p = p.as_ref();
        AudioFolderShort {
            name: get_file_name(&p).into(),
            path: p.strip_prefix(base_path).unwrap().into(),
            is_file: false,
            modified: None,
            finished: false,
        }
    }

    pub fn from_dir_entry(
        f: &std::fs::DirEntry,
        path: PathBuf,
        is_file: bool,
    ) -> std::result::Result<Self, std::io::Error> {
        Ok(AudioFolderShort {
            path,
            name: f.file_name().to_string_lossy().into(),
            is_file,
            modified: get_modified(f.path()).map(|t| t.into()),
            finished: false,
        })
    }

    pub fn from_path_and_name(name: String, path: PathBuf, is_file: bool) -> Self {
        AudioFolderShort {
            name: name.into(),
            path,
            is_file,
            modified: None,
            finished: false,
        }
    }

    pub fn compare_as(&self, ord: FoldersOrdering, other: &Self) -> Ordering {
        match ord {
            FoldersOrdering::Alphabetical => self.collate(other),
            FoldersOrdering::RecentFirst => match (self.modified, other.modified) {
                (Some(ref a), Some(ref b)) => b.cmp(a),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
        }
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct FolderByModification(AudioFolderShort);

impl From<AudioFolderShort> for FolderByModification {
    fn from(f: AudioFolderShort) -> Self {
        FolderByModification(f)
    }
}

impl From<FolderByModification> for AudioFolderShort {
    fn from(f: FolderByModification) -> Self {
        f.0
    }
}

impl PartialOrd for FolderByModification {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match other.0.modified.partial_cmp(&self.0.modified) {
            Some(Ordering::Equal) => self.0.partial_cmp(&other.0),
            other => other,
        }
    }
}

impl Ord for FolderByModification {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap() // OK cause Option<Timestamp> has Ord
    }
}

pub struct Chapter {
    pub number: u32,
    pub title: String,
    pub start: u64,
    pub end: u64,
}

fn has_subtype(mime: &Mime, subtypes: &[&str]) -> bool {
    subtypes.iter().any(|&s| s == mime.subtype())
}

const AUDIO: &[&str] = &[
    "ogg",
    "mpeg",
    "aac",
    "m4a",
    "m4b",
    "x-matroska",
    "flac",
    "webm",
];
pub fn is_audio<P: AsRef<Path>>(path: P) -> bool {
    let mime = guess_mime_type(path);
    mime.type_() == "audio" && has_subtype(&mime, AUDIO)
}

const COVERS: &[&str] = &["jpeg", "png"];

pub fn is_cover<P: AsRef<Path>>(path: P) -> bool {
    let mime = guess_mime_type(path);
    mime.type_() == "image" && has_subtype(&mime, COVERS)
}

const DESCRIPTIONS: &[&str] = &["html", "plain", "markdown"];

pub fn is_description<P: AsRef<Path>>(path: P) -> bool {
    let mime = guess_mime_type(path);
    mime.type_() == "text" && has_subtype(&mime, DESCRIPTIONS)
}

/// trait to generalize access to media metadata
/// (so that underlying library can be easily changed)
pub trait MediaInfo<'a>: Sized {
    fn get_audio_info(&self, required_tags: &Option<HashSet<String>>) -> Option<AudioMeta>;
    fn get_chapters(&self) -> Option<Vec<Chapter>>;
    fn has_chapters(&self) -> bool;
}

mod libavformat {
    use lofty::{Tag, TagType, ItemKey, AudioFile, TaggedFileExt};

    use super::*;
    use std::{collections::HashSet, sync::Once};

    pub struct Info {
        media_file: media_info::MediaFile,
    }

    impl<'a> MediaInfo<'a> for Info {
        fn get_audio_info(&self, required_tags: &Option<HashSet<String>>) -> Option<AudioMeta> {
            Some(AudioMeta {
                duration: (self.media_file.duration() as f32 / 1000.0).round() as u32,
                bitrate: self.media_file.bitrate(),
                tags: self.collect_tags(required_tags),
            })
        }

        fn has_chapters(&self) -> bool {
            self.media_file.chapters_count() > 1
        }

        fn get_chapters(&self) -> Option<Vec<Chapter>> {
            self.media_file.chapters().map(|l| {
                l.into_iter()
                    .map(|c| Chapter {
                        number: c.number as u32,
                        title: c.title,
                        start: c.start,
                        end: c.end,
                    })
                    .collect()
            })
        }
    }

    impl Info {
        fn collect_tags(
            &self,
            required_tags: &Option<HashSet<String>>,
        ) -> Option<HashMap<String, String>> {
            let mut map: HashMap<String, String> = HashMap::new();
            let tag = match self.media_file.file.primary_tag() {
                Some(primary_tag) => primary_tag,
                // If the "primary" tag doesn't exist, we just grab the
                // first tag we can find. Realistically, a tag reader would likely
                // iterate through the tags to find a suitable one.
                None => match self.media_file.file.first_tag() {
                    Some(first_tag) => first_tag,
                    None => {
                        map.insert("Title".to_string(), self.media_file.file_name.clone());
                        return Some(map);
                    }
                },
            };

            // println!("Title: {}", tag.get_string(&ItemKey::TrackTitle).unwrap_or("None"));
            map.insert("Artist".to_string(), tag.get_string(&ItemKey::TrackArtist).unwrap_or("None").to_string());
            map.insert("Title".to_string(), tag.get_string(&ItemKey::TrackTitle).unwrap_or("None").to_string());
            map.insert("Year".to_string(), tag.get_string(&ItemKey::Year).unwrap_or("None").to_string());
            map.insert("Genre".to_string(), tag.get_string(&ItemKey::Genre).unwrap_or("None").to_string());
            Some(map)
        }

        pub fn from_file(
            path: &Path,
            #[cfg(feature = "tags-encoding")] alternate_encoding: Option<impl AsRef<str>>,
        ) -> Result<Info> {
            match path.as_os_str().to_str() {
                Some(fname) => {
                    #[cfg(feature = "tags-encoding")]
                    let media_file =
                        media_info::MediaFile::open_with_encoding(fname, alternate_encoding)?;

                    #[cfg(not(feature = "tags-encoding"))]
                    let media_file = media_info::MediaFile::open(fname)?;

                    Ok(Info { media_file })
                }
                None => {
                    error!("Invalid file name {:?}, not utf-8", path);
                    Err(Error::InvalidPath)
                }
            }
        }
    }
}

#[cfg(not(feature = "tags-encoding"))]
pub fn get_audio_properties(audio_file_path: &Path) -> Result<impl MediaInfo> {
    libavformat::Info::from_file(audio_file_path)
}

#[cfg(feature = "tags-encoding")]
pub fn get_audio_properties(
    audio_file_path: &Path,
    alternate_encoding: Option<impl AsRef<str>>,
) -> Result<impl MediaInfo> {
    libavformat::Info::from_file(audio_file_path, alternate_encoding)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::util::get_meta;

    use super::*;

    #[test]
    fn test_is_audio() {
        println!("meta: {:?}", get_meta(r"f:\music\!Hard\\Linkin Park\\01-Linkin Park--Wake.mp3").unwrap());
        assert!(is_audio("my/song.mp3"));
        assert!(is_audio("other/chapter.opus"));
        assert!(!is_audio("cover.jpg"));
    }

    #[test]
    fn test_is_cover() {
        assert!(is_cover("cover.jpg"));
        assert!(!is_cover("my/song.mp3"));
    }

    #[test]
    fn test_is_description() {
        assert!(!is_description("cover.jpg"));
        assert!(is_description("about.html"));
        assert!(is_description("about.txt"));
        assert!(is_description("some/folder/text.md"));
    }

    #[test]
    fn test_timestamp() {
        let now = SystemTime::now();
        let now_ts: TimeStamp = now.into();
        let in_future = now + Duration::from_secs(120);
        let in_future_ts: TimeStamp = in_future.into();
        assert!(now_ts < in_future_ts);
        assert!(now_ts < in_future);
        assert!(in_future_ts > now_ts);
        assert!(in_future_ts > now);
    }
}
