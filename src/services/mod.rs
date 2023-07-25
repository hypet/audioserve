use self::auth::{AuthResult, Authenticator};
use self::search::Search;
use self::subs::{
    collections_list, get_folder, get_all, recent, search, send_file, send_file_simple, ResponseFuture,
};
use crate::config::get_config;
use crate::util::ResponseBuilderExt;
use crate::{error, util::header2header, Error};
use bytes::{Bytes, BytesMut};
use collection::{Collections, FoldersOrdering};
use dashmap::DashMap;
use futures::prelude::*;
use futures::{future, TryFutureExt};
use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::{pin_mut, stream::TryStreamExt, StreamExt};
use headers::{
    AccessControlAllowCredentials, AccessControlAllowHeaders, AccessControlAllowMethods,
    AccessControlAllowOrigin, AccessControlMaxAge, AccessControlRequestHeaders, HeaderMapExt,
    Origin, Range, UserAgent,
};
use hyper::body::HttpBody;
use hyper::{
    header::{
        HeaderValue, CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION,
        UPGRADE,
    },
    service::{Service},
    upgrade::Upgraded,
    Body, Method, Request, Response, StatusCode, 
};
use leaky_cauldron::Leaky;
use percent_encoding::percent_decode;
use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use std::iter::FromIterator;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    fmt::Display,
    net::IpAddr,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc},
    task::Poll,
};
use tokio_tungstenite::WebSocketStream;
use url::form_urlencoded;

pub mod auth;
#[cfg(feature = "shared-positions")]
pub mod position;
pub mod resp;
pub mod search;
mod subs;
pub mod transcode;
mod types;

type Tx = UnboundedSender<Message>;

#[derive(Debug)]
pub enum RemoteIpAddr {
    Direct(IpAddr),
    #[allow(dead_code)]
    Proxied(IpAddr),
}

#[derive(Clone, Debug)]
struct Device {
    name: Option<String>,
    id: String,
    ws: Tx,
    active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClientDevice {
    name: String,
    id: String,
    active: bool,
}

#[derive(Debug)]
pub struct Devices {
    map: DashMap<SocketAddr, Device>,
    shuffle_mode: Mutex<ShuffleMode>,
}

impl Devices {
    pub fn new() -> Self {
        Devices {
            map: DashMap::new(),
            shuffle_mode: Mutex::new(ShuffleMode::Off),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
enum ShuffleMode {
    Off,
    CurrentDir,
    CollectionWide,
    Global,
}

#[derive(Debug, Serialize, Deserialize)]
enum MsgIn {
    RegisterDevice { name: String },
    MakeDeviceActive { device_id: String },
    PlayTrack { collection: usize, track_id: u32 },
    NextTrack { collection: usize, },
    Pause {},
    Resume {},
    SwitchShuffle { mode: ShuffleMode },
    CurrentPos { collection: usize, track_id: u32, time: f32 },
    RewindTo { time: f32 },
    VolumeChange { value: u8 },
}

impl FromStr for MsgIn {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: MsgIn = serde_json::from_str(s)?;
        Ok(v)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum MsgOut {
    RegisterDeviceEvent { device_id: String },
    DevicesOnlineEvent { devices: Vec<ClientDevice> },
    MakeDeviceActiveEvent { device_id: String },
    PlayTrackEvent { collection: usize, track_id: u32 },
    NextTrackEvent { track: String },
    RewindTrackToTime {},
    PauseEvent {},
    ResumeEvent {},
    SwitchShuffleEvent { mode: ShuffleMode },
    CurrentPosEvent { collection: usize, track_id: u32, time: f32 },
    RewindToEvent { time: f32 },
    VolumeChangeEvent { value: u8 },
}

#[derive(Debug, Serialize, Deserialize)]
struct Msg(MsgOut);

impl AsRef<IpAddr> for RemoteIpAddr {
    fn as_ref(&self) -> &IpAddr {
        match self {
            RemoteIpAddr::Direct(a) => a,
            RemoteIpAddr::Proxied(a) => a,
        }
    }
}

impl Display for RemoteIpAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteIpAddr::Direct(a) => a.fmt(f),
            RemoteIpAddr::Proxied(a) => write!(f, "Proxied: {}", a),
        }
    }
}

pub struct QueryParams<'a> {
    params: Option<HashMap<Cow<'a, str>, Cow<'a, str>>>,
}

impl<'a> QueryParams<'a> {
    pub fn get<S: AsRef<str>>(&self, name: S) -> Option<&Cow<'_, str>> {
        self.params.as_ref().and_then(|m| m.get(name.as_ref()))
    }

    pub fn exists<S: AsRef<str>>(&self, name: S) -> bool {
        self.get(name).is_some()
    }

    pub fn get_string<S: AsRef<str>>(&self, name: S) -> Option<String> {
        self.get(name).map(|s| s.to_string())
    }
}

pub struct RequestWrapper {
    request: Request<Body>,
    path: String,
    remote_addr: IpAddr,
    #[allow(dead_code)]
    is_ssl: bool,
    #[allow(dead_code)]
    is_behind_proxy: bool,
}

impl RequestWrapper {
    pub fn new(
        request: Request<Body>,
        path_prefix: Option<&str>,
        remote_addr: IpAddr,
        is_ssl: bool,
    ) -> error::Result<Self> {
        let path = match percent_decode(request.uri().path().as_bytes()).decode_utf8() {
            Ok(s) => s.into_owned(),
            Err(e) => {
                return Err(error::Error::msg(format!(
                    "Invalid path encoding, not UTF-8: {}",
                    e
                )))
            }
        };
        //Check for unwanted path segments - e.g. ., .., .anything - so we do not want special directories and hidden directories and files
        let mut segments = path.split('/');
        if segments.any(|s| s.starts_with('.')) {
            return Err(error::Error::msg(
                "Illegal path, contains either special directories or hidden name",
            ));
        }

        let path = match path_prefix {
            Some(p) => match path.strip_prefix(p) {
                Some(s) => {
                    if s.is_empty() {
                        "/".to_string()
                    } else {
                        s.to_string()
                    }
                }
                None => {
                    error!("URL path is missing prefix {}", p);
                    return Err(error::Error::msg(format!(
                        "URL path is missing prefix {}",
                        p
                    )));
                }
            },
            None => path,
        };
        let is_behind_proxy = get_config().behind_proxy;
        Ok(RequestWrapper {
            request,
            path,
            remote_addr,
            is_ssl,
            is_behind_proxy,
        })
    }

    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    pub fn remote_addr(&self) -> Option<RemoteIpAddr> {
        #[cfg(feature = "behind-proxy")]
        if self.is_behind_proxy {
            return self
                .request
                .headers()
                .typed_get::<proxy_headers::Forwarded>()
                .and_then(|fwd| fwd.client().copied())
                .map(RemoteIpAddr::Proxied)
                .or_else(|| {
                    self.request
                        .headers()
                        .typed_get::<proxy_headers::XForwardedFor>()
                        .map(|xfwd| RemoteIpAddr::Proxied(*xfwd.client()))
                });
        }
        Some(RemoteIpAddr::Direct(self.remote_addr))
    }

    pub fn headers(&self) -> &hyper::HeaderMap {
        self.request.headers()
    }

    pub fn method(&self) -> &hyper::Method {
        self.request.method()
    }

    #[allow(dead_code)]
    pub fn into_body(self) -> Body {
        self.request.into_body()
    }

    pub async fn body_bytes(&mut self) -> Result<Bytes, hyper::Error> {
        let first = self.request.body_mut().data().await;
        match first {
            Some(Ok(data)) => {
                let mut buf = BytesMut::from(&data[..]);
                while let Some(res) = self.request.body_mut().data().await {
                    let next = res?;
                    buf.extend_from_slice(&next);
                }
                Ok(buf.into())
            }
            Some(Err(e)) => Err(e),
            None => Ok(Bytes::new()),
        }
    }

    #[allow(dead_code)]
    pub fn into_request(self) -> Request<Body> {
        self.request
    }

    pub fn params(&self) -> QueryParams<'_> {
        QueryParams {
            params: self
                .request
                .uri()
                .query()
                .map(|query| form_urlencoded::parse(query.as_bytes()).collect::<HashMap<_, _>>()),
        }
    }

    pub fn is_https(&self) -> bool {
        if self.is_ssl {
            return true;
        }
        #[cfg(feature = "behind-proxy")]
        if self.is_behind_proxy {
            //try scommon  proxy headers
            let forwarded_https = self
                .request
                .headers()
                .typed_get::<proxy_headers::Forwarded>()
                .and_then(|fwd| fwd.client_protocol().map(|p| p.as_ref() == "https"))
                .unwrap_or(false);

            if forwarded_https {
                return true;
            }

            return self
                .request
                .headers()
                .get("X-Forwarded-Proto")
                .map(|v| v.as_bytes() == b"https")
                .unwrap_or(false);
        }
        false
    }
}

pub struct ServiceFactory<T> {
    authenticator: Option<Arc<dyn Authenticator<Credentials = T>>>,
    rate_limitter: Option<Arc<Leaky>>,
    search: Search<String>,
    collections: Arc<Collections>,
    devices: Arc<Devices>,
}

impl<T> ServiceFactory<T> {
    pub fn new<A>(
        auth: Option<A>,
        search: Search<String>,
        collections: Arc<Collections>,
        devices: Arc<Devices>,
        rate_limit: Option<f32>,
    ) -> Self
    where
        A: Authenticator<Credentials = T> + 'static,
    {
        ServiceFactory {
            authenticator: auth.map(|a| Arc::new(a) as Arc<dyn Authenticator<Credentials = T>>),
            rate_limitter: rate_limit.map(|l| Arc::new(Leaky::new(l))),
            search,
            collections,
            devices,
        }
    }

    pub fn create(
        &self,
        remote_addr: SocketAddr,
        is_ssl: bool,
    ) -> impl Future<Output = Result<FileSendService<T>, Infallible>> {
        future::ok(FileSendService {
            authenticator: self.authenticator.clone(),
            rate_limitter: self.rate_limitter.clone(),
            search: self.search.clone(),
            collections: self.collections.clone(),
            devices: self.devices.clone(),
            remote_addr,
            is_ssl,
        })
    }
}

#[derive(Clone)]
pub struct FileSendService<T> {
    pub authenticator: Option<Arc<dyn Authenticator<Credentials = T>>>,
    pub rate_limitter: Option<Arc<Leaky>>,
    pub search: Search<String>,
    pub collections: Arc<Collections>,
    pub devices: Arc<Devices>,
    pub remote_addr: SocketAddr,
    pub is_ssl: bool,
}

// use only on checked prefixes
fn get_subpath(path: &str, prefix: &str) -> PathBuf {
    Path::new(&path).strip_prefix(prefix).unwrap().to_path_buf()
}

fn add_cors_headers(
    mut resp: Response<Body>,
    origin: Option<Origin>,
    enabled: bool,
) -> Response<Body> {
    if !enabled {
        return resp;
    }
    match origin {
        Some(o) => {
            if let Ok(allowed_origin) = header2header::<_, AccessControlAllowOrigin>(o) {
                let headers = resp.headers_mut();
                headers.typed_insert(allowed_origin);
                headers.typed_insert(AccessControlAllowCredentials);
            }
            resp
        }
        None => resp,
    }
}

fn preflight_cors_response(req: &Request<Body>) -> Response<Body> {
    let origin = req.headers().typed_get::<Origin>();
    const ALLOWED_METHODS: &[Method] = &[Method::GET, Method::POST, Method::OPTIONS];

    let mut resp_builder = Response::builder()
        .status(StatusCode::NO_CONTENT)
        // Allow all requested headers
        .typed_header(AccessControlAllowMethods::from_iter(
            ALLOWED_METHODS.iter().cloned(),
        ))
        .typed_header(AccessControlMaxAge::from(Duration::from_secs(24 * 3600)));

    if let Some(requested_headers) = req.headers().typed_get::<AccessControlRequestHeaders>() {
        resp_builder = resp_builder.typed_header(AccessControlAllowHeaders::from_iter(
            requested_headers.iter(),
        ));
    }

    let resp = resp_builder.body(Body::empty()).unwrap();

    add_cors_headers(resp, origin, true)
}

const STATIC_FILE_NAMES: &[&str] = &[
    "/bundle.js",
    "/bundle.css",
    "/global.css",
    "/favicon.png",
    "/app.webmanifest",
    "/service-worker.js",
];

const STATIC_DIR: &str = "/static/";

fn is_static_file(path: &str) -> bool {
    STATIC_FILE_NAMES.contains(&path) || path.starts_with(STATIC_DIR)
}

#[allow(clippy::type_complexity)]
impl<C: 'static> Service<Request<Body>> for FileSendService<C> {
    type Response = Response<Body>;
    type Error = error::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        Box::pin(self.process_request(req).or_else(|e| {
            error!("Request processing error: {}", e);
            future::ok(resp::internal_error())
        }))
    }
}

impl<C: 'static> FileSendService<C> {
    fn process_request(&mut self, req: Request<Body>) -> ResponseFuture {
        //Limit rate of requests if configured
        if let Some(limiter) = self.rate_limitter.as_ref() {
            if limiter.start_one().is_err() {
                debug!("Rejecting request due to rate limit");
                return resp::fut(resp::too_many_requests);
            }
        }

        // handle OPTIONS method for CORS preflightAtomicUsize
        if req.method() == Method::OPTIONS && get_config().is_cors_enabled(&req) {
            debug!(
                "Got OPTIONS request in CORS mode : {} {:?}",
                req.uri(),
                req.headers()
            );
            return Box::pin(future::ok(preflight_cors_response(&req)));
        }

        let req = match RequestWrapper::new(
            req,
            get_config().url_path_prefix.as_deref(),
            self.remote_addr.ip(),
            self.is_ssl,
        ) {
            Ok(r) => r,
            Err(e) => {
                error!("Request URL error: {}", e);
                return resp::fut(resp::not_found);
            }
        };
        //static files
        if req.method() == Method::GET {
            if req.path() == "/" || req.path() == "/index.html" {
                return send_file_simple(
                    &get_config().client_dir,
                    "index.html",
                    get_config().static_resource_cache_age,
                );
            } else if is_static_file(req.path()) {
                return send_file_simple(
                    &get_config().client_dir,
                    &req.path()[1..],
                    get_config().static_resource_cache_age,
                );
            }
        }
        // from here everything must be authenticated
        let searcher = self.search.clone();
        let cors = get_config().is_cors_enabled(&req.request);
        let origin = req.headers().typed_get::<Origin>();
        let addr = self.remote_addr;

        let resp = match self.authenticator {
            Some(ref auth) => {
                let collections = self.collections.clone();
                let devices = self.devices.clone();
                Box::pin(auth.authenticate(req).and_then(move |result| match result {
                    AuthResult::Authenticated { request, .. } => {
                        FileSendService::<C>::process_checked(
                            request,
                            addr,
                            searcher,
                            collections,
                            devices,
                        )
                    }
                    AuthResult::LoggedIn(resp) | AuthResult::Rejected(resp) => {
                        Box::pin(future::ok(resp))
                    }
                }))
            }
            None => FileSendService::<C>::process_checked(
                req,
                addr,
                searcher,
                self.collections.clone(),
                self.devices.clone(),
            ),
        };
        Box::pin(resp.map_ok(move |r| add_cors_headers(r, origin, cors)))
    }

    fn process_checked(
        #[allow(unused_mut)] mut req: RequestWrapper,
        addr: SocketAddr,
        searcher: Search<String>,
        collections: Arc<Collections>,
        devices: Arc<Devices>,
    ) -> ResponseFuture {
        let params = req.params();
        let path = req.path();
        match *req.method() {
            Method::GET => {
                if path.starts_with("/collections") {
                    collections_list()
                } else if path.starts_with("/ws") {
                    handle_request(collections, devices, req.request, addr)
                } else if cfg!(feature = "shared-positions") && path.starts_with("/positions") {
                    // positions API
                    #[cfg(feature = "shared-positions")]
                    match extract_group(path) {
                        PositionGroup::Group(group) => match position_params(&params) {
                            Ok(p) => subs::all_positions(collections, group, Some(p)),

                            Err(e) => {
                                error!("Invalid timestamp param: {}", e);
                                resp::fut(resp::bad_request)
                            }
                        },
                        PositionGroup::Last(group) => subs::last_position(collections, group),
                        PositionGroup::Path {
                            collection,
                            group,
                            path,
                        } => {
                            let recursive = req.params().exists("rec");
                            let filter = match position_params(&params) {
                                Ok(p) => p,

                                Err(e) => {
                                    error!("Invalid timestamp param: {}", e);
                                    return resp::fut(resp::bad_request);
                                }
                            };
                            subs::folder_position(
                                collections,
                                group,
                                collection,
                                path,
                                recursive,
                                Some(filter),
                            )
                        }
                        PositionGroup::Malformed => resp::fut(resp::bad_request),
                    }
                    #[cfg(not(feature = "shared-positions"))]
                    unimplemented!();
                } else if cfg!(feature = "shared-positions") && path.starts_with("/position") {
                    #[cfg(not(feature = "shared-positions"))]
                    unimplemented!();
                    #[cfg(feature = "shared-positions")]
                    self::position::position_service(req, collections)
                } else {
                    let (path, collection_index) = match extract_collection_number(path) {
                        Ok(r) => r,
                        Err(_) => {
                            error!("Invalid collection number");
                            return resp::fut(resp::not_found);
                        }
                    };

                    let base_dir = &get_config().base_dirs[collection_index];
                    let ord = params
                        .get("ord")
                        .map(|l| FoldersOrdering::from_letter(l))
                        .unwrap_or(FoldersOrdering::Alphabetical);
                    if path.starts_with("/audio/") {
                        let user_agent = req.headers().typed_get::<UserAgent>();
                        FileSendService::<C>::serve_audio(
                            &req,
                            base_dir,
                            path,
                            params,
                            user_agent.as_ref().map(|ua| ua.as_str()),
                        )
                    } else if path.starts_with("/media/") {
                        FileSendService::<C>::serve_media(
                            &req,
                            base_dir,
                            path,
                            collections,
                            collection_index,
                        )
                    } else if path.starts_with("/folder/") {
                        let group = params.get_string("group");
                        get_folder(
                            collection_index,
                            get_subpath(path, "/folder/"),
                            collections,
                            ord,
                            group,
                        )
                    } else if path.starts_with("/all") {
                        get_all(
                            collection_index,
                            collections,
                        )
                    } else if !get_config().disable_folder_download && path.starts_with("/download")
                    {
                        #[cfg(feature = "folder-download")]
                        {
                            let format = params
                                .get("fmt")
                                .and_then(|f| f.parse::<types::DownloadFormat>().ok())
                                .unwrap_or_default();
                            let recursive = params
                                .get("collapsed")
                                .and_then(|_| get_config().collapse_cd_folders.as_ref())
                                .and_then(|c| c.regex.as_ref())
                                .and_then(|re| Regex::new(re).ok());
                            subs::download_folder(
                                base_dir,
                                get_subpath(path, "/download/"),
                                format,
                                recursive,
                            )
                        }
                        #[cfg(not(feature = "folder-download"))]
                        {
                            error!("folder download not ");
                            resp::fut(resp::not_found)
                        }
                    } else if path == "/search" {
                        if let Some(search_string) = params.get_string("q") {
                            let group = params.get_string("group");
                            search(collection_index, searcher, search_string, ord, group)
                        } else {
                            error!("q parameter is missing in search");
                            resp::fut(resp::not_found)
                        }
                    } else if path.starts_with("/recent") {
                        let group = params.get_string("group");
                        recent(collection_index, searcher, group)
                    } else if path.starts_with("/cover/") {
                        send_file_simple(
                            base_dir,
                            get_subpath(path, "/cover"),
                            get_config().folder_file_cache_age,
                        )
                    } else if path.starts_with("/desc/") {
                        send_file_simple(
                            base_dir,
                            get_subpath(path, "/desc"),
                            get_config().folder_file_cache_age,
                        )
                    } else {
                        error!("Invalid path requested {}", path);
                        resp::fut(resp::not_found)
                    }
                }
            }

            Method::POST => {
                #[cfg(feature = "shared-positions")]
                if path.starts_with("/positions") {
                    match extract_group(path) {
                        PositionGroup::Group(group) => {
                            let is_json = req
                                .headers()
                                .get("Content-Type")
                                .and_then(|v| {
                                    v.to_str()
                                        .ok()
                                        .map(|s| s.to_lowercase().eq("application/json"))
                                })
                                .unwrap_or(false);
                            if is_json {
                                Box::pin(async move {
                                    match req.body_bytes().await {
                                        Ok(bytes) => {
                                            subs::insert_position(collections, group, bytes).await
                                        }
                                        Err(e) => {
                                            error!("Error reading POST body: {}", e);
                                            Ok(resp::bad_request())
                                        }
                                    }
                                })
                            } else {
                                error!("Not JSON content type");
                                resp::fut(resp::bad_request)
                            }
                        }
                        _ => resp::fut(resp::bad_request),
                    }
                } else {
                    resp::fut(resp::not_found)
                }

                #[cfg(not(feature = "shared-positions"))]
                resp::fut(resp::method_not_supported)
            }

            _ => resp::fut(resp::method_not_supported),
        }
    }

    fn serve_audio(
        req: &RequestWrapper,
        base_dir: &'static Path,
        path: &str,
        params: QueryParams,
        user_agent: Option<&str>,
    ) -> ResponseFuture {
        debug!(
            "Received request with following headers {:?}",
            req.headers()
        );

        let range = req.headers().typed_get::<Range>();

        let bytes_range = match range.map(|r| r.iter().collect::<Vec<_>>()) {
            Some(bytes_ranges) => {
                if bytes_ranges.is_empty() {
                    error!("Range header without range bytes");
                    return resp::fut(resp::bad_request);
                } else if bytes_ranges.len() > 1 {
                    error!("Range with multiple ranges is not supported");
                    return resp::fut(resp::not_implemented);
                } else {
                    Some(bytes_ranges[0])
                }
            }

            None => None,
        };
        let seek: Option<f32> = params.get("seek").and_then(|s| s.parse().ok());

        send_file(base_dir, get_subpath(path, "/audio/"), bytes_range, seek)
    }

    fn serve_media(
        req: &RequestWrapper,
        base_dir: &'static Path,
        path: &str,
        collections: Arc<Collections>,
        collection_index: usize,
    ) -> ResponseFuture {
        debug!(
            "Received request with following headers {:?}",
            req.headers()
        );
        let range = req.headers().typed_get::<Range>();
        

        let bytes_range = match range.map(|r| r.iter().collect::<Vec<_>>()) {
            Some(bytes_ranges) => {
                if bytes_ranges.is_empty() {
                    error!("Range header without range bytes");
                    return resp::fut(resp::bad_request);
                } else if bytes_ranges.len() > 1 {
                    error!("Range with multiple ranges is not supported");
                    return resp::fut(resp::not_implemented);
                } else {
                    Some(bytes_ranges[0])
                }
            }

            None => None,
        };
        let track_id: Option<u32> = get_subpath(path, "/media/").to_str().and_then(|s| s.parse().ok());
        match collections.get_audio_track(collection_index, track_id.unwrap()) {
            Ok(af) => send_file(base_dir, af.path, bytes_range, None),
            Err(e) => {
                error!("Error while retrieving track {:?} info, {}", track_id, e);
                resp::fut(resp::not_found)
            }
        }
    }
}

async fn handle_connection(
    ws_stream: WebSocketStream<Upgraded>,
    collections: Arc<Collections>,
    devices: Arc<Devices>,
    addr: SocketAddr,
) {
    debug!("WebSocket connection established: {}", addr);

    // Insert the write part of this peer to the peer map.
    let (tx, rx) = unbounded();
    let device_uuid = Uuid::new_v4().to_string();
    devices.map.insert(
        addr,
        Device {
            name: None,
            id: device_uuid.clone(),
            ws: tx,
            active: false,
        },
    );

    let (outgoing, incoming) = ws_stream.split();

    let broadcast_incoming = incoming.try_for_each(|msg| {
        match msg {
            Message::Text(string) => {
                debug!("Received a message from {}: {}", addr, string);
                process_message(string, collections.clone(), devices.clone(), addr);
            }
            Message::Binary(_) => {
                error!("Message from {}. Binary messaging is not supported", addr)
            }
            Message::Ping(_) | Message::Pong(_) => debug!("Received ping/pong from {}", addr),
            Message::Close(None) => {
                debug!("{} closed websocket", addr);
                devices.map.remove(&addr);
                send_updated_device_list(devices.clone());
            }
            Message::Close(Some(frame)) => {
                debug!("{} closed websocket, reason: {}", addr, frame.reason);
                devices.map.remove(&addr);
                send_updated_device_list(devices.clone());
            }
            Message::Frame(frame) => debug!("Received frame: {:?}", frame.into_data()),
        }

        future::ok(())
    });

    let receive_from_others = rx.map(Ok).forward(outgoing);
    debug!("receive_from_others");

    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    debug!("{} disconnected", &addr);
}

pub fn handle_request(
    collections: Arc<Collections>,
    devices: Arc<Devices>,
    mut req: Request<Body>,
    addr: SocketAddr,
) -> ResponseFuture {
    debug!("Received a new, potentially ws handshake");
    debug!("The request's path is: {}", req.uri().path());
    debug!("The request's headers are:");
    let upgrade = HeaderValue::from_static("Upgrade");
    let websocket = HeaderValue::from_static("websocket");
    let headers = req.headers();
    let key = headers.get(SEC_WEBSOCKET_KEY);
    let derived = key.map(|k| derive_accept_key(k.as_bytes()));
    // if req.method() != Method::GET
    //     || req.version() < Version::HTTP_11
    //     || !headers
    //         .get(CONNECTION)
    //         .and_then(|h| h.to_str().ok())
    //         .map(|h| {
    //             h.split(|c| c == ' ' || c == ',')
    //                 .any(|p| p.eq_ignore_ascii_case(upgrade.to_str().unwrap()))
    //         })
    //         .unwrap_or(false)
    //     || !headers
    //         .get(UPGRADE)
    //         .and_then(|h| h.to_str().ok())
    //         .map(|h| h.eq_ignore_ascii_case("websocket"))
    //         .unwrap_or(false)
    //     || !headers.get(SEC_WEBSOCKET_VERSION).map(|h| h == "13").unwrap_or(false)
    //     || key.is_none()
    //     || req.uri() != "/socket"
    // {
    //     Box::pin(future::ok(json_response(&collections)))
    //     return Ok(Response::new(Body::from("Hello World!")));
    // }
    let ver = req.version();
    tokio::task::spawn(async move {
        match hyper::upgrade::on(&mut req).await {
            Ok(upgraded) => {
                trace!("upgraded connection");
                handle_connection(
                    WebSocketStream::from_raw_socket(upgraded, Role::Server, None).await,
                    collections,
                    devices,
                    addr,
                )
                .await;
            }
            Err(e) => error!("upgrade error: {}", e),
        }
    });
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *res.version_mut() = ver;
    res.headers_mut().append(CONNECTION, upgrade);
    res.headers_mut().append(UPGRADE, websocket);
    res.headers_mut()
        .append(SEC_WEBSOCKET_ACCEPT, derived.unwrap().parse().unwrap());
    Box::pin(future::ok(res))
}

fn process_message(msg: String, collections: Arc<Collections>, devices: Arc<Devices>, addr: SocketAddr) {
    debug!("Got message {:?} from {}", msg, addr);
    let message: Result<MsgIn, Error> = msg.parse();

    let devices = devices.clone();

    match message {
        Ok(message) => match message {
            MsgIn::RegisterDevice { name } => {
                let device_count = devices.map.len();
                let mut device = devices.map.get_mut(&addr).unwrap();
                device.name = Some(name);
                if device_count == 1 {
                    device.active = true;
                }
                debug!("RegisterDevice: {:?}", device.id);
                let reg_device_event = MsgOut::RegisterDeviceEvent {
                    device_id: device.id.clone()
                };
                send_to_device(reg_device_event, devices.clone(), addr);
                send_updated_device_list(devices.clone());
            }
            MsgIn::MakeDeviceActive { device_id } => {
                let device_key = find_device_key_by_id(devices.clone(), &device_id);
                let mut device = devices.map.get_mut(&device_key).unwrap();
                device.active = true;
                let make_device_active = MsgOut::MakeDeviceActiveEvent {
                    device_id: device_id,
                };
                send_to_all_devices(make_device_active, devices.clone())
            }
            MsgIn::PlayTrack { collection, track_id } => {
                debug!("PlayTrack: {} {}", collection, track_id);
                let play_track = MsgOut::PlayTrackEvent { collection, track_id };
                send_to_all_devices(play_track, devices.clone())
            }
            MsgIn::NextTrack { collection} => {
                let shuffle_mode = devices.shuffle_mode.lock().unwrap().to_owned();
                let collections = collections.clone();
                match shuffle_mode {
                    ShuffleMode::Off => {
                        debug!("Shuffle is off");
                    },
                    ShuffleMode::CollectionWide | ShuffleMode::CurrentDir => {
                        debug!("Current dir");
                        let track_count = collections.count_tracks(collection).unwrap();
                        let rnd: usize = rand::thread_rng().gen_range(0..(track_count as usize));

                        let play_track = MsgOut::PlayTrackEvent { collection: collection, track_id: rnd as u32};
                        send_to_all_devices(play_track, devices.clone())
                    },
                    ShuffleMode::Global => {
                        debug!("Global");
                    },
                }
            },
            MsgIn::Pause {} => {
                let pause = MsgOut::PauseEvent { };
                send_to_all_devices_excluding(pause, devices.clone(), Some(addr))
            },
            MsgIn::Resume {} => {
                let resume = MsgOut::ResumeEvent { };
                send_to_all_devices_excluding(resume, devices.clone(), Some(addr))
            },
            MsgIn::SwitchShuffle { mode } => {
                let mut mutex = devices.shuffle_mode.lock().unwrap();
                *mutex = mode.clone();
                let switch_shuffle = MsgOut::SwitchShuffleEvent { mode: mode };
                send_to_all_devices_excluding(switch_shuffle, devices.clone(), Some(addr))
            },
            MsgIn::CurrentPos { collection, track_id, time } => {
                let current_pos = MsgOut::CurrentPosEvent { collection, track_id, time };
                send_to_all_devices_excluding(current_pos, devices.clone(), Some(addr))
            },
            MsgIn::RewindTo { time } => {
                let rewind_to = MsgOut::RewindToEvent { time: time };
                send_to_all_devices_excluding(rewind_to, devices.clone(), Some(addr))
            },
            MsgIn::VolumeChange { value } => {
                let volume_change = MsgOut::VolumeChangeEvent { value: value };
                send_to_all_devices_excluding(volume_change, devices.clone(), Some(addr))
            },
        },
        Err(e) => {
            error!("Client message error: {}", e);
        }
    }
}

fn find_device_key_by_id(devices: Arc<Devices>, device_id: &str) -> SocketAddr {
    let ref_key_value = devices.map.iter()
        .find(|ref_multi| ref_multi.value().id == device_id)
        .unwrap();
    return *ref_key_value.key();
}

fn send_updated_device_list(devices: Arc<Devices>) {
    tokio::spawn(async move {
        debug!("creating device list...");
        let device_list: Vec<ClientDevice> = devices.map
            .iter()
            .map(|ref_multi| { 
                let d = ref_multi.value();
                ClientDevice {
                    name: d.name.as_ref().unwrap().clone(),
                    id: d.id.clone(),
                    active: d.active,
                }
            })
            .collect();
        debug!("device list: {:?}", device_list);
        let reg_device = MsgOut::DevicesOnlineEvent {
            devices: device_list,
        };
        debug!("out msg: {:?}", reg_device);
        send_to_all_devices(reg_device, devices.clone())
    });
}

fn send_to_device(msg: MsgOut, devices: Arc<Devices>, addr: SocketAddr) {
    tokio::spawn(async move {
        let m = Msg(msg);
        debug!("Sending msg to {}: {:?}", addr, m);
        let device = devices.map.get(&addr).unwrap();
        match device.ws.unbounded_send(Message::Text(serde_json::to_string(&m).unwrap())) {
            Ok(_) => {
                debug!("Sent to device");
            },
            Err(err) => {
                error!("Error while sending: {}", err);
            }
        }
    });
}

fn send_to_all_devices(msg: MsgOut, devices: Arc<Devices>) {
    send_to_all_devices_excluding(msg, devices.clone(), None);
}

fn send_to_all_devices_excluding(msg: MsgOut, devices: Arc<Devices>, device_to_exclude: Option<SocketAddr>) {
    tokio::spawn(async move {
        let m = Msg(msg);
        debug!("Sending msg to all but {:?}: {:?}", device_to_exclude, m);
        let inactive_devices: Vec<SocketAddr> = devices.map.iter()
            .filter(|ref_multi| {
                device_to_exclude.is_none() || (device_to_exclude.is_some() && *ref_multi.key() != device_to_exclude.unwrap())
            })
            .map(|ref_multi| {
                let socket_addr = ref_multi.key();
                let device = ref_multi.value();
                // debug!("Sending {:?}", &m);
                match device.ws.unbounded_send(Message::Text(serde_json::to_string(&m).unwrap())) {
                    Ok(_) => {
                        // debug!("Sent");
                        None
                    },
                    Err(err) => {
                        error!("Error while sending: {}", err);
                        Some(socket_addr.to_owned())
                    }
                }
            })
            .filter(|opt_socket| opt_socket.is_some())
            .map(|opt_socket| opt_socket.unwrap()) 
            .collect();

        // debug!("Sent msg to everyone: {:?}", m);
        remove_devices(devices.clone(), inactive_devices);
    });
}

fn remove_devices(devices: Arc<Devices>, to_remove: Vec<SocketAddr>) {
    if !to_remove.is_empty() {
        debug!("Removing {} inactive devices", to_remove.len());
        to_remove.into_iter().for_each(|socket_addr| {
            devices.map.remove(&socket_addr);
            ()
        });
        debug!("Removed all inactive devices");
    }
}

lazy_static! {
    static ref COLLECTION_NUMBER_RE: Regex = Regex::new(r"^/(\d+)/.+").unwrap();
}

fn extract_collection_number(path: &str) -> Result<(&str, usize), ()> {
    let matches = COLLECTION_NUMBER_RE.captures(path);
    if let Some(matches) = matches {
        let cnum = matches.get(1).unwrap();
        // match gives us char position it's safe to slice
        let new_path = &path[cnum.end()..];
        // and cnum is guarateed to contain digits only
        let cnum: usize = cnum.as_str().parse().unwrap();
        if cnum >= get_config().base_dirs.len() {
            return Err(());
        }
        Ok((new_path, cnum))
    } else {
        Ok((path, 0))
    }
}

#[cfg(feature = "shared-positions")]
#[derive(Debug)]
enum PositionGroup {
    Group(String),
    Last(String),
    Path {
        group: String,
        collection: usize,
        path: String,
    },
    Malformed,
}

#[cfg(feature = "shared-positions")]
fn position_params(params: &QueryParams) -> error::Result<collection::PositionFilter> {
    use collection::{audio_meta::TimeStamp, PositionFilter};

    fn get_ts_param(params: &QueryParams, name: &str) -> Result<Option<TimeStamp>, anyhow::Error> {
        Ok(if let Some(ts) = params.get(name) {
            Some(ts.parse::<u64>().map_err(error::Error::new)?).map(TimeStamp::from)
        } else {
            None
        })
    }

    let finished = params.exists("finished");
    let unfinished = params.exists("unfinished");
    let finished = match (finished, unfinished) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    };

    let from = get_ts_param(params, "from")?;
    let to = get_ts_param(params, "to")?;

    Ok(PositionFilter::new(finished, from, to))
}

#[cfg(feature = "shared-positions")]
fn extract_group(path: &str) -> PositionGroup {
    let mut segments = path.splitn(5, '/');
    segments.next(); // read out first empty segment
    segments.next(); // readout positions segment
    if let Some(group) = segments.next().map(|g| g.to_owned()) {
        if let Some(last) = segments.next() {
            if last == "last" {
                //only last position
                return PositionGroup::Last(group);
            } else if let Ok(collection) = last.parse::<usize>() {
                if let Some(path) = segments.next() {
                    return PositionGroup::Path {
                        group,
                        collection,
                        path: path.into(),
                    };
                }
            }
        } else {
            return PositionGroup::Group(group);
        }
    }
    PositionGroup::Malformed
}

#[cfg(test)]
#[cfg(feature = "shared-positions")]
mod tests {
    use super::*;

    #[test]
    fn test_extract_group() {
        if let PositionGroup::Group(x) = extract_group("/positions/usak") {
            assert_eq!(x, "usak");
        } else {
            panic!("group does not match")
        }

        if let PositionGroup::Last(x) = extract_group("/positions/usak/last") {
            assert_eq!(x, "usak");
        } else {
            panic!("group does not match")
        }

        if let PositionGroup::Path {
            path,
            collection,
            group,
        } = extract_group("/positions/usak/0/hrabe/drakula")
        {
            assert_eq!(group, "usak");
            assert_eq!(collection, 0);
            assert_eq!(path, "hrabe/drakula");
        } else {
            panic!("group does not match")
        }

        if let PositionGroup::Malformed = extract_group("/positions/chcip/pes") {
        } else {
            panic!("should be invalid")
        }
    }

    #[test]
    fn test_json() {
        let s = serde_json::to_string(&MsgIn::RegisterDevice {
            name: "Android".to_string(),
        });
        println!("json: {}", s.unwrap());

        let json = r#"
        {"RegisterDevice":{"name":"Android"}}
        "#;
        let message: Result<MsgIn, Error> = json.parse();
        println!("msg: {:?}", message.unwrap());
    }
}
