# Internet radio sidecar for nd-organizer.
#
# Always-on HTTP service that manages Navidrome's internet radio stations via
# the Radio-Browser community database (github.com/WB2024/Add-Navidrome-Radios
# functionality, as a service). It writes stations directly into Navidrome's
# SQLite database (the `radio` table) - the same way the web UI does - so they
# appear immediately without a restart.
#
# The Navidrome plugin (WASM sandbox) cannot touch the host SQLite, so this
# sidecar does it: mount Navidrome's data dir (with navidrome.db) read-write at
# /data, and the webhook dashboard calls its HTTP API to search/add radio.
#
# Endpoints:
#   GET  /health            -> {"ok": true, "db": "...", "exists": bool}
#   GET  /logs              -> recent log lines (for the webhook dashboard)
#   GET  /search?q=<name>&type=byname|bytag|bycountry&limit=N
#       -> {"ok": true, "results": [{name, url, homepage, country, tags, bitrate, votes, codec}]}
#   GET  /top?limit=N       -> top-voted stations
#   GET  /list              -> existing stations in Navidrome
#   POST /add   {"stations": [{name, url, homepage}]}   (dedups by name/url)
#       -> {"ok": true, "added": N, "skipped": N}
#
# All activity is logged to stdout (visible via `docker logs`) and to a ring
# buffer served at /logs.

import base64
import datetime
import hashlib
import json
import logging
import os
import sys
import time
import urllib.parse
import urllib.request
import sqlite3
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import collections

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s [radio] %(message)s", datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("radio")

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

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8100
SERVICE = "nd-organizer-radio"
RADIO_BROWSER_API = os.environ.get("RADIO_BROWSER_API", "https://de1.api.radio-browser.info/json")
DB_PATH = os.environ.get("NAVIDROME_DB", "/data/navidrome.db")
USER_AGENT = "nd-organizer-radio/1.0"

STARTED = time.time()


def generate_id(name):
    unique = f"{name}{datetime.datetime.utcnow().isoformat()}"
    digest = hashlib.md5(unique.encode()).digest()
    return base64.b64encode(digest).decode().rstrip("=").replace("+", "-").replace("/", "_")[:22]


def get_timestamp():
    return datetime.datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S.%f")


def db_connect():
    conn = sqlite3.connect(DB_PATH, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def radio_table_exists():
    try:
        conn = db_connect()
        cur = conn.cursor()
        cur.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='radio'")
        ok = cur.fetchone() is not None
        conn.close()
        return ok
    except Exception:
        return False


def rb_get(path):
    url = RADIO_BROWSER_API.rstrip("/") + path
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read().decode("utf-8", "replace"))


def search_stations(query, search_type="byname", limit=30):
    if search_type == "top":
        return rb_get(f"/stations/topvote/{max(1, limit)}")
    q = urllib.parse.quote(query)
    return rb_get(f"/stations/{search_type}/{q}?limit={max(1, limit)}&hidebroken=true")


def station_exists(cur, name, url):
    cur.execute("SELECT id FROM radio WHERE name = ? OR stream_url = ?", (name, url))
    return cur.fetchone() is not None


def add_stations(stations):
    added = 0
    skipped = 0
    errors = []
    try:
        conn = db_connect()
        cur = conn.cursor()
        for st in stations:
            name = (st.get("name") or "").strip()
            url = (st.get("url") or st.get("stream_url") or "").strip()
            if not name or not url:
                continue
            if station_exists(cur, name, url):
                skipped += 1
                continue
            station_id = generate_id(name)
            ts = get_timestamp()
            homepage = (st.get("homepage") or "").strip()
            cur.execute(
                "INSERT INTO radio (id, name, stream_url, home_page_url, created_at, updated_at) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (station_id, name, url, homepage, ts, ts),
            )
            added += 1
        conn.commit()
        conn.close()
    except Exception as e:
        errors.append(str(e))
    return added, skipped, errors


def list_stations():
    try:
        conn = db_connect()
        cur = conn.cursor()
        cur.execute("SELECT name, stream_url FROM radio ORDER BY name")
        rows = [{"name": r["name"], "url": r["stream_url"]} for r in cur.fetchall()]
        conn.close()
        return rows
    except Exception:
        return []


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def _get_param(self, name, default=None):
        parsed = urllib.parse.urlparse(self.path)
        qs = urllib.parse.parse_qs(parsed.query)
        vals = qs.get(name)
        return vals[0] if vals else default

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path.rstrip("/")
        if path.endswith("/logs"):
            body = "\n".join(LOG_BUFFER).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            return
        if path.endswith("/health"):
            self._send(200, {
                "ok": True, "service": SERVICE, "port": PORT,
                "db": DB_PATH, "exists": os.path.exists(DB_PATH),
                "radioTable": radio_table_exists(),
                "uptime": int(time.time() - STARTED),
            })
            return
        if path.endswith("/status"):
            self._send(200, {
                "ok": True, "service": SERVICE, "uptime": int(time.time() - STARTED),
                "stats": {"stations": len(list_stations()), "db": DB_PATH},
            })
            return
        if path.endswith("/list"):
            self._send(200, {"ok": True, "stations": list_stations()})
            return
        if path.endswith("/top"):
            limit = int(self._get_param("limit", "20") or 20)
            try:
                results = search_stations("", "top", limit)
                self._send(200, {"ok": True, "results": results})
            except Exception as e:
                self._send(200, {"ok": False, "error": str(e)})
            return
        if path.endswith("/search"):
            q = self._get_param("q", "")
            stype = self._get_param("type", "byname")
            limit = int(self._get_param("limit", "30") or 30)
            try:
                results = search_stations(q, stype, limit)
                self._send(200, {"ok": True, "results": results})
            except Exception as e:
                self._send(200, {"ok": False, "error": str(e)})
            return
        self._send(404, {"error": "not found"})

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        if not parsed.path.rstrip("/").endswith("/add"):
            self._send(404, {"error": "not found"})
            return
        try:
            n = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(n) if n > 0 else b"{}"
            req = json.loads(raw or b"{}")
            stations = req.get("stations") or []
        except Exception as e:
            self._send(400, {"error": "bad request: %s" % e})
            return
        if not os.path.exists(DB_PATH):
            self._send(200, {"ok": False, "error": "database not found: %s" % DB_PATH})
            return
        if not radio_table_exists():
            self._send(200, {"ok": False, "error": "radio table not found - is Navidrome initialized?"})
            return
        added, skipped, errors = add_stations(stations)
        log.info("add: %d added, %d skipped (%d stations)", added, skipped, len(stations))
        self._send(200, {"ok": True, "added": added, "skipped": skipped, "errors": errors})


def start_heartbeat():
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
                    data=json.dumps({"service": "radio", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass

    threading.Thread(target=_loop, daemon=True).start()


if __name__ == "__main__":
    start_heartbeat()
    log.info("=" * 60)
    log.info("%s starting (version from tag)", SERVICE)
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("database: %s (exists=%s, radio table=%s)", DB_PATH, os.path.exists(DB_PATH), radio_table_exists())
    if not os.path.exists(DB_PATH):
        log.warning("navidrome.db not found at %s - mount the data dir at /data", DB_PATH)
    elif not radio_table_exists():
        log.warning("radio table missing - Navidrome not fully initialized")
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
