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
import json
import logging
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    import pymysql
    import pymysql.cursors
except ImportError:
    pymysql = None

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s [mysqlkv] %(message)s", datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("mysqlkv")

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8098

SCHEMA = """
CREATE TABLE IF NOT EXISTS kvstore (
    `key` VARCHAR(256) PRIMARY KEY NOT NULL,
    value LONGBLOB NOT NULL,
    size BIGINT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME DEFAULT NULL
)
"""

NOT_EXPIRED = "(expires_at IS NULL OR expires_at > NOW())"


def connect(db):
    if pymysql is None:
        raise RuntimeError("pymysql not installed")
    return pymysql.connect(
        host=db.get("host", "127.0.0.1"),
        port=int(db.get("port", 3306)),
        user=db.get("user", ""),
        password=db.get("password", ""),
        database=db.get("name", ""),
        autocommit=True,
        connect_timeout=5,
    )


def ensure_schema(conn):
    with conn.cursor() as cur:
        cur.execute(SCHEMA)


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
            return {"error": "unknown op: %s" % op}
    finally:
        conn.close()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        log.info("http %s", fmt % args)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length).decode("utf-8", "replace")) if length else {}
        op = body.get("op")
        db = body.get("db") or {}
        try:
            result = handle(db, op, body)
            out = json.dumps({"result": result}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
        except Exception as e:
            log.warning("op %s failed: %s", op, e)
            out = json.dumps({"result": {"error": "mysql: %s" % e}}).encode()
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)


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
                    data=json.dumps({"service": "mysql", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass

    threading.Thread(target=_loop, daemon=True).start()


if __name__ == "__main__":
    start_heartbeat()
    log.info("=" * 60)
    log.info("nd-organizer MySQL KV sidecar starting")
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("pymysql %s", "ready" if pymysql else "MISSING (pip install pymysql)")
    log.info("point the plugin's persistenceUrl at http://<host>:%d/", PORT)
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
