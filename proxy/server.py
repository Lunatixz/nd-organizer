# nd-organizer Subsonic filtering proxy.
#
# A faithful pass-through mirror of Navidrome's Subsonic/OpenSubsonic API.
# Point any Subsonic-compatible client at THIS service instead of Navidrome:
#   - every request is forwarded UNCHANGED to Navidrome (same method, path,
#     query, body and safe headers) - nothing is added, rewritten or dropped,
#     so the proxy is transparent to Navidrome;
#   - the response is only touched when it is JSON with a song list; then it is
#     filtered/reordered and returned. Everything else (XML, audio streams,
#     cover art, errors) is passed back byte-for-byte, so non-JSON clients see
#     an exact mirror of Navidrome.
#
# Filtering applied to JSON song lists:
#   - hard-excluded track IDs (published by the organizer via POST /filters) are
#     removed everywhere: these are net-negative skips past your cap (skipped more
#     than ever played in full);
#   - filler-keyword tracks (pushed by the organizer from Navidrome's
#     fillerKeywords setting) are ignored from the QUEUE: dropped from auto-queue
#     lists (random/search/playlist/genre/top/similar) while an album's track
#     list stays whole;
#   - song lists in queue containers are re-sorted by published weight
#     (plays - 2*skips) so skipped tracks sink and liked tracks rise.
# Album track order (getAlbum) and live/active views (getNowPlaying, getPlayQueue)
# are never reordered.
#
# Credentials are passed through unchanged (clients use their normal
# Navidrome user/password).
#
# Run standalone:
#   python server.py [port]
# Env vars:
#   NAVIDROME_URL   http://navidrome:4533   (must be reachable from this container)
#   FILTER_KEYWORDS intro,outro,instrumental (startup default only; the organizer
#                     pushes Navidrome's fillerKeywords setting via /filters)

import json
import logging
import os
import sys
import time
import urllib.parse
import urllib.request

import collections

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s [filter] %(message)s", datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("filter")

# Ring buffer of recent log lines so the webhook dashboard can read this
# sidecar's logs (GET /logs) without Docker socket access.
LOG_BUFFER = collections.deque(maxlen=500)


class MemHandler(logging.Handler):
    def emit(self, record):
        try:
            LOG_BUFFER.append(self.format(record))
        except Exception:
            pass


_mem = MemHandler()
try:
    _mem.setFormatter(logging.getLogger().handlers[0].formatter)
except Exception:
    pass
logging.getLogger().addHandler(_mem)

NAVIDROME_URL = os.environ.get("NAVIDROME_URL", "http://navidrome:4533").rstrip("/")
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4534
KEYWORDS = [k.strip().lower() for k in os.environ.get("FILTER_KEYWORDS", "intro,outro,interlude,transition,prelude,postlude,christmas,commercial,skit,instrumental,interview,classical,karaoke").split(",") if k.strip()]
EXCLUDED = set()      # skip-heavy track IDs published by the organizer plugin
WEIGHTS = {}          # track ID -> weight (plays - 2*skips); used to reorder returned song lists
KEYWORD_FILTER_ENABLED = True   # pushed by the plugin; startup default
SKIP_MODE = "none"              # none|exclude|third|lessThanHalf|half - pushed by the plugin

STARTED = time.time()
REQUESTS = 0
LAST_REQUEST_TS = 0.0
LAST_PUBLISH_TS = 0.0
STREAMS = collections.deque(maxlen=20)  # recent stream requests: {ts, id}
FILTERED = collections.deque(maxlen=50)  # recent dropped tracks: {ts, id, song, artist}
# Response containers whose song/entry lists are QUEUE sources: lists are
# re-sorted by weight (skipped tracks sink) so auto-queues push better songs.
# Covers every documented Subsonic/OpenSubsonic song-list endpoint except
# getAlbum (track order) and getNowPlaying / getPlayQueue (live/active queues).
REORDER_CONTAINERS = {
    "randomSongs", "searchResult3", "starred2", "playlist",
    "similarSongs", "similarSongs2", "songsByGenre", "topSongs",
}

# Filler-keyword tracks are dropped from EVERY media response the proxy returns
# (albums, playlists, queues, genre/similar/top, starred, ...) - see filter_json.
# Only explicit user searches (searchResult*) keep their keyword tracks because
# the user asked for those. Reordering by weight is limited to REORDER_CONTAINERS
# (auto-queue sources) so album track order is preserved.

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def is_filler_title(title):
    t = (title or "").strip().lower()
    if not t:
        return False
    for k in KEYWORDS:
        if not k:
            continue
        if t == k or t.startswith(k + " ") or t.endswith(" " + k) or (" " + k + " ") in t:
            return True
    return False


def is_song(item):
    """Subsonic songs have NO isDir key; folders/albums set isDir: true."""
    return isinstance(item, dict) and not item.get("isDir", False) and ("path" in item or "title" in item)


def is_skip_heavy(item):
    """A skip-heavy (low-star) track, per the organizer's published ID set."""
    return isinstance(item, dict) and item.get("id") in EXCLUDED


def weight_of(item):
    """Weight of a song dict; unweighted tracks sort as 0 (below played, above skipped)."""
    try:
        return float(WEIGHTS.get(str(item.get("id")), 0.0))
    except (TypeError, ValueError):
        return 0.0


def _record_drop(item, reason):
    try:
        FILTERED.appendleft({
            "ts": int(time.time()),
            "reason": reason,
            "id": str(item.get("id", "")),
            "song": item.get("title", "") or item.get("path", ""),
            "artist": item.get("artist", ""),
        })
    except Exception:
        pass


def _limit_skip_heavy(lst):
    """Cap how many skip-heavy tracks may remain in a queued list (per SKIP_MODE:
    exclude=0, third=1/3, lessThanHalf=0.4, half=1/2, none=no limit). Keeps the
    highest-weight skip-heavy tracks and drops the rest."""
    if SKIP_MODE in ("none", "exclude"):
        return lst
    sh = [it for it in lst if is_song(it) and is_skip_heavy(it)]
    if not sh:
        return lst
    frac = {"third": 1.0 / 3.0, "lessThanHalf": 0.4, "half": 0.5}.get(SKIP_MODE, 0.0)
    allowed = int(len(lst) * frac)
    if len(sh) <= allowed:
        return lst
    drop = sorted(sh, key=weight_of)[: len(sh) - allowed]
    drop_ids = {id(x) for x in drop}
    for it in drop:
        _record_drop(it, "excluded")
    return [it for it in lst if id(it) not in drop_ids]


def filter_json(obj, own_key=None):
    """Drop filler-keyword + skip-heavy song entries from ANY media response and
    reorder song lists inside auto-queue containers.

    Keyword filtering now applies to every `song`/`entry` list the proxy returns
    (albums, playlists, random, searches, genre, similar, starred, ...) - not just
    auto-queues - so filler tracks are removed everywhere a client pulls media.
    Only explicit user searches keep their keyword tracks (the user asked for
    them). Reordering by weight stays limited to auto-queue containers so album
    track order is preserved.
    """
    if isinstance(obj, dict):
        new = {k: filter_json(v, k) for k, v in obj.items()}
        for ck in ("song", "entry"):
            lst = new.get(ck)
            if not isinstance(lst, list):
                continue
            reorder = own_key in REORDER_CONTAINERS
            # Explicit user searches (searchResult*) keep keyword tracks; every
            # other media response drops them.
            is_search = own_key in ("searchResult", "searchResult2", "searchResult3")
            drop_keyword = not is_search and KEYWORD_FILTER_ENABLED
            kept = []
            for it in lst:
                if not is_song(it):
                    kept.append(it)
                    continue
                if is_skip_heavy(it):
                    if reorder and SKIP_MODE == "exclude":
                        _record_drop(it, "excluded")
                        continue
                elif drop_keyword and is_filler_title(it.get("title", "")):
                    _record_drop(it, "keyword")
                    continue
                kept.append(it)
            if reorder:
                kept = _limit_skip_heavy(kept)
                kept = sorted(kept, key=weight_of, reverse=True)
            new[ck] = kept
        return new
    if isinstance(obj, list):
        return [filter_json(it, None) for it in obj]
    return obj


def forward(method, path, query, headers, body):
    """Faithfully mirror the request to Navidrome: exact path, query, method,
    body and (safe) headers. Nothing is added or rewritten - if the client asked
    for XML or no format, Navidrome's answer comes back verbatim."""
    url = NAVIDROME_URL + path + ("?" + query if query else "")
    req = urllib.request.Request(url, data=body or None, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read()
        ctype = resp.headers.get("Content-Type", "")
        return raw, ctype, resp.status


def _safe_headers(h):
    """Forward only hop-by-hop-safe headers (never Host/Connection/Length)."""
    out = {}
    for name in ("Content-Type", "Accept", "User-Agent", "X-Forwarded-For"):
        val = h.get(name)
        if val:
            out[name] = val
    return out


class Handler(BaseHTTPRequestHandler):
    def _wfile_write(self, data):
        """Write a response body, swallowing broken-pipe/reset errors - a client
        that disconnects mid-response is normal and shouldn't dump a traceback."""
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def log_message(self, fmt, *args):
        log.info("http %s", fmt % args)

    def _handle(self, method):
        global REQUESTS, LAST_REQUEST_TS
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        query = parsed.query
        # Sidecar log reader for the webhook dashboard (never forwarded).
        if method == "GET" and path.rstrip("/").endswith("/logs"):
            body = "\n".join(LOG_BUFFER).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return
        # Rich status for the webhook dashboard (never forwarded).
        if method == "GET" and path.rstrip("/").endswith("/status"):
            info = {
                "service": "nd-organizer-proxy",
                "uptime": int(time.time() - STARTED),
                "requests": REQUESTS,
                "lastRequest": int(LAST_REQUEST_TS) if LAST_REQUEST_TS else 0,
                "lastPublish": int(LAST_PUBLISH_TS) if LAST_PUBLISH_TS else 0,
                "inUse": bool(LAST_REQUEST_TS and time.time() - LAST_REQUEST_TS < 300),
                "keywords": list(KEYWORDS),
                "excluded": len(EXCLUDED),
                "weights": len(WEIGHTS),
                "keywordFilter": KEYWORD_FILTER_ENABLED,
                "skipMode": SKIP_MODE,
                "streams": list(STREAMS),
                "filtered": list(FILTERED),
            }
            body = json.dumps(info).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return
        REQUESTS += 1
        LAST_REQUEST_TS = time.time()
        if method == "GET" and path.rstrip("/").endswith("/stream"):
            ids = urllib.parse.parse_qs(query).get("id")
            if ids:
                try:
                    STREAMS.appendleft({"ts": int(time.time()), "id": ids[0]})
                except Exception:
                    pass
        if method == "POST" and path.rstrip("/").endswith("/filters"):
            self._publish_filters()
            return
        # Read the request body so POSTs (scrobble, star, setRating, ...) reach
        # Navidrome intact instead of being silently dropped.
        body = b""
        if method in ("POST", "PUT", "PATCH"):
            try:
                length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(length) if length > 0 else b""
            except Exception:
                body = b""
        try:
            raw, ctype, status = forward(method, path, query, _safe_headers(self.headers), body)
        except Exception as e:
            log.warning("forward %s %s failed: %s", method, path, e)
            self.send_response(502)
            body = json.dumps({"subsonic-response": {"status": "failed", "error": {"code": 0, "message": str(e)}}}).encode()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return

        if "json" in ctype:
            # The client asked for JSON (e.g. f=json) - parse, filter, reorder.
            try:
                obj = json.loads(raw.decode("utf-8", "replace"))
                before = _count_songs(obj)
                obj = filter_json(obj)
                after = _count_songs(obj)
                if before != after:
                    log.info("filtered %s %s: removed %d of %d songs", method, path, before - after, before)
                body = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self._wfile_write(body)
            except Exception as e:
                log.warning("filter failed for %s: %s (passing through)", path, e)
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self._wfile_write(raw)
        else:
            # Binary (stream / cover art / ...) - pass through untouched.
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self._wfile_write(raw)

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")

    def do_PUT(self):
        self._handle("PUT")

    def do_PATCH(self):
        self._handle("PATCH")

    def do_DELETE(self):
        self._handle("DELETE")

    def _publish_filters(self):
        global EXCLUDED, WEIGHTS, KEYWORDS, KEYWORD_FILTER_ENABLED, SKIP_MODE, LAST_PUBLISH_TS
        LAST_PUBLISH_TS = time.time()
        try:
            length = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(length) if length else b"{}"
            body = json.loads(raw.decode("utf-8", "replace"))
            ids = body.get("excluded") or []
            EXCLUDED = {str(i) for i in ids if isinstance(i, (str, int))}
            weights = {}
            for w in body.get("weights") or []:
                if isinstance(w, (list, tuple)) and len(w) >= 2 and isinstance(w[0], (str, int)):
                    try:
                        weights[str(w[0])] = float(w[1])
                    except (TypeError, ValueError):
                        continue
            WEIGHTS = weights
            # Navidrome's fillerKeywords setting is the source of truth when
            # provided; the FILTER_KEYWORDS env value only seeds the default.
            kw = body.get("keywords")
            if isinstance(kw, list):
                pushed = [str(k).strip().lower() for k in kw if str(k).strip()]
                if pushed:
                    KEYWORDS = pushed
            if "keywordFilter" in body:
                KEYWORD_FILTER_ENABLED = bool(body["keywordFilter"])
            mode = body.get("skipMode")
            if isinstance(mode, str) and mode in ("none", "exclude", "third", "lessThanHalf", "half"):
                SKIP_MODE = mode
            log.info(
                "filter set updated: %d skip-heavy IDs, %d weights, %d keywords, keywordFilter=%s, skipMode=%s",
                len(EXCLUDED),
                len(WEIGHTS),
                len(KEYWORDS),
                KEYWORD_FILTER_ENABLED,
                SKIP_MODE,
            )
            out = json.dumps(
                {"ok": True, "excluded": len(EXCLUDED), "weights": len(WEIGHTS), "keywords": len(KEYWORDS), "skipMode": SKIP_MODE}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self._wfile_write(out)
        except Exception as e:
            log.warning("publish_filters failed: %s", e)
            self.send_response(400)
            self.end_headers()


def _count_songs(obj):
    n = 0
    if isinstance(obj, dict):
        for v in obj.values():
            n += _count_songs(v)
    elif isinstance(obj, list):
        for it in obj:
            if isinstance(it, dict) and not it.get("isDir", False) and ("path" in it or "title" in it):
                n += 1
            else:
                n += _count_songs(it)
    return n


def start_heartbeat():
    """Post a liveness heartbeat to the webhook dashboard (WEBHOOK_URL)."""
    import threading

    url = os.environ.get("WEBHOOK_URL", "").rstrip("/")
    if not url:
        return

    def _loop():
        while True:
            time.sleep(60)
            try:
                req = urllib.request.Request(
                    url,
                    data=json.dumps({"service": "proxy", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass

    threading.Thread(target=_loop, daemon=True).start()


if __name__ == "__main__":
    start_heartbeat()
    log.info("=" * 60)
    log.info("nd-organizer Subsonic filtering proxy starting")
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("forwarding to %s", NAVIDROME_URL)
    log.info("filter keywords: %s", KEYWORDS or "(none)")
    log.info("POST /filters {'excluded':[ids], 'weights':[[id,w,plays,skips],...], 'skipMode':..., 'keywordFilter':...} to flag/reorder")
    log.info("queue containers (weight re-sort): %s", ", ".join(sorted(REORDER_CONTAINERS)))
    log.info("filler-keyword tracks dropped from all media responses except explicit user search")
    log.info("skip-heavy limit mode: %s (exclude/third/lessThanHalf/half/none)", SKIP_MODE)
    log.info("keywords ignored from the queue; albums stay whole")
    log.info("point a Subsonic-compatible client at http://<host>:%d/rest/ using Navidrome credentials", PORT)
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
