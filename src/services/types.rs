use super::transcode::TranscodingFormat;
use collection::{AudioFile, AudioFolderShort};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct CollectionsInfo {
    pub version: &'static str,
    pub folder_download: bool,
    pub shared_positions: bool,
    pub count: u32,
    pub names: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct TranscodingSummary {
    bitrate: u32,
    name: &'static str,
}

impl From<TranscodingFormat> for TranscodingSummary {
    fn from(f: TranscodingFormat) -> Self {
        TranscodingSummary {
            bitrate: f.bitrate(),
            name: f.format_name(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub files: Vec<AudioFile>,
    pub subfolders: Vec<AudioFolderShort>,
}

#[cfg(feature = "folder-download")]
pub use download_format::DownloadFormat;

#[cfg(feature = "folder-download")]
mod download_format {

    use crate::error::Error;
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum DownloadFormat {
        Tar,
        Zip,
    }

    impl DownloadFormat {
        pub fn extension(&self) -> &'static str {
            match self {
                DownloadFormat::Tar => ".tar",
                DownloadFormat::Zip => ".zip",
            }
        }

        pub fn mime(&self) -> mime::Mime {
            match self {
                DownloadFormat::Tar => "application/x-tar".parse().unwrap(),
                DownloadFormat::Zip => "application/zip".parse().unwrap(),
            }
        }
    }

    impl std::str::FromStr for DownloadFormat {
        type Err = Error;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            match s {
                "tar" => Ok(DownloadFormat::Tar),
                "zip" => Ok(DownloadFormat::Zip),
                _ => Err(Error::msg("Invalid download archive format tag")),
            }
        }
    }

    impl Default for DownloadFormat {
        fn default() -> Self {
            if cfg!(feature = "folder-download-default-tar") {
                DownloadFormat::Tar
            } else {
                DownloadFormat::Zip
            }
        }
    }
}
