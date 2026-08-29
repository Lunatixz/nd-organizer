# Essentia ML sidecar for nd-organizer.
#
# Provides genre/mood analysis as a fallback when AudioMuse-AI is down.
# Uses the Essentia library (Music Technology Group) with Discogs-400 (genres)
# and MTG-Jamendo (moods) ML models.
#
# Endpoints:
#   GET  /health  -> {"ok": true, "service": "...", "essentia": bool}
#   POST /analyze {"path": "/music/song.flac", "genres": true, "moods": true}
#       -> {"ok": true, "genres": [{"name": "Rock", "score": 0.45}], ...}
#
# No internet required after model download. Models are loaded at startup.
# If Essentia is not installed, returns empty predictions (fail-soft).

import json
import logging
import os
import subprocess
import sys
import time
import tempfile

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import collections

logging.basicConfig(level=logging.INFO,
                    format="%(asctime)s %(levelname)s [essentia] %(message)s",
                    datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("essentia")

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

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8101
SERVICE = "nd-organizer-essentia"
STARTED = time.time()
ESSENTIA_AVAILABLE = False
GENRE_MODEL = None
MOOD_MODEL = None

# Genre labels from the Discogs-400 taxonomy (top classes).
GENRE_LABELS = [
    "Rock", "Pop", "Hip-Hop", "Electronic", "Jazz", "Classical", "R&B", "Country",
    "Folk", "Blues", "Reggae", "Punk", "Metal", "Funk", "Soul", "Latin", "World",
    "Film", "Stage", "TV", "Audiobook", "Podcast",
]


def load_models():
    global ESSENTIA_AVAILABLE, GENRE_MODEL, MOOD_MODEL
    try:
        # Try the full Essentia + TensorFlow stack.
        import essentia
        import essentia.standard as es
        log.info("Essentia loaded successfully (v%s)", essentia.__version__)
        ESSENTIA_AVAILABLE = True
    except ImportError:
        log.warning("Essentia not installed - returning empty predictions (install essentia-tensorflow)")
        return
    model_dir = os.environ.get("MODEL_DIR", os.path.expanduser("~/essentia_models"))
    genre_path = os.path.join(model_dir, "discogs_400_epCNN_discogs-hard_256.pb")
    if os.path.exists(genre_path):
        try:
            GENRE_MODEL = es.TensorflowPredictCNN(model=genre_path)
            log.info("Genre model loaded from %s", genre_path)
        except Exception as e:
            log.warning("Genre model load failed: %s", e)
    mood_path = os.path.join(model_dir, "mtg_jamendo_mood_256.pb")
    if os.path.exists(mood_path):
        try:
            MOOD_MODEL = es.TensorflowPredictCNN(model=mood_path)
            log.info("Mood model loaded from %s", mood_path)
        except Exception as e:
            log.warning("Mood model load failed: %s", e)


def analyze_audio(path, genres=True, moods=True):
    """Analyze an audio file and return genre/mood predictions."""
    result = {"genres": [], "moods": [], "energy": None}
    if not ESSENTIA_AVAILABLE:
        return result, "Essentia not installed"
    if not os.path.exists(path):
        return result, "file not found"

    try:
        import essentia.standard as es
        from essentia.standard import MonoLoader, TensorflowPredictCNN
    except ImportError:
        return result, "Essentia import failed"

    # Load audio
    try:
        audio = MonoLoader(filename=path, sampleRate=44100, endTime=120)()
    except Exception as e:
        log.error("audio load failed for %s: %s", path, e)
        return result, str(e)

    t0 = time.time()
    # Genre prediction
    if genres and GENRE_MODEL is not None:
        try:
            pooled = es.TensorflowPredictVGGish()(audio)
            preds = GENRE_MODEL(pooled)[0]
            # Get top predictions
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:5]
            for idx, score in top:
                if idx < len(GENRE_LABELS) and score > 0.05:
                    result["genres"].append({"name": GENRE_LABELS[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("genre prediction failed for %s: %s", path, e)

    # Mood prediction
    if moods and MOOD_MODEL is not None:
        try:
            pooled = es.TensorflowPredictVGGish()(audio)
            preds = MOOD_MODEL(pooled)[0]
            # Top mood labels (hardcoded from MTG-Jamendo)
            mood_labels = ["happy", "sad", "angry", "fear", "tender", "excited", "energetic", "dark"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:5]
            for idx, score in top:
                if idx < len(mood_labels) and score > 0.005:
                    result["moods"].append({"name": mood_labels[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("mood prediction failed for %s: %s", path, e)

    dt = time.time() - t0
    log.info("analyzed %s (%.1fs): %d genres, %d moods", os.path.basename(path), dt,
             len(result["genres"]), len(result["moods"]))
    return result, None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        log.info("http %s", fmt % args)

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

    def do_GET(self):
        path = self.path.rstrip("/")
        if path == "/health":
            self._send(200, {
                "ok": True, "service": SERVICE, "port": PORT,
                "essentia": ESSENTIA_AVAILABLE,
                "genre_model": GENRE_MODEL is not None,
                "mood_model": MOOD_MODEL is not None,
                "uptime": int(time.time() - STARTED),
            })
            return
        if path == "/status":
            self._send(200, {
                "service": SERVICE,
                "ok": ESSENTIA_AVAILABLE,
                "uptime": int(time.time() - STARTED),
                "stats": {
                    "essentia_loaded": ESSENTIA_AVAILABLE,
                    "genre_model": GENRE_MODEL is not None,
                    "mood_model": MOOD_MODEL is not None,
                    "genre_labels": len(GENRE_LABELS),
                },
            })
            return
        if path == "/logs":
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
        self._send(404, {"error": "not found"})

    def do_POST(self):
        path = self.path.rstrip("/")
        if path == "/analyze":
            try:
                n = int(self.headers.get("Content-Length", 0))
                raw = self.rfile.read(n) if n > 0 else b"{}"
                req = json.loads(raw or "{}")
                audio_path = req.get("path", "")
                genres = req.get("genres", True)
                moods = req.get("moods", True)
            except Exception as e:
                return self._send(400, {"error": "bad request: %s" % e})
            if not audio_path:
                return self._send(400, {"error": "path required"})
            result, err = analyze_audio(audio_path, genres, moods)
            if err:
                return self._send(200, {"ok": False, "error": err})
            self._send(200, {"ok": True, **result})
            return
        self._send(404, {"error": "not found"})


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
                    data=json.dumps({"service": "essentia", "ts": time.time()}).encode(),
                    headers={"Content-Type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass
    threading.Thread(target=_loop, daemon=True).start()


if __name__ == "__main__":
    load_models()
    start_heartbeat()
    log.info("=" * 60)
    log.info("%s starting", SERVICE)
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("Essentia available: %s", ESSENTIA_AVAILABLE)
    log.info("Genre model: %s, Mood model: %s", GENRE_MODEL is not None, MOOD_MODEL is not None)
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
