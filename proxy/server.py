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
import urllib.parse
import urllib.request

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s [filter] %(message)s", datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("filter")

NAVIDROME_URL = os.environ.get("NAVIDROME_URL", "http://navidrome:4533").rstrip("/")
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4534
KEYWORDS = [k.strip().lower() for k in os.environ.get("FILTER_KEYWORDS", "intro,outro,interlude,transition,prelude,postlude,christmas,commercial,skit,instrumental,interview").split(",") if k.strip()]
EXCLUDED = set()      # track IDs published by the organizer plugin (skip-heavy, removed entirely)
WEIGHTS = {}          # track ID -> weight (plays - 2*skips); used to reorder returned song lists
# Response containers whose song/entry lists are QUEUE sources: filler-keyword
# tracks are dropped and lists are re-sorted by weight (skipped tracks sink).
# Covers every documented Subsonic/OpenSubsonic song-list endpoint except
# getAlbum (track order) and getNowPlaying / getPlayQueue (live/active queues).
REORDER_CONTAINERS = {
    "randomSongs", "searchResult3", "starred2", "playlist",
    "similarSongs", "similarSongs2", "songsByGenre", "topSongs",
}

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


def should_drop(item, drop_filler):
    if not isinstance(item, dict):
        return False
    # Hard-excluded tracks (net-negative skips, past the cap): removed everywhere,
    # including albums.
    if item.get("id") in EXCLUDED:
        return True
    # Filler keywords are ignored from the QUEUE: dropped from auto-queue lists
    # (random/search/playlist/genre/top/similar) but an album's track list stays
    # whole so intros/outros remain visible in their album.
    return drop_filler and is_filler_title(item.get("title", ""))


def weight_of(item):
    """Weight of a song dict; unweighted tracks sort as 0 (below played, above skipped)."""
    try:
        return float(WEIGHTS.get(str(item.get("id")), 0.0))
    except (TypeError, ValueError):
        return 0.0


def filter_json(obj, own_key=None):
    """Drop filtered song entries and reorder song lists inside auto-queue containers.

    `own_key` is the dict key under which the current object sits. Containers in
    REORDER_CONTAINERS (queue sources) drop filler-keyword tracks and re-sort by
    weight; album / now-playing / play-queue views stay whole (only hard-excluded
    and path-marker tracks are removed there).
    """
    if isinstance(obj, dict):
        new = {k: filter_json(v, k) for k, v in obj.items()}
        for ck in ("song", "entry"):
            lst = new.get(ck)
            if not isinstance(lst, list):
                continue
            if own_key in REORDER_CONTAINERS:
                lst = [it for it in lst if not is_song(it) or not should_drop(it, True)]
                lst = sorted(lst, key=weight_of, reverse=True)
            else:
                lst = [it for it in lst if not is_song(it) or not should_drop(it, False)]
            new[ck] = lst
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
    def log_message(self, fmt, *args):
        log.info("http %s", fmt % args)

    def _handle(self, method):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        query = parsed.query
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
            self.wfile.write(body)
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
                self.wfile.write(body)
            except Exception as e:
                log.warning("filter failed for %s: %s (passing through)", path, e)
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
        else:
            # Binary (stream / cover art / ...) - pass through untouched.
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

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
        global EXCLUDED, WEIGHTS, KEYWORDS
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
            log.info(
                "filter set updated: %d excluded track IDs, %d weights, %d keywords",
                len(EXCLUDED),
                len(WEIGHTS),
                len(KEYWORDS),
            )
            out = json.dumps(
                {"ok": True, "excluded": len(EXCLUDED), "weights": len(WEIGHTS), "keywords": len(KEYWORDS)}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
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


if __name__ == "__main__":
    log.info("=" * 60)
    log.info("nd-organizer Subsonic filtering proxy starting")
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("forwarding to %s", NAVIDROME_URL)
    log.info("filter keywords: %s", KEYWORDS or "(none)")
    log.info("POST /filters {'excluded':[ids], 'weights':[[id,w,plays,skips],...]} to flag/reorder")
    log.info("queue containers (keywords dropped + weight re-sort): %s", ", ".join(sorted(REORDER_CONTAINERS)))
    log.info("keywords ignored from the queue; albums stay whole")
    log.info("point a Subsonic-compatible client at http://<host>:%d/rest/ using Navidrome credentials", PORT)
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
