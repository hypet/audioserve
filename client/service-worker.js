var EventType;
(function (EventType) {
    EventType[EventType["FileCached"] = 0] = "FileCached";
    EventType[EventType["FileDeleted"] = 1] = "FileDeleted";
})(EventType || (EventType = {}));

function removeQuery(url) {
    const parsedUrl = new URL(url);
    parsedUrl.search = "";
    return parsedUrl.toString();
}
function splitPath(path) {
    const idx = path.lastIndexOf("/");
    if (idx >= 0) {
        return {
            folder: path.substring(0, idx),
            file: path.substring(idx + 1),
        };
    }
    else {
        return {
            file: path,
        };
    }
}

const AUDIO_CACHE_NAME = "audio";
const AUDIO_CACHE_LIMIT = 1000;
var CacheMessageKind;
(function (CacheMessageKind) {
    CacheMessageKind[CacheMessageKind["Prefetch"] = 1] = "Prefetch";
    CacheMessageKind[CacheMessageKind["AbortLoads"] = 2] = "AbortLoads";
    CacheMessageKind[CacheMessageKind["PrefetchCached"] = 10] = "PrefetchCached";
    CacheMessageKind[CacheMessageKind["ActualCached"] = 11] = "ActualCached";
    CacheMessageKind[CacheMessageKind["Skipped"] = 12] = "Skipped";
    CacheMessageKind[CacheMessageKind["Deleted"] = 20] = "Deleted";
    CacheMessageKind[CacheMessageKind["PrefetchError"] = 30] = "PrefetchError";
    CacheMessageKind[CacheMessageKind["ActualError"] = 31] = "ActualError";
    CacheMessageKind[CacheMessageKind["OtherError"] = 39] = "OtherError";
    CacheMessageKind[CacheMessageKind["Ping"] = 40] = "Ping";
    CacheMessageKind[CacheMessageKind["Pong"] = 41] = "Pong";
})(CacheMessageKind || (CacheMessageKind = {}));

const API_CACHE_NAME = "api";
const APP_CACHE_PREFIX = "static-";

/******************************************************************************
Copyright (c) Microsoft Corporation.

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
***************************************************************************** */

function __awaiter(thisArg, _arguments, P, generator) {
    function adopt(value) { return value instanceof P ? value : new P(function (resolve) { resolve(value); }); }
    return new (P || (P = Promise))(function (resolve, reject) {
        function fulfilled(value) { try { step(generator.next(value)); } catch (e) { reject(e); } }
        function rejected(value) { try { step(generator["throw"](value)); } catch (e) { reject(e); } }
        function step(result) { result.done ? resolve(result.value) : adopt(result.value).then(fulfilled, rejected); }
        step((generator = generator.apply(thisArg, _arguments || [])).next());
    });
}

/// <reference no-default-lib="true"/>
function parseRange(range) {
    const r = /^bytes=(\d+)-?(\d+)?/.exec(range);
    return [Number(r[1]), r[2] ? Number(r[2]) : undefined];
}
function buildResponse(originalResponse, range) {
    return __awaiter(this, void 0, void 0, function* () {
        if (range) {
            const body = yield originalResponse.blob();
            const size = body.size;
            const [start, end] = parseRange(range);
            return new Response(body.slice(start, end ? end + 1 : undefined), {
                status: 206,
                headers: {
                    "Content-Range": `bytes ${start}-${end ? end : size - 1}/${size}`,
                    "Content-Type": originalResponse.headers.get("Content-Type"),
                },
            });
        }
        else {
            return originalResponse;
        }
    });
}
function evictCache(cache, sizeLimit, onDelete) {
    return __awaiter(this, void 0, void 0, function* () {
        const keys = yield cache.keys();
        const toDelete = keys.length - sizeLimit;
        if (toDelete > 0) {
            const deleteList = keys.slice(0, toDelete);
            for (const key of deleteList.reverse()) {
                if (yield cache.delete(key)) {
                    if (onDelete)
                        yield onDelete(key);
                }
            }
        }
    });
}
function cloneRequest(req) {
    return new Request(req.url, {
        credentials: "include",
    });
}
function logFetchError(e, url) {
    if (e instanceof DOMException && e.name == "AbortError") {
        console.debug(`Caching of ${url} was aborted`);
    }
    else {
        console.error(`Error caching of ${url}: ${e}`, e);
    }
}
class FetchQueueItem {
    constructor(url, abort, isDirect, folderPosition) {
        this.url = url;
        this.abort = abort;
        this.isDirect = isDirect;
        this.folderPosition = folderPosition;
    }
}
class AudioCache {
    constructor(audioCache, sizeLimit, broadcastMessage) {
        this.audioCache = audioCache;
        this.sizeLimit = sizeLimit;
        this.broadcastMessage = broadcastMessage;
        this.queue = [];
    }
    getQueue() {
        return this.queue.map((item) => item.url);
    }
    has(url) {
        return this.queue.findIndex((i) => i.url === url) >= 0;
    }
    add(url, abort, isDirect, folderPosition) {
        // abort all previous direct request
        if (isDirect)
            this.queue.forEach((i) => {
                if (i.isDirect)
                    i.abort.abort();
            });
        this.queue.push(new FetchQueueItem(url, abort, isDirect, folderPosition));
    }
    delete(url) {
        const idx = this.queue.findIndex((i) => i.url === url);
        if (idx >= 0) {
            this.queue.splice(idx, 1);
        }
    }
    abort(pathPrefix, keepDirect) {
        for (const i of this.queue) {
            if (!(keepDirect && i.isDirect) &&
                (!pathPrefix || new URL(i.url).pathname.startsWith(pathPrefix))) {
                i.abort.abort();
                // Firefox seems not to generate error when aborting request, so delete it if it was not deleted due to error
                setTimeout(() => {
                    this.delete(i.url);
                }, 1000);
            }
        }
    }
    handleRequest(evt) {
        const rangeHeader = evt.request.headers.get("range");
        evt.respondWith(caches
            .open(this.audioCache)
            .then((cache) => cache.match(evt.request).then((resp) => {
            if (resp) {
                console.debug(`Serving cached audio file: ${resp.url}`);
                return buildResponse(resp, rangeHeader);
            }
            else {
                const keyReq = removeQuery(evt.request.url);
                if (this.has(keyReq)) {
                    console.debug(`Not caching direct request ${keyReq} as it is already in progress elsewhere`);
                    return fetch(evt.request);
                }
                else {
                    const req = cloneRequest(evt.request);
                    const abort = new AbortController();
                    this.add(keyReq, abort, true);
                    req.headers.delete("Range"); // let remove range header so we can cache whole file
                    return fetch(req, { signal: abort.signal }).then((resp) => {
                        // if not cached we can put it
                        if (resp.status === 200) {
                            const keyReq = removeQuery(evt.request.url);
                            evt.waitUntil(cache
                                .put(keyReq, resp.clone())
                                .then(() => __awaiter(this, void 0, void 0, function* () {
                                yield this.broadcastMessage({
                                    kind: CacheMessageKind.ActualCached,
                                    data: {
                                        originalUrl: resp.url,
                                        cachedUrl: keyReq,
                                    },
                                });
                                yield evictCache(cache, this.sizeLimit, (req) => {
                                    return this.broadcastMessage({
                                        kind: CacheMessageKind.Deleted,
                                        data: {
                                            cachedUrl: req.url,
                                            originalUrl: req.url,
                                        },
                                    });
                                });
                            }))
                                .catch((e) => logFetchError(e, keyReq))
                                .then(() => this.delete(keyReq)));
                        }
                        else {
                            console.error("Audio fetch return non-success status " + resp.status);
                            this.delete(keyReq);
                        }
                        return resp;
                    });
                }
            }
        }))
            .catch((err) => {
            console.error("Service worker Error", err);
            return new Response("Service Worker Cache Error", { status: 555 });
        }));
    }
    handlePrefetch(evt) {
        const msg = evt.data;
        console.debug("Service worker: Prefetch request", msg.data.url);
        const keyUrl = removeQuery(msg.data.url);
        let abort;
        if (this.has(keyUrl)) {
            evt.waitUntil(this.broadcastMessage({
                kind: CacheMessageKind.Skipped,
                data: {
                    cachedUrl: keyUrl,
                    originalUrl: msg.data.url,
                },
            }));
            return;
        }
        abort = new AbortController();
        this.add(keyUrl, abort, false, msg.data.folderPosition);
        evt.waitUntil(fetch(msg.data.url, {
            credentials: "include",
            cache: "no-cache",
            signal: abort.signal,
        })
            .then((resp) => __awaiter(this, void 0, void 0, function* () {
            if (resp.ok) {
                const cache = yield self.caches.open(this.audioCache);
                yield cache.put(keyUrl, resp);
                yield this.broadcastMessage({
                    kind: CacheMessageKind.PrefetchCached,
                    data: {
                        cachedUrl: keyUrl,
                        originalUrl: resp.url,
                    },
                });
                yield evictCache(cache, this.sizeLimit, (req) => {
                    return this.broadcastMessage({
                        kind: CacheMessageKind.Deleted,
                        data: {
                            cachedUrl: req.url,
                            originalUrl: req.url,
                        },
                    });
                });
                console.debug(`Service worker: Prefetch response ${resp.status} saving as ${keyUrl}`);
            }
            else {
                console.error(`Cannot cache audio ${resp.url}: status ${resp.status}`);
                yield this.broadcastMessage({
                    kind: CacheMessageKind.PrefetchError,
                    data: {
                        cachedUrl: keyUrl,
                        originalUrl: resp.url,
                        error: new Error(`Response status error code: ${resp.status}`),
                    },
                });
            }
        }))
            .catch((err) => this.broadcastMessage({
            kind: CacheMessageKind.PrefetchError,
            data: {
                cachedUrl: keyUrl,
                originalUrl: msg.data.url,
                error: err,
            },
        }))
            .then(() => this.delete(keyUrl)));
    }
}
class NetworkFirstCache {
    constructor(cacheName, sizeLimit = 1000) {
        this.cacheName = cacheName;
        this.sizeLimit = sizeLimit;
        this.isEnabled = true;
    }
    handleRequest(evt) {
        return __awaiter(this, void 0, void 0, function* () {
            if (!this.isEnabled)
                return;
            evt.respondWith(fetch(evt.request)
                .then((response) => {
                if (response.status !== 200) {
                    console.error(`Server returned status ${response.status} for ${evt.request.url}`);
                    throw response;
                }
                return caches.open(this.cacheName).then((cache) => {
                    cache.put(evt.request, response.clone()).then(() => {
                        return evictCache(cache, this.sizeLimit, (req) => {
                            console.debug(`Deleted ${req.url} from cache ${this.cacheName}`);
                            return Promise.resolve();
                        });
                    });
                    return response;
                });
            })
                .catch((e) => {
                // For 401, 404 errors we must not use cache!!!
                if (e instanceof Response && [401, 404].indexOf(e.status) >= 0) {
                    // delete it from cache
                    caches.open(this.cacheName).then((c) => c.delete(evt.request));
                    return e;
                }
                const errorResponse = () => {
                    if (e instanceof Response) {
                        return e;
                    }
                    else {
                        return new Response("NetworkFirst Cache Error: " + e, {
                            status: 555,
                        });
                    }
                };
                return caches
                    .open(this.cacheName)
                    .then((cache) => {
                    return cache.match(evt.request);
                })
                    .then((resp) => {
                    if (resp) {
                        console.debug("Returning cached response");
                        return resp;
                    }
                    else {
                        return errorResponse();
                    }
                })
                    .catch(() => {
                    return errorResponse();
                });
            }));
        });
    }
    enable() {
        this.isEnabled = true;
    }
    disable() {
        this.isEnabled = false;
    }
}

const ENVIRONMENT = "PRODUCTION";
const APP_COMMIT = "391a0f50";
/// @ts-ignore
const isDevelopment = ENVIRONMENT === "DEVELOPMENT";

/// <reference no-default-lib="true"/>
function broadcastMessage(msg) {
    return self.clients
        .matchAll({ includeUncontrolled: true })
        .then((clients) => {
        for (const c of clients) {
            console.debug(`Sending ${msg} to client ${c.type}::${c.id}`);
            c.postMessage(msg);
        }
    });
}
let globalPathPrefix = (() => {
    const base = location.pathname;
    const folder = splitPath(base).folder;
    if (folder) {
        return folder + "/";
    }
    else {
        return "/";
    }
})();
const staticResources = [
    globalPathPrefix,
    "index.html",
    "global.css",
    "favicon.png",
    "bundle.css",
    "bundle.js",
    "app.webmanifest",
    "static/will_sleep_soon.mp3",
    "static/extended.mp3",
];
const cacheName = APP_CACHE_PREFIX + APP_COMMIT;
const audioCache = AUDIO_CACHE_NAME;
const apiCache = API_CACHE_NAME;
self.addEventListener("install", (evt) => {
    evt.waitUntil(caches
        .open(cacheName)
        .then((cache) => {
        return cache.addAll(staticResources);
    })
        .then(() => {
        console.debug(`Service worker Installation successful (dev ${isDevelopment} ) on path ${location.pathname}`);
        return self.skipWaiting(); // forces to immediately replace old SW
    }));
});
self.addEventListener("activate", (evt) => {
    evt.waitUntil(caches
        .keys()
        .then((keyList) => {
        return Promise.all(keyList.map((key) => {
            if (key.startsWith("static-") && key != cacheName) {
                return caches.delete(key);
            }
            // else if (key == apiCache) {
            //   return caches.delete(key);
            // }
        }));
    })
        .then(() => {
        console.debug("Service worker Activation successful");
        return self.clients.claim(); // and forces immediately to take over current page
    }));
});
const audioCacheHandler = new AudioCache(audioCache, AUDIO_CACHE_LIMIT, broadcastMessage);
const apiCacheHandler = new NetworkFirstCache(apiCache);
self.addEventListener("message", (evt) => {
    var _a;
    const msg = evt.data;
    if (msg.kind === CacheMessageKind.Prefetch) {
        audioCacheHandler.handlePrefetch(evt);
    }
    else if (msg.kind === CacheMessageKind.AbortLoads) {
        audioCacheHandler.abort(msg.data.pathPrefix, msg.data.keepDirect);
    }
    else if (msg.kind === CacheMessageKind.Ping) {
        console.debug("Got PING from client");
        (_a = evt.source) === null || _a === void 0 ? void 0 : _a.postMessage({
            kind: CacheMessageKind.Pong,
            data: {
                pendingAudio: audioCacheHandler.getQueue(),
            },
        });
    }
});
const AUDIO_REG_EXP = new RegExp(`^${globalPathPrefix}\\d+/audio/`);
const API_REG_EXP = new RegExp(`^${globalPathPrefix}(\\d+/)?(folder|collections|transcodings)/?`);
self.addEventListener("fetch", (evt) => {
    const parsedUrl = new URL(evt.request.url);
    if (AUDIO_REG_EXP.test(parsedUrl.pathname)) {
        console.debug("AUDIO FILE request: ", decodeURI(parsedUrl.pathname));
        // we are not intercepting requests with seek query
        if (parsedUrl.searchParams.get("seek"))
            return;
        audioCacheHandler.handleRequest(evt);
    }
    else if (API_REG_EXP.test(parsedUrl.pathname)) {
        console.debug("API request " + parsedUrl.pathname);
        apiCacheHandler.handleRequest(evt);
    }
    else {
        // console.debug(`Checking ${parsedUrl.pathname} against ${API_REG_EXP} result ${API_REG_EXP.test(parsedUrl.pathname)}`)
        evt.respondWith(caches.open(cacheName).then((cache) => cache.match(evt.request).then((response) => {
            console.debug(`OTHER request: ${evt.request.url}`, evt.request, response);
            return response || fetch(evt.request);
        })));
    }
});
