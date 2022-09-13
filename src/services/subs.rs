//#[cfg(feature = "folder-download")]
use super::{
    resp,
    search::{Search, SearchTrait},
    types::*,
};
use crate::{
    config::get_config,
    error::{Error, Result},
    util::{checked_dec, into_range_bounds, to_satisfiable_range, ResponseBuilderExt},
};
use collection::{guess_mime_type, parse_chapter_path, FoldersOrdering};
use futures::prelude::*;
use futures::{future, ready, Stream};
use headers::{AcceptRanges, CacheControl, ContentLength, ContentRange, ContentType, LastModified};
use hyper::{Body, Response as HyperResponse, StatusCode};
use std::{
    collections::Bound,
    ffi::OsStr,
    io::{self, SeekFrom},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll}, env,
};
use tokio::{
    io::{AsyncRead, AsyncSeekExt, ReadBuf},
    task::spawn_blocking as blocking,
};

pub type ByteRange = (Bound<u64>, Bound<u64>);
type Response = HyperResponse<Body>;
pub type ResponseFuture = Pin<Box<dyn Future<Output = Result<Response, Error>> + Send>>;

pub struct ChunkStream<T> {
    src: Option<T>,
    remains: u64,
    buf: [u8; 8 * 1024],
}

impl<T: AsyncRead + Unpin> Stream for ChunkStream<T> {
    type Item = Result<Vec<u8>, io::Error>;

    fn poll_next(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();
        if let Some(ref mut src) = pin.src {
            if pin.remains == 0 {
                pin.src.take();
                return Poll::Ready(None);
            }
            let mut buf = ReadBuf::new(&mut pin.buf[..]);
            match ready! {
                {
                let pinned_stream = Pin::new(src);
                pinned_stream.poll_read(ctx, &mut buf)
                }
            } {
                Ok(_) => {
                    let read = buf.filled().len();
                    if read == 0 {
                        pin.src.take();
                        Poll::Ready(None)
                    } else {
                        let to_send = pin.remains.min(read as u64);
                        pin.remains -= to_send;
                        let chunk = pin.buf[..to_send as usize].to_vec();
                        Poll::Ready(Some(Ok(chunk)))
                    }
                }
                Err(e) => Poll::Ready(Some(Err(e))),
            }
        } else {
            error!("Polling after stream is done");
            Poll::Ready(None)
        }
    }
}

impl<T: AsyncRead> ChunkStream<T> {
    pub fn new(src: T) -> Self {
        ChunkStream::new_with_limit(src, std::u64::MAX)
    }
    pub fn new_with_limit(src: T, remains: u64) -> Self {
        ChunkStream {
            src: Some(src),
            remains,
            buf: [0u8; 8 * 1024],
        }
    }
}

async fn serve_opened_file(
    mut file: tokio::fs::File,
    range: Option<ByteRange>,
    caching: Option<u32>,
    mime: mime::Mime,
) -> Result<Response, io::Error> {
    let meta = file.metadata().await?;
    let file_len = meta.len();
    if file_len == 0 {
        warn!("File has zero size ")
    }
    let last_modified = meta.modified().ok();
    let mut resp = HyperResponse::builder().typed_header(ContentType::from(mime));
    if let Some(age) = caching {
        if age > 0 {
            let cache = CacheControl::new()
                .with_public()
                .with_max_age(std::time::Duration::from_secs(u64::from(age)));
            resp = resp.typed_header(cache);
        }
        if let Some(last_modified) = last_modified {
            resp = resp.typed_header(LastModified::from(last_modified));
        }
    } else {
        resp = resp.typed_header(CacheControl::new().with_no_store());
    }

    let (start, end) = match range {
        Some(range) => match to_satisfiable_range(range, file_len) {
            Some(l) => {
                resp = resp.status(StatusCode::PARTIAL_CONTENT).typed_header(
                    ContentRange::bytes(into_range_bounds(l), Some(file_len)).unwrap(),
                );
                l
            }
            None => {
                error!("Wrong range {:?}", range);
                (0, checked_dec(file_len))
            }
        },
        None => {
            resp = resp
                .status(StatusCode::OK)
                .typed_header(AcceptRanges::bytes());
            (0, checked_dec(file_len))
        }
    };
    let _pos = file.seek(SeekFrom::Start(start)).await;
    let sz = end - start + 1;
    let stream = ChunkStream::new_with_limit(file, sz);
    let resp = resp
        .typed_header(ContentLength(sz))
        .body(Body::wrap_stream(stream))
        .unwrap();
    Ok(resp)
}

fn serve_file_from_fs(
    full_path: &Path,
    range: Option<ByteRange>,
    caching: Option<u32>,
) -> ResponseFuture {
    let base_dir = env::current_dir();
    let filename: PathBuf = base_dir.unwrap().join(full_path).into();
    trace!("Requested static file: {:?}", filename);
    let fut = async move {
        match tokio::fs::File::open(&filename).await {
            Ok(file) => {
                let mime = guess_mime_type(&filename);
                serve_opened_file(file, range, caching, mime)
                    .await
                    .map_err(Error::new)
            }
            Err(e) => {
                error!("Error when sending file {:?} : {}", filename, e);
                Ok(resp::not_found())
            }
        }
    };
    Box::pin(fut)
}

pub fn send_file_simple<P: AsRef<Path>>(
    base_path: &'static Path,
    file_path: P,
    cache: Option<u32>,
) -> ResponseFuture {
    let full_path = base_path.join(&file_path);
    serve_file_from_fs(&full_path, None, cache)
}

pub fn send_file<P: AsRef<Path>>(
    base_path: &'static Path,
    file_path: P,
    range: Option<ByteRange>,
    seek: Option<f32>
) -> ResponseFuture {
    let (real_path, span) = parse_chapter_path(file_path.as_ref());
    let full_path = base_path.join(real_path);
    trace!("Sending file directly from fs");
    serve_file_from_fs(&full_path, range, None)
}

pub fn get_folder(
    collection: usize,
    folder_path: PathBuf,
    collections: Arc<collection::Collections>,
    ordering: FoldersOrdering,
    group: Option<String>,
) -> ResponseFuture {
    Box::pin(
        blocking(move || collections.list_dir(collection, &folder_path, ordering, group))
            .map_ok(|res| match res {
                Ok(folder) => json_response(&folder),
                Err(_) => resp::not_found(),
            })
            .map_err(Error::new),
    )
}

#[cfg(feature = "folder-download")]
pub fn download_folder(
    base_path: &'static Path,
    folder_path: PathBuf,
    format: DownloadFormat,
    include_subfolders: Option<regex::Regex>,
) -> ResponseFuture {
    use anyhow::Context;
    use hyper::header::CONTENT_DISPOSITION;
    let full_path = base_path.join(&folder_path);
    let f = async move {
        let meta = tokio::fs::metadata(&full_path)
            .await
            .context("metadata for folder download")?;
        if meta.is_file() {
            serve_file_from_fs(&full_path, None, None).await
        } else {
            let mut download_name = folder_path
                .file_name()
                .and_then(OsStr::to_str)
                .map(std::borrow::ToOwned::to_owned)
                .unwrap_or_else(|| "audio".into());

            download_name.push_str(format.extension());

            match blocking(move || {
                let allow_symlinks = get_config().allow_symlinks;
                if let Some(folder_re) = include_subfolders {
                    collection::list_dir_files_with_subdirs(
                        &base_path,
                        &folder_path,
                        allow_symlinks,
                        folder_re,
                    )
                } else {
                    collection::list_dir_files_only(&base_path, &folder_path, allow_symlinks)
                }
            })
            .await
            {
                Ok(Ok(folder)) => {
                    let total_len: u64 = match format {
                        DownloadFormat::Tar => {
                            let lens_iter = folder.iter().map(|i| i.2);
                            async_tar::calc_size(lens_iter)
                        }
                        DownloadFormat::Zip => {
                            let iter = folder
                                .iter()
                                .map(|&(ref path, ref name, len)| (path, name.as_str(), len));
                            async_zip::calc_size(iter).context("calc zip size")?
                        }
                    };

                    debug!("Total len of folder is {:?}", total_len);

                    let stream: Box<dyn Stream<Item = _> + Unpin + Send> = match format {
                        DownloadFormat::Tar => {
                            let files = folder.into_iter().map(|i| i.0);
                            Box::new(async_tar::TarStream::tar_iter(files))
                        }
                        DownloadFormat::Zip => {
                            let files = folder.into_iter().map(|i| (i.0, i.1));
                            let zipper = async_zip::Zipper::from_iter(files);
                            Box::new(zipper.zipped_stream())
                        }
                    };

                    let disposition = format!("attachment; filename=\"{}\"", download_name);
                    let builder = HyperResponse::builder()
                        .typed_header(ContentType::from(format.mime()))
                        .header(CONTENT_DISPOSITION, disposition.as_bytes())
                        .typed_header(ContentLength(total_len));
                    Ok(builder.body(Body::wrap_stream(stream)).unwrap())
                }
                Ok(Err(e)) => Err(Error::new(e).context("listing directory")),
                Err(e) => Err(Error::new(e).context("spawn blocking directory")),
            }
        }
    };

    Box::pin(f)
}

fn json_response<T: serde::Serialize>(data: &T) -> Response {
    let json = serde_json::to_string(data).expect("Serialization error");

    HyperResponse::builder()
        .typed_header(ContentType::json())
        .typed_header(ContentLength(json.len() as u64))
        .body(json.into())
        .unwrap()
}

const UNKNOWN_NAME: &str = "unknown";

pub fn collections_list() -> ResponseFuture {
    let collections = CollectionsInfo {
        version: env!("CARGO_PKG_VERSION"),
        folder_download: !get_config().disable_folder_download,
        shared_positions: cfg!(feature = "shared-positions"),
        count: get_config().base_dirs.len() as u32,
        names: get_config()
            .base_dirs
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or(UNKNOWN_NAME)
            })
            .collect(),
    };
    debug!("Sending collections: {}", &collections.count);
    Box::pin(future::ok(json_response(&collections)))
}

#[cfg(feature = "shared-positions")]
pub fn insert_position(
    collections: Arc<collection::Collections>,
    group: String,
    bytes: bytes::Bytes,
) -> ResponseFuture {
    match serde_json::from_slice::<collection::Position>(&bytes) {
        Ok(pos) => {
            let path = if !pos.folder.is_empty() {
                pos.folder + "/" + &pos.file
            } else {
                pos.file
            };
            Box::pin(
                collections
                    .insert_position_if_newer_async(
                        pos.collection,
                        group,
                        path,
                        pos.position,
                        pos.folder_finished,
                        pos.timestamp,
                    )
                    .then(|res| match res {
                        Ok(_) => resp::fut(resp::created),
                        Err(e) => match e {
                            collection::error::Error::IgnoredPosition => resp::fut(resp::ignored),
                            _ => Box::pin(future::err(Error::new(e))),
                        },
                    }),
            )
        }
        Err(e) => {
            error!("Error in position JSON: {}", e);
            resp::fut(resp::bad_request)
        }
    }
}

#[cfg(feature = "shared-positions")]
pub fn last_position(collections: Arc<collection::Collections>, group: String) -> ResponseFuture {
    Box::pin(
        collections
            .get_last_position_async(group)
            .map(|pos| Ok(json_response(&pos))),
    )
}

#[cfg(feature = "shared-positions")]
pub fn folder_position(
    collections: Arc<collection::Collections>,
    group: String,
    collection: usize,
    path: String,
    recursive: bool,
    filter: Option<collection::PositionFilter>,
) -> ResponseFuture {
    if recursive {
        Box::pin(
            collections
                .get_positions_recursive_async(collection, group, path, filter)
                .map(|pos| Ok(json_response(&pos))),
        )
    } else {
        Box::pin(
            collections
                .get_position_async(collection, group, path)
                .map(|pos| Ok(json_response(&pos))),
        )
    }
}

#[cfg(feature = "shared-positions")]
pub fn all_positions(
    collections: Arc<collection::Collections>,
    group: String,
    filter: Option<collection::PositionFilter>,
) -> ResponseFuture {
    Box::pin(
        collections
            .get_all_positions_for_group_async(group, filter)
            .map(|pos| Ok(json_response(&pos))),
    )
}

pub fn search(
    collection: usize,
    searcher: Search<String>,
    query: String,
    ordering: FoldersOrdering,
    group: Option<String>,
) -> ResponseFuture {
    Box::pin(
        blocking(move || {
            let res = searcher.search(collection, query, ordering, group);
            json_response(&res)
        })
        .map_err(Error::new),
    )
}

pub fn recent(
    collection: usize,
    searcher: Search<String>,
    group: Option<String>,
) -> ResponseFuture {
    Box::pin(
        blocking(move || {
            let res = searcher.recent(collection, group);
            json_response(&res)
        })
        .map_err(Error::new),
    )
}
