# AcoustID fingerprint sidecar for nd-organizer.
#
# The Navidrome plugin cannot decode audio or run chromaprint in its WASM
# sandbox, so this small Docker service does it: it reads the audio file
# (the library must be mounted at the same path Navidrome sees), fingerprints it
# with fpcalc, queries the AcoustID API, and returns the matched recordings /
# release groups (MBIDs) the plugin uses to pair songs to albums.
#
# Endpoints:
#   GET  /health                     -> {"ok": true, "service": "...", "libraryMounts": [...]}
#   POST /lookup  {"path": "...", "acoustidApiKey": "..."}
#       -> {"ok": true, "matches": [ {recordingId, title, artist, score,
#             releaseGroups:[{id, title, type}]}, ... ]}
#
# All activity is logged to stdout (visible via `docker logs`):
#   - startup: port + which library mounts are visible
#   - every /lookup: file path, fingerprint duration, AcoustID result count or error
#   - any request from the plugin proves the plugin->sidecar link works

import json
import logging
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s [acoustid] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("acoustid")

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8097
FPCALC_LENGTH = 120  # seconds; AcoustID recommends ~120s
SERVICE = "nd-organizer-acoustid"

COMMON_MOUNTS = ["/music", "/unsorted", "/mnt/music", "/mnt/unsorted", "/data/music"]


def startup_banner():
    log.info("=" * 60)
    log.info("%s starting (version from tag)", SERVICE)
    log.info("listening on 0.0.0.0:%d", PORT)
    visible = 0
    for m in COMMON_MOUNTS:
        if os.path.isdir(m):
            visible += 1
            log.info("library mount visible: %s (readable=%s)", m, os.access(m, os.R_OK))
    if visible == 0:
        log.warning(
            "no library mounts found at common paths (%s). "
            "Mount your music at the SAME paths Navidrome uses, e.g. /music, /unsorted",
            ", ".join(COMMON_MOUNTS),
        )
    log.info("=" * 60)


def fpcalc(path):
    t0 = time.time()
    try:
        out = subprocess.run(
            ["fpcalc", "-json", "-length", str(FPCALC_LENGTH), path],
            capture_output=True,
            text=True,
            timeout=600,
        )
    except FileNotFoundError:
        log.error("fpcalc not found (install libchromaprint-tools)")
        return None, "fpcalc not found (install libchromaprint-tools)"
    except Exception as e:
        log.error("fpcalc exception for %s: %s", path, e)
        return None, str(e)
    dt = time.time() - t0
    if out.returncode != 0:
        log.warning("fpcalc failed for %s (%.1fs): %s", path, dt, out.stderr.strip()[:200])
        return None, "fpcalc failed: %s" % out.stderr.strip()
    try:
        data = json.loads(out.stdout)
        log.info("fingerprinted %s (%.1fs, duration=%s, fp length=%d)", path, dt, data.get("duration"), len(data.get("fingerprint", "")))
        return data, None
    except Exception as e:
        log.error("bad fpcalc output for %s: %s", path, e)
        return None, "bad fpcalc output: %s" % e


def acoustid_lookup(apikey, duration, fingerprint):
    q = urllib.parse.urlencode({
        "client": apikey,
        "duration": duration,
        "fingerprint": fingerprint,
        "meta": "recordings+releasegroups+sources",
        "format": "json",
    })
    t0 = time.time()
    try:
        with urllib.request.urlopen("https://api.acoustid.org/v2/lookup?" + q, timeout=30) as r:
            data = json.loads(r.read().decode())
        log.info("AcoustID lookup ok (%.2fs, status=%s)", time.time() - t0, data.get("status"))
        return data, None
    except Exception as e:
        log.warning("AcoustID lookup failed (%.2fs): %s", time.time() - t0, e)
        return None, str(e)


def top_matches(data):
    out = []
    for r in data.get("results", [])[:5]:
        for rec in r.get("recordings", [])[:3]:
            artist = ""
            if rec.get("artists"):
                artist = rec["artists"][0].get("name", "")
            out.append({
                "recordingId": rec.get("id", ""),
                "title": rec.get("title", ""),
                "artist": artist,
                "score": round(r.get("score", 0.0), 4),
                "duration": r.get("duration"),
                "releaseGroups": [
                    {"id": rg.get("id", ""), "title": rg.get("title", ""), "type": rg.get("type", "")}
                    for rg in rec.get("releasegroups", [])[:5]
                ],
            })
    return out


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        # route the built-in request line through our logger at DEBUG-ish level
        log.info("http %s", fmt % args)

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/health"):
            mounts = [m for m in COMMON_MOUNTS if os.path.isdir(m)]
            log.info("health check from %s (mounts=%s)", self.client_address[0], mounts)
            self._send(200, {"ok": True, "service": SERVICE, "port": PORT, "libraryMounts": mounts})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        try:
            n = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(n) if n > 0 else b"{}"
            req = json.loads(raw or b"{}")
        except Exception as e:
            log.error("bad request from %s: %s", self.client_address[0], e)
            return self._send(400, {"error": "bad request: %s" % e})

        path = req.get("path", "")
        apikey = req.get("acoustidApiKey", "")
        if not path or not apikey:
            log.warning("missing path or acoustidApiKey from %s", self.client_address[0])
            return self._send(400, {"error": "path and acoustidApiKey are required"})

        log.info("lookup request from %s for %s", self.client_address[0], path)
        if not os.path.exists(path):
            log.error("file not found: %s (is this mount the same path as Navidrome sees?)", path)
            return self._send(200, {"ok": False, "error": "file not found: %s" % path})

        data, err = fpcalc(path)
        if err or data is None:
            return self._send(200, {"ok": False, "error": err or "could not fingerprint"})

        res, err = acoustid_lookup(apikey, data.get("duration", 0), data.get("fingerprint", ""))
        if err:
            return self._send(200, {"ok": False, "error": err})
        if not res or res.get("status") != "ok":
            msg = res.get("error", {}).get("message", "lookup failed")
            log.warning("AcoustID lookup reported error: %s", msg)
            return self._send(200, {"ok": False, "error": msg})

        matches = top_matches(res)
        log.info("AcoustID result for %s: %d match(es) (top: %s)", path, len(matches),
                 matches[0]["title"] if matches else "none")
        return self._send(200, {"ok": True, "matches": matches})


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
                    data=json.dumps({"service": "acoustid", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass

    threading.Thread(target=_loop, daemon=True).start()


if __name__ == "__main__":
    start_heartbeat()
    startup_banner()
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
