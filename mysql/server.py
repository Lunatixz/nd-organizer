# nd-organizer MySQL KV sidecar.
#
# The Navidrome plugin's KVStore is host-managed SQLite and cannot be pointed at
# MySQL by the plugin itself. This sidecar is the bridge: the plugin sends its
# kvstore operations over HTTP and this service executes them against the
# user's MySQL/MariaDB database. The plugin still stores/reads the same keys,
# so switching persistence backends keeps the data model identical.
#
# The MySQL connection details come from the PLUGIN's Navidrome config (sent in
# each request body) - nothing is stored here. The sidecar just opens a
# connection per request and mirrors the host KVStore semantics:
#   keys <= 256 bytes, values are blobs, optional TTL (expires_at), and
#   list/get_many/search over a `kvstore` table.
#
# Endpoints (all POST /kv, JSON body):
#   { "op": "health",   "db": {...} }
#   { "op": "get",      "db": {...}, "key": "..." }
#   { "op": "set",      "db": {...}, "key": "...", "value": "<base64>", "ttlSeconds": 0 }
#   { "op": "delete",   "db": {...}, "key": "..." }
#   { "op": "list",     "db": {...}, "prefix": "..." }
#   { "op": "get_many", "db": {...}, "keys": ["..."] }
#   { "op": "has",      "db": {...}, "key": "..." }
#
# db = { "host": "...", "port": 3306, "name": "...", "user": "...", "password": "..." }
#
# Run standalone:
#   python server.py [port]
# Env vars: none required (port defaults to 8098).

import base64
import hashlib
import json
import logging
import os
import sys
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import collections

try:
    import pymysql
    import pymysql.cursors
except ImportError:
    pymysql = None

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s [mysqlkv] %(message)s", datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("mysqlkv")

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

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8098

STARTED = time.time()
OPS = 0
READS = 0    # get/has/list/get_many/health (heartbeat counters)
WRITES = 0   # set/delete
ERRORS = 0
LAST_OP = ""
LAST_OP_TS = 0
_LAST_DB = None  # db params from the most recent successful op (never persisted)

SCHEMA = """
CREATE TABLE IF NOT EXISTS kvstore (
    `key` VARCHAR(256) PRIMARY KEY NOT NULL,
    value LONGBLOB NOT NULL,
    size BIGINT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME DEFAULT NULL
);
CREATE TABLE IF NOT EXISTS listen_history (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    play_key CHAR(40) NOT NULL,
    source VARCHAR(16) NOT NULL,
    listened_at INT UNSIGNED NOT NULL,
    artist VARCHAR(512) NOT NULL,
    track VARCHAR(512) NOT NULL,
    album VARCHAR(512) DEFAULT NULL,
    recording_mbid VARCHAR(36) DEFAULT NULL,
    release_mbid VARCHAR(36) DEFAULT NULL,
    artist_mbid VARCHAR(36) DEFAULT NULL,
    extra TEXT DEFAULT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uq_play (play_key),
    KEY idx_source_at (source, listened_at),
    KEY idx_listened (listened_at)
);
CREATE TABLE IF NOT EXISTS listen_source_state (
    source VARCHAR(32) PRIMARY KEY NOT NULL,
    state MEDIUMTEXT NOT NULL,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
)
"""

NOT_EXPIRED = "(expires_at IS NULL OR expires_at > NOW())"


def connect(db):
    if pymysql is None:
        raise RuntimeError("pymysql not installed")
    host = db.get("host") or os.environ.get("MYSQL_HOST", "127.0.0.1")
    port = int(db.get("port") or os.environ.get("MYSQL_PORT", "3306"))
    user = db.get("user") or os.environ.get("MYSQL_USER", "")
    password = db.get("password") or os.environ.get("MYSQL_PASSWORD", "")
    database = db.get("name") or os.environ.get("MYSQL_DATABASE", "")
    
    # Create database if it doesn't exist
    if database:
        try:
            tmp = pymysql.connect(host=host, port=port, user=user, password=password,
                                  autocommit=True, connect_timeout=5)
            with tmp.cursor() as cur:
                cur.execute(f"CREATE DATABASE IF NOT EXISTS `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci")
            tmp.close()
        except Exception as e:
            log.warning("could not ensure database exists: %s", e)
    
    return pymysql.connect(
        host=host, port=port, user=user, password=password,
        database=database, autocommit=True, connect_timeout=5,
    )


def ensure_schema(conn):
    # SCHEMA holds several CREATE TABLE statements; pymysql's default client
    # flags reject multi-statement execute(), so run them one by one.
    with conn.cursor() as cur:
        for stmt in SCHEMA.split(";"):
            if stmt.strip():
                cur.execute(stmt)


def handle(db, op, body):
    conn = connect(db)
    try:
        ensure_schema(conn)
        with conn.cursor() as cur:
            if op == "health":
                cur.execute("SELECT 1")
                return {"ok": True}
            if op == "get":
                key = body["key"]
                cur.execute(
                    "SELECT value FROM kvstore WHERE `key`=%s AND " + NOT_EXPIRED,
                    (key,),
                )
                row = cur.fetchone()
                if row is None:
                    return {"value": None, "exists": False}
                return {"value": base64.b64encode(row[0]).decode(), "exists": True}
            if op == "has":
                key = body["key"]
                cur.execute(
                    "SELECT COUNT(*) FROM kvstore WHERE `key`=%s AND " + NOT_EXPIRED,
                    (key,),
                )
                return {"exists": cur.fetchone()[0] > 0}
            if op == "set":
                key = body["key"]
                value = base64.b64decode(body["value"]) if body.get("value") else b""
                ttl = int(body.get("ttlSeconds") or 0)
                if len(key) > 256:
                    return {"error": "key exceeds maximum length of 256 bytes"}
                if ttl > 0:
                    cur.execute(
                        """INSERT INTO kvstore (`key`, value, size, expires_at)
                           VALUES (%s, %s, %s, DATE_ADD(NOW(), INTERVAL %s SECOND))
                           ON DUPLICATE KEY UPDATE value=VALUES(value), size=VALUES(size),
                               updated_at=NOW(), expires_at=VALUES(expires_at)""",
                        (key, value, len(value), ttl),
                    )
                else:
                    cur.execute(
                        """INSERT INTO kvstore (`key`, value, size, expires_at)
                           VALUES (%s, %s, %s, NULL)
                           ON DUPLICATE KEY UPDATE value=VALUES(value), size=VALUES(size),
                               updated_at=NOW(), expires_at=NULL""",
                        (key, value, len(value)),
                    )
                return {"ok": True}
            if op == "delete":
                cur.execute("DELETE FROM kvstore WHERE `key`=%s", (body["key"],))
                return {"deleted": cur.rowcount > 0}
            if op == "import":
                # Bulk restore: replace=True wipes the table first (first batch
                # of a restore), later batches just upsert. One transaction per
                # batch - an interrupted restore leaves a partial table, rerun it.
                rows = body.get("rows") or []
                replace = bool(body.get("replace"))
                conn.autocommit(False)
                try:
                    if replace:
                        cur.execute("DELETE FROM kvstore")
                    with_ttl, without = [], []
                    for r in rows:
                        v = base64.b64decode(r["v"]) if r.get("v") else b""
                        t = int(r.get("t") or 0)
                        rec = (r["k"], v, len(v), t)
                        (with_ttl if t > 0 else without).append(rec)
                    if without:
                        cur.executemany(
                            """INSERT INTO kvstore (`key`, value, size, expires_at)
                               VALUES (%s, %s, %s, NULL)
                               ON DUPLICATE KEY UPDATE value=VALUES(value), size=VALUES(size),
                                   updated_at=NOW(), expires_at=NULL""",
                            [x[:3] for x in without],
                        )
                    if with_ttl:
                        cur.executemany(
                            """INSERT INTO kvstore (`key`, value, size, expires_at)
                               VALUES (%s, %s, %s, DATE_ADD(NOW(), INTERVAL %s SECOND))
                               ON DUPLICATE KEY UPDATE value=VALUES(value), size=VALUES(size),
                                   updated_at=NOW(), expires_at=VALUES(expires_at)""",
                            with_ttl,
                        )
                    conn.commit()
                except Exception:
                    conn.rollback()
                    raise
                finally:
                    conn.autocommit(True)
                return {"ok": True, "n": len(rows), "replaced": replace}
            if op == "list":
                prefix = body.get("prefix", "")
                cur.execute(
                    "SELECT `key` FROM kvstore WHERE `key` LIKE %s AND " + NOT_EXPIRED + " ORDER BY `key`",
                    (prefix.replace("%", "\\%").replace("_", "\\_") + "%",),
                )
                return {"keys": [r[0] for r in cur.fetchall()]}
            if op == "get_many":
                keys = body.get("keys") or []
                values = {}
                for key in keys:
                    cur.execute(
                        "SELECT `key`, value FROM kvstore WHERE `key`=%s AND " + NOT_EXPIRED,
                        (key,),
                    )
                    for r in cur.fetchall():
                        values[r[0]] = base64.b64encode(r[1]).decode()
                return {"values": values}
            if op == "history_upsert":
                # Idempotent bulk insert into the union listen history; play_key
                # (sha1 of source|ts|artist|track) makes replays no-ops and lets
                # us count real inserts (INSERT IGNORE -> rowcount).
                rows = body.get("rows") or []
                data = []
                for r in rows:
                    src = str(r.get("source") or "")[:16]
                    ts = int(r.get("listened_at") or 0)
                    artist = str(r.get("artist") or "")
                    track = str(r.get("track") or "")
                    if not src or not ts or not artist or not track:
                        continue
                    play_key = hashlib.sha1(
                        ("%s|%d|%s|%s" % (src, ts, artist, track)).encode("utf-8", "replace")
                    ).hexdigest()
                    album = r.get("album")
                    extra = r.get("extra")
                    data.append((
                        play_key, src, ts,
                        artist[:512], track[:512],
                        str(album)[:512] if album else None,
                        (r.get("recording_mbid") or None),
                        (r.get("release_mbid") or None),
                        (r.get("artist_mbid") or None),
                        json.dumps(extra, ensure_ascii=False) if extra is not None else None,
                    ))
                if not data:
                    return {"ok": True, "inserted": 0}
                cur.executemany(
                    "INSERT IGNORE INTO listen_history "
                    "(play_key, source, listened_at, artist, track, album, "
                    "recording_mbid, release_mbid, artist_mbid, extra) "
                    "VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s)",
                    data,
                )
                return {"ok": True, "inserted": int(cur.rowcount or 0)}
            if op == "history_state_get":
                src = body.get("source")
                cur.execute("SELECT state FROM listen_source_state WHERE source=%s", (src,))
                row = cur.fetchone()
                if not row:
                    return {"state": None}
                try:
                    return {"state": json.loads(row[0])}
                except Exception:
                    return {"state": None}
            if op == "history_state_set":
                src = body.get("source")
                state = body.get("state")
                if not src:
                    return {"error": "source required"}
                blob = state if isinstance(state, str) else json.dumps(state, ensure_ascii=False)
                cur.execute(
                    "INSERT INTO listen_source_state (source, state) VALUES (%s, %s) "
                    "ON DUPLICATE KEY UPDATE state=VALUES(state)",
                    (src, blob),
                )
                return {"ok": True}
            if op == "history_counts":
                cur.execute(
                    "SELECT source, COUNT(*), COALESCE(MIN(listened_at),0), "
                    "COALESCE(MAX(listened_at),0) FROM listen_history GROUP BY source"
                )
                return {"counts": {r[0]: {"count": int(r[1]), "oldest": int(r[2]),
                                          "newest": int(r[3])} for r in cur.fetchall()}}
            if op == "history_missing":
                # Plays every source has EXCEPT the push target - identified by
                # the target row whose play_key would equal sha1(target|ts|a|t).
                target = body.get("target")
                limit = int(body.get("limit") or 50)
                cur.execute(
                    "SELECT h.source, h.listened_at, h.artist, h.track, h.album, "
                    "h.recording_mbid, h.release_mbid, h.artist_mbid, h.extra "
                    "FROM listen_history h "
                    "WHERE h.source <> %s AND NOT EXISTS ("
                    "  SELECT 1 FROM listen_history t WHERE t.source = %s "
                    "  AND t.play_key = SHA1(CONCAT(%s, '|', h.listened_at, '|', h.artist, '|', h.track))) "
                    "ORDER BY h.listened_at LIMIT %s",
                    (target, target, target, limit),
                )
                return {"rows": [
                    {"source": r[0], "listened_at": int(r[1]), "artist": r[2], "track": r[3],
                     "album": r[4], "recording_mbid": r[5], "release_mbid": r[6],
                     "artist_mbid": r[7],
                     "extra": json.loads(r[8]) if r[8] else None}
                    for r in cur.fetchall()
                ]}
            return {"error": "unknown op: %s" % op}
    finally:
        conn.close()


class Handler(BaseHTTPRequestHandler):
    def _wfile_write(self, data):
        """Write a response body, swallowing broken-pipe/reset errors - a client
        that disconnects mid-response is normal and shouldn't dump a traceback."""
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def log_message(self, fmt, *args):
        msg = fmt % args
        t0 = getattr(self, "_t0", None)
        if t0:
            msg += " (%dms)" % int((time.time() - t0) * 1000)
        log.info("http %s", msg)

    def _stream_export(self, db):
        """POST /export {"db": ...} -> one NDJSON line per kvstore row
        ({"k": key, "v": base64, "t": ttl-seconds}) streamed straight from an
        unbuffered cursor, close-delimited (no Content-Length) so a multi-GB
        dump never sits in memory. Used by the webhook's backup."""
        global OPS, READS, ERRORS, LAST_OP, LAST_OP_TS, _LAST_DB
        OPS += 1
        READS += 1
        LAST_OP = "export"
        LAST_OP_TS = int(time.time())
        t0 = time.time()

        def _fail(msg):
            global ERRORS
            ERRORS += 1
            log.error("export failed: %s", msg)
            out = json.dumps({"result": {"error": "mysql: %s" % msg}}).encode()
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self._wfile_write(out)

        if pymysql is None:
            return _fail("pymysql not installed")
        try:
            conn = connect(db)
            ensure_schema(conn)
            _LAST_DB = db
        except Exception as e:
            return _fail(str(e))
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.end_headers()
        self.close_connection = True
        n = 0
        try:
            cur = conn.cursor(pymysql.cursors.SSCursor)  # unbuffered: rows stream from MySQL
            cur.execute(
                "SELECT `key`, value, TIMESTAMPDIFF(SECOND, NOW(), expires_at) "
                "FROM kvstore"
            )
            for key, value, ttl in cur:
                line = json.dumps({
                    "k": key,
                    "v": base64.b64encode(value).decode(),
                    "t": int(ttl) if ttl and ttl > 0 else 0,
                }) + "\n"
                self.wfile.write(line.encode("utf-8"))
                n += 1
                if n % 512 == 0:
                    self.wfile.flush()
            self.wfile.flush()
            cur.close()
        except (BrokenPipeError, ConnectionResetError, OSError):
            log.warning("export: client disconnected after %d rows", n)
        finally:
            conn.close()
        log.info("export streamed %d rows (%dms)", n, int((time.time() - t0) * 1000))

    def do_GET(self):
        self._t0 = time.time()
        if self.path.startswith("/logs"):
            body = "\n".join(LOG_BUFFER).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return
        if self.path.startswith("/status"):
            ver = ""
            try:
                ver = open(os.path.join(os.path.dirname(__file__), "VERSION")).read().strip()
            except Exception:
                pass
            info = {
                "service": "nd-organizer-mysql",
                "version": ver,
                "uptime": int(time.time() - STARTED),
                "ops": OPS,
                "reads": READS,
                "writes": WRITES,
                "errors": ERRORS,
                "lastOp": LAST_OP,
                "lastOpTs": LAST_OP_TS,
                "db": None,
            }
            if _LAST_DB:
                try:
                    conn = connect(_LAST_DB)
                    with conn.cursor() as cur:
                        cur.execute(
                            "SELECT COUNT(*), COALESCE(MAX(updated_at), MAX(created_at)) FROM kvstore"
                        )
                        rows, last_upd = cur.fetchone()
                        cur.execute(
                            "SELECT COALESCE(SUM(data_length + index_length), 0) "
                            "FROM information_schema.tables "
                            "WHERE table_schema = DATABASE() AND table_name = 'kvstore'"
                        )
                        size = cur.fetchone()[0]
                    conn.close()
                    info["db"] = {
                        "rows": int(rows or 0),
                        "bytes": int(size or 0),
                        "lastUpdate": last_upd.strftime("%Y-%m-%d %H:%M:%S") if last_upd else None,
                    }
                except Exception as e:
                    info["dbError"] = str(e)
            body = json.dumps(info).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        global OPS, READS, WRITES, ERRORS, LAST_OP, LAST_OP_TS, _LAST_DB
        self._t0 = time.time()
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length).decode("utf-8", "replace")) if length else {}
        op = body.get("op")
        db = body.get("db") or {}
        OPS += 1
        LAST_OP = str(op)
        LAST_OP_TS = int(time.time())
        # Bulk export streams NDJSON rows instead of one JSON envelope - see
        # _stream_export; it can be multi-GB, so it never buffers in memory.
        if op == "export":
            self._stream_export(db)
            return
        mutating = op in ("set", "delete", "import", "history_upsert", "history_state_set")
        detail = body.get("key") or body.get("prefix") or ""
        if op == "get_many":
            detail = "%d keys" % len(body.get("keys") or [])
        t0 = time.time()
        try:
            result = handle(db, op, body)
            _LAST_DB = db
            dt = int((time.time() - t0) * 1000)
            if mutating:
                WRITES += 1
                log.info("op %s %s (%dms)", op, detail, dt)
            else:
                READS += 1
                if dt > 100:
                    log.info("op %s %s slow (%dms)", op, detail, dt)
            out = json.dumps({"result": result}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self._wfile_write(out)
        except Exception as e:
            ERRORS += 1
            log.exception("op %s %s failed", op, detail)
            out = json.dumps({"result": {"error": "mysql: %s" % e}}).encode()
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self._wfile_write(out)


def start_heartbeat():
    """Local log heartbeat every 60s + optional liveness post to the webhook
    dashboard (WEBHOOK_URL)."""
    import threading

    url = os.environ.get("WEBHOOK_URL", "").rstrip("/")

    def _log_loop():
        while True:
            time.sleep(60)
            log.info(
                "heartbeat: uptime=%ds ops=%d reads=%d writes=%d errors=%d lastOp=%s",
                int(time.time() - STARTED), OPS, READS, WRITES, ERRORS, LAST_OP or "-",
            )

    def _post_loop():
        while True:
            time.sleep(60)
            try:
                req = urllib.request.Request(
                    url,
                    data=json.dumps({"service": "mysql", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass

    threading.Thread(target=_log_loop, daemon=True).start()
    if url:
        threading.Thread(target=_post_loop, daemon=True).start()


if __name__ == "__main__":
    start_heartbeat()
    log.info("=" * 60)
    log.info("nd-organizer MySQL KV sidecar starting")
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("pymysql %s", "ready" if pymysql else "MISSING (pip install pymysql)")
    log.info("point the plugin's persistenceUrl at http://<host>:%d/", PORT)
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
