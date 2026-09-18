# AcoustID fingerprint + ReplayGain sidecar for nd-organizer.
#
# The Navidrome plugin cannot decode audio or run chromaprint/ffmpeg in its WASM
# sandbox, so this small Docker service does it: it reads the audio file
# (the library must be mounted at the same path Navidrome sees), fingerprints it
# with fpcalc, queries the AcoustID API, and returns the matched recordings /
# release groups (MBIDs) the plugin uses to pair songs to albums. It also
# computes ReplayGain track gain/peak (ffmpeg EBU R128) for loudness tags.
#
# Endpoints:
#   GET  /health                     -> {"ok": true, "service": "...", "libraryMounts": [...]}
#   POST /lookup  {"path": "...", "acoustidApiKey": "..."}
#       -> {"ok": true, "matches": [ {recordingId, title, artist, score,
#             releaseGroups:[{id, title, type}]}, ... ], "replaygain": {gain, peak}}
#   POST /replaygain {"path": "..."}
#       -> {"ok": true, "replaygain": {"integrated": -18.2, "peak": 0.98}}
#
# All activity is logged to stdout (visible via `docker logs`):
#   - startup: port + which library mounts are visible
#   - every /lookup: file path, fingerprint duration, AcoustID result count or error
#   - every /replaygain: file path, loudness analysis duration, gain/peak
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

import collections

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s [acoustid] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("acoustid")

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

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8097
FPCALC_LENGTH = 120  # seconds; AcoustID recommends ~120s
SERVICE = "nd-organizer-acoustid"
CACHE_DIR = "/data/plugins/nd-organizer/sidecar-cache"
JOB_TTL = 600  # 10 minutes — auto-expire stale jobs

STARTED = time.time()
STATS = {"lookups": 0, "matches": 0, "errors": 0, "lastLookup": 0, "lastMatch": 0}

COMMON_MOUNTS = ["/music", "/unsorted", "/mnt/music", "/mnt/unsorted", "/data/music"]

# Job queue: job_id -> {batches, files, results, processing, created_at}
_jobs = {}
# Idempotency cache: "path:mtime" -> result dict
_results_cache = {}


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
    os.makedirs(CACHE_DIR, exist_ok=True)
    log.info("=" * 60)


def _expire_jobs():
    """Evict jobs older than JOB_TTL seconds."""
    now = time.time()
    expired = [jid for jid, j in _jobs.items() if now - j["created_at"] > JOB_TTL]
    for jid in expired:
        log.info("expiring stale job %s (age %.0fs)", jid, now - _jobs[jid]["created_at"])
        # Clean up disk cache
        cache_path = os.path.join(CACHE_DIR, f"{jid}.json")
        if os.path.exists(cache_path):
            try:
                os.remove(cache_path)
            except OSError:
                pass
        del _jobs[jid]


def _process_job_file(job_id, entry, apikey):
    """Process a single file: fpcalc + AcoustID lookup + replaygain.
    Returns a result dict. Checks idempotency cache first."""
    path = entry.get("path", "")
    mtime = entry.get("mtime", 0)
    if not path:
        return {"path": path, "ok": False, "error": "no path"}

    # Idempotency: skip if already processed
    cache_key = f"{path}:{mtime}"
    if cache_key in _results_cache:
        return _results_cache[cache_key]

    # Validate mtime hasn't changed
    if not os.path.exists(path):
        result = {"path": path, "ok": False, "error": "file not found", "transient": True}
        _results_cache[cache_key] = result
        return result
    try:
        current_mtime = os.path.getmtime(path)
        if abs(current_mtime - mtime) > 1:
            result = {"path": path, "ok": False, "error": f"mtime mismatch: expected {mtime}, got {current_mtime}", "transient": True}
            _results_cache[cache_key] = result
            return result
    except OSError:
        pass

    STATS["lookups"] += 1
    data, err = fpcalc(path)
    if err or data is None:
        STATS["errors"] += 1
        result = {"path": path, "ok": False, "error": err or "fingerprint failed"}
        _results_cache[cache_key] = result
        return result

    res, err = acoustid_lookup(apikey, data.get("duration", 0), data.get("fingerprint", ""))
    if err:
        STATS["errors"] += 1
        # Distinguish transient (rate limit, network) from permanent errors
        transient = "429" in err or "timeout" in err.lower() or "connection" in err.lower()
        result = {"path": path, "ok": False, "error": err, "transient": transient}
        _results_cache[cache_key] = result
        return result
    if not res or res.get("status") != "ok":
        msg = res.get("error", {}).get("message", "lookup failed") if res else "no response"
        STATS["errors"] += 1
        result = {"path": path, "ok": False, "error": msg}
        _results_cache[cache_key] = result
        return result

    matches = top_matches(res)
    if matches:
        STATS["matches"] += len(matches)
        STATS["lastMatch"] = int(time.time())

    rg, rg_err = replaygain(path)
    entry_result = {"path": path, "ok": True, "matches": matches}
    if rg is not None:
        entry_result["replaygain"] = rg
    _results_cache[cache_key] = entry_result
    return entry_result


def _process_job(job_id):
    """Background thread: process all files in a job, write results incrementally."""
    job = _jobs.get(job_id)
    if not job:
        return
    apikey = job.get("apikey", "")
    log.info("job %s: starting processing (%d files)", job_id, len(job["files"]))
    results = []
    for i, entry in enumerate(job["files"]):
        if job_id not in _jobs:
            log.info("job %s: cancelled (removed)", job_id)
            return
        result = _process_job_file(job_id, entry, apikey)
        results.append(result)
        job["results"][len(results) - 1] = result
        job["done"] = len(results)
        # Write incremental results to disk
        if len(results) % 50 == 0:
            _save_job(job_id, job)
    job["processing"] = False
    job["done"] = len(results)
    _save_job(job_id, job)
    STATS["lastLookup"] = int(time.time())
    log.info("job %s: completed (%d files processed)", job_id, len(results))


def _save_job(job_id, job):
    """Persist job state to disk for crash recovery."""
    try:
        os.makedirs(CACHE_DIR, exist_ok=True)
        path = os.path.join(CACHE_DIR, f"{job_id}.json")
        # Only save serializable parts
        save_data = {
            "job_id": job_id,
            "apikey": job.get("apikey", ""),
            "total_batches": job.get("total_batches", 0),
            "received_batches": len(job.get("batches", {})),
            "files": job.get("files", []),
            "done": job.get("done", 0),
            "processing": job.get("processing", False),
            "created_at": job.get("created_at", 0),
        }
        with open(path, "w") as f:
            json.dump(save_data, f)
    except Exception as e:
        log.warning("failed to save job %s: %s", job_id, e)


def _load_jobs_from_disk():
    """Load incomplete jobs from disk on startup."""
    if not os.path.isdir(CACHE_DIR):
        return
    now = time.time()
    for fname in os.listdir(CACHE_DIR):
        if not fname.endswith(".json"):
            continue
        try:
            with open(os.path.join(CACHE_DIR, fname)) as f:
                data = json.load(f)
            job_id = data.get("job_id", "")
            if not job_id:
                continue
            # Expire old jobs
            if now - data.get("created_at", 0) > JOB_TTL:
                os.remove(os.path.join(CACHE_DIR, fname))
                log.info("expired stale job %s from disk", job_id)
                continue
            # Restore job state
            _jobs[job_id] = {
                "batches": {},  # batches are ephemeral, files are what matter
                "files": data.get("files", []),
                "results": {},
                "processing": False,  # don't resume processing on restart
                "done": data.get("done", 0),
                "apikey": data.get("apikey", ""),
                "created_at": data.get("created_at", 0),
                "total_batches": data.get("total_batches", 0),
            }
            log.info("restored job %s from disk (%d files)", job_id, len(data.get("files", [])))
        except Exception as e:
            log.warning("failed to load job %s: %s", fname, e)


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
        "meta": "recordings+releasegroups+sources+isrcs",
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


def replaygain(path):
    """Compute loudness with ffmpeg's EBU R128: integrated loudness (LUFS) and
    true peak (linear). The plugin derives ReplayGain gain from its configurable
    reference (`gain = reference - integrated`). Returns ({"integrated", "peak"}, err).
    """
    t0 = time.time()
    try:
        out = subprocess.run(
            ["ffmpeg", "-nostats", "-i", path, "-filter:a", "ebur128=peak=true", "-f", "null", "-"],
            capture_output=True,
            text=True,
            timeout=900,
        )
    except FileNotFoundError:
        return None, "ffmpeg not found (install ffmpeg)"
    except Exception as e:
        return None, str(e)
    stderr = out.stderr
    integrated = None
    peak_db = None
    for line in stderr.splitlines():
        if "Integrated loudness:" in line and "LUFS" in line:
            try:
                integrated = float(line.split("I:")[1].split("LUFS")[0].strip())
            except (ValueError, IndexError):
                pass
        if "True peak:" in line and "dBFS" in line:
            try:
                peak_db = float(line.split("TP:")[1].split("dBFS")[0].strip())
            except (ValueError, IndexError):
                pass
    dt = time.time() - t0
    if integrated is None:
        log.warning("replaygain: could not parse loudness for %s (%.1fs): %s", path, dt, stderr.strip()[:200])
        return None, "could not compute loudness: %s" % stderr.strip()[:200]
    peak = round(10 ** (peak_db / 20.0), 6) if peak_db is not None else None
    log.info("replaygain %s (%.1fs): integrated=%s LUFS peak=%s", path, dt, integrated, peak)
    return {"integrated": round(integrated, 2), "peak": peak}, None


class Handler(BaseHTTPRequestHandler):
    def _wfile_write(self, data):
        """Write a response body, swallowing broken-pipe/reset errors - a client
        that disconnects mid-response is normal and shouldn't dump a traceback."""
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def log_message(self, fmt, *args):
        # route the built-in request line through our logger at DEBUG-ish level
        log.info("http %s", fmt % args)

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self._wfile_write(body)

    def do_GET(self):
        if self.path.startswith("/logs"):
            body = "\n".join(LOG_BUFFER).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self._wfile_write(body)
            return
        if self.path.startswith("/status"):
            self._send(200, {
                "ok": True, "service": SERVICE,
                "uptime": int(time.time() - STARTED),
                "stats": STATS,
                "replaygains": STATS.get("replaygains", 0),
            })
            return
        if self.path.startswith("/health"):
            mounts = [m for m in COMMON_MOUNTS if os.path.isdir(m)]
            ver = ""
            try:
                ver = open(os.path.join(os.path.dirname(__file__), "VERSION")).read().strip()
            except Exception:
                pass
            log.info("health check from %s (mounts=%s)", self.client_address[0], mounts)
            self._send(200, {"ok": True, "service": SERVICE, "port": PORT, "libraryMounts": mounts, "version": ver})
            return
        # Job status: GET /status?job_id=xxx
        if self.path.startswith("/job-status"):
            params = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(self.path).query))
            job_id = params.get("job_id", "")
            _expire_jobs()
            job = _jobs.get(job_id)
            if not job:
                self._send(404, {"ok": False, "error": "job not found"})
                return
            self._send(200, {
                "ok": True,
                "processing": job["processing"],
                "done": job.get("done", 0),
                "total": len(job.get("files", [])),
                "errors": sum(1 for r in job.get("results", {}).values() if not r.get("ok", True)),
            })
            return
        # Job results: GET /job-results?job_id=xxx
        if self.path.startswith("/job-results"):
            params = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(self.path).query))
            job_id = params.get("job_id", "")
            job = _jobs.get(job_id)
            if not job:
                self._send(404, {"ok": False, "error": "job not found"})
                return
            self._send(200, {
                "ok": True,
                "results": job.get("results", {}),
                "processing": job["processing"],
            })
            return
        self._send(404, {"error": "not found"})

    def _handle_batch(self, req):
        """Process multiple files: fpcalc + AcoustID lookup + replaygain."""
        files = req.get("files", [])
        apikey = req.get("acoustidApiKey", "")
        if not files:
            return self._send(400, {"error": "no files provided"})
        if not apikey:
            return self._send(400, {"error": "acoustidApiKey required"})

        results = []
        batch_start = time.time()
        for i, entry in enumerate(files):
            # Rate limit: AcoustID allows 3 requests/second
            if i > 0 and time.time() - batch_start < 0.35 * i:
                time.sleep(0.35 - (time.time() - batch_start - 0.35 * (i - 1)))
            path = entry.get("path", "")
            path = entry.get("path", "")
            mtime = entry.get("mtime", 0)
            if not path or not os.path.exists(path):
                results.append({"path": path, "ok": False, "error": "file not found"})
                continue

            STATS["lookups"] += 1
            data, err = fpcalc(path)
            if err or data is None:
                STATS["errors"] += 1
                results.append({"path": path, "ok": False, "error": err or "fingerprint failed"})
                continue

            res, err = acoustid_lookup(apikey, data.get("duration", 0), data.get("fingerprint", ""))
            if err:
                STATS["errors"] += 1
                results.append({"path": path, "ok": False, "error": err})
                continue
            if not res or res.get("status") != "ok":
                msg = res.get("error", {}).get("message", "lookup failed") if res else "no response"
                STATS["errors"] += 1
                results.append({"path": path, "ok": False, "error": msg})
                continue

            matches = top_matches(res)
            if matches:
                STATS["matches"] += len(matches)
                STATS["lastMatch"] = int(time.time())

            rg, rg_err = replaygain(path)
            entry_result = {"path": path, "ok": True, "matches": matches}
            if rg is not None:
                entry_result["replaygain"] = rg
            results.append(entry_result)

        STATS["lastLookup"] = int(time.time())
        log.info("batch: processed %d files", len(results))
        return self._send(200, {"ok": True, "processed": len(results), "results": results})

    def do_POST(self):
        try:
            n = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(n) if n > 0 else b"{}"
            req = json.loads(raw or b"{}")
        except Exception as e:
            log.error("bad request from %s: %s", self.client_address[0], e)
            return self._send(400, {"error": "bad request: %s" % e})

        path = req.get("path", "")
        # Async job: receive batches, process in background
        if self.path.startswith("/job"):
            _expire_jobs()
            job_id = req.get("job_id", "")
            if not job_id:
                return self._send(400, {"error": "job_id required"})
            batch_index = req.get("batch_index", 0)
            batch_total = req.get("batch_total", 1)
            files = req.get("files", [])
            apikey = req.get("acoustidApiKey", "")
            if not files:
                return self._send(400, {"error": "no files provided"})
            # Create or update job
            if job_id not in _jobs:
                _jobs[job_id] = {
                    "batches": {}, "files": [], "results": {},
                    "processing": False, "created_at": time.time(),
                    "apikey": apikey, "total_batches": batch_total, "done": 0,
                }
            job = _jobs[job_id]
            # Deduplicate by batch_index
            if batch_index in job["batches"]:
                self._send(200, {"ok": True, "deduplicated": True, "received": len(job["batches"]), "total": batch_total})
                return
            job["batches"][batch_index] = files
            job["files"].extend(files)
            log.info("job %s: received batch %d/%d (%d files)", job_id, batch_index + 1, batch_total, len(files))
            _save_job(job_id, job)
            # Start processing when all batches received
            if len(job["batches"]) >= batch_total and not job["processing"]:
                job["processing"] = True
                _save_job(job_id, job)
                import threading
                threading.Thread(target=_process_job, args=(job_id,), daemon=True).start()
            self._send(200, {"ok": True, "received": len(job["batches"]), "total": batch_total})
            return
        # Batch processing: process multiple files in one request (legacy sync).
        if self.path.startswith("/batch"):
            return self._handle_batch(req)
        # ReplayGain-only request: compute loudness for any file, no AcoustID.
        if self.path.startswith("/replaygain"):
            if not path or not os.path.exists(path):
                STATS["errors"] += 1
                return self._send(200, {"ok": False, "error": "file not found: %s" % path})
            rg, err = replaygain(path)
            if err or rg is None:
                STATS["errors"] += 1
                return self._send(200, {"ok": False, "error": err or "could not compute replaygain"})
            STATS["replaygains"] = STATS.get("replaygains", 0) + 1
            return self._send(200, {"ok": True, "replaygain": rg})

        if self.path.startswith("/submit"):
            # Submit fingerprint to AcoustID for tracks with MusicBrainz IDs.
            apikey = req.get("acoustidApiKey", "")
            recording_id = req.get("recordingId", "")
            if not apikey or not recording_id:
                return self._send(400, {"error": "acoustidApiKey and recordingId required"})
            if not path or not os.path.exists(path):
                STATS["errors"] += 1
                return self._send(200, {"ok": False, "error": "file not found: %s" % path})
            data, err = fpcalc(path)
            if err or data is None:
                STATS["errors"] += 1
                return self._send(200, {"ok": False, "error": err or "fingerprint failed"})
            # Submit to AcoustID
            submit_data = urllib.parse.urlencode({
                "client": apikey,
                "duration": data.get("duration", 0),
                "fingerprint": data.get("fingerprint", ""),
                "recordingid": recording_id,
            }).encode()
            try:
                req_url = "https://api.acoustid.org/v2/submit"
                http_req = urllib.request.Request(req_url, data=submit_data, method="POST")
                with urllib.request.urlopen(http_req, timeout=30) as r:
                    result = json.loads(r.read().decode())
                if result.get("status") == "ok":
                    log.info("Submitted fingerprint for %s (recording=%s)", path, recording_id)
                    return self._send(200, {"ok": True, "submitted": True})
                else:
                    msg = result.get("error", {}).get("message", "submit failed")
                    return self._send(200, {"ok": False, "error": msg})
            except Exception as e:
                return self._send(200, {"ok": False, "error": str(e)})

        apikey = req.get("acoustidApiKey", "")
        if not path or not apikey:
            log.warning("missing path or acoustidApiKey from %s", self.client_address[0])
            return self._send(400, {"error": "path and acoustidApiKey are required"})

        STATS["lookups"] += 1
        STATS["lastLookup"] = int(time.time())
        log.info("lookup request from %s for %s", self.client_address[0], path)
        if not os.path.exists(path):
            STATS["errors"] += 1
            log.error("file not found: %s (is this mount the same path as Navidrome sees?)", path)
            return self._send(200, {"ok": False, "error": "file not found: %s" % path})

        data, err = fpcalc(path)
        if err or data is None:
            STATS["errors"] += 1
            return self._send(200, {"ok": False, "error": err or "could not fingerprint"})

        res, err = acoustid_lookup(apikey, data.get("duration", 0), data.get("fingerprint", ""))
        if err:
            STATS["errors"] += 1
            return self._send(200, {"ok": False, "error": err})
        if not res or res.get("status") != "ok":
            msg = res.get("error", {}).get("message", "lookup failed")
            STATS["errors"] += 1
            log.warning("AcoustID lookup reported error: %s", msg)
            return self._send(200, {"ok": False, "error": msg})

        matches = top_matches(res)
        if matches:
            STATS["matches"] += len(matches)
            STATS["lastMatch"] = int(time.time())
        log.info("AcoustID result for %s: %d match(es) (top: %s)", path, len(matches),
                 matches[0]["title"] if matches else "none")
        rg, rg_err = replaygain(path)
        resp = {"ok": True, "matches": matches}
        if rg is not None:
            resp["replaygain"] = rg
        elif rg_err:
            log.warning("replaygain for %s: %s", path, rg_err)
        return self._send(200, resp)


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
    _load_jobs_from_disk()
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
