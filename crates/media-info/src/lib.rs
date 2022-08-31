use std::path::Path;

use lofty::{Accessor, AudioFile, Probe, TaggedFile};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("libav error code {0}")]
    AVError(i32),
    #[error("memory allocation error - maybe full memory")]
    AllocationError,

    #[error("UTF8 error: {0}")]
    InvalidString(#[from] std::str::Utf8Error),

    #[error("Invalid encoding name {0}")]
    InvalidEncoding(String),

    #[error("Invalid file {0}")]
    InvalidFile(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Chapter {
    pub number: u32,
    pub title: String,
    pub start: u64,
    pub end: u64,
}

pub struct MediaFile {
    pub file: TaggedFile,
    pub file_name: String
}

impl MediaFile {
    
    pub fn open(fname: &str) -> Result<Self> {
        let path = Path::new(fname);

        let tagged_file_res = Probe::open(path)
            .expect("ERROR: Bad path provided!")
            .read(true);
        let tagged_file = match tagged_file_res {
            Ok(file) => file,
            Err(e) => {
                return Err(Error::InvalidFile(fname.to_string()));
            },
        };

        Ok(MediaFile { file: tagged_file, file_name: path.file_name().unwrap().to_str().unwrap().to_string() })
    }

    pub fn duration(&self) -> u64 {
        self.file.properties().duration().as_millis() as u64
    }

    pub fn bitrate(&self) -> u32 {
        self.file.properties().audio_bitrate().unwrap()
    }

    pub fn chapters_count(&self) -> usize {
        1
    }

    pub fn chapters(&self) -> Option<Vec<Chapter>> {
        None
    }


}
