# Essentia ML sidecar for nd-organizer.
#
# Provides genre/mood analysis, song structure, chord detection, and audio
# fingerprinting. Uses the Essentia library (Music Technology Group) with
# Discogs-400 (genres), MTG-Jamendo (moods), and built-in Essentia algorithms.
# Falls back to librosa when Essentia is unavailable.
#
# Endpoints:
#   GET  /health  -> {"ok": true, "service": "...", "essentia": bool}
#   POST /analyze {"path": "/music/song.flac", "genres": true, "moods": true,
#                  "structure": true, "chroma": true, "bpm": true}
#       -> {"ok": true, "genres": [...], "moods": [...], "structure": [...],
#          "chords": [...], "bpm": 120.0, "key": "C", "mode": "major"}
#   POST /fingerprint {"path": "/music/song.flac"}
#       -> {"ok": true, "fingerprint": [...], "duration": 240.5}
#   POST /compare {"path_a": "/music/a.flac", "path_b": "/music/b.flac"}
#       -> {"ok": true, "similarity": 0.85, "is_cover": true}
#   POST /instrumental-check {"path": "/music/song.flac"}
#       -> {"ok": true, "isInstrumental": true, "confidence": 0.85, "vocalRatio": 0.02}
#
# No internet required after model download. Models are loaded at startup.
# If Essentia is not installed, falls back to librosa for analysis.

import collections
import functools
import json
import logging
import os
import struct
import sys
import time
import urllib.request

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

logging.basicConfig(level=logging.INFO,
                    format="%(asctime)s %(levelname)s [essentia] %(message)s",
                    datefmt="%Y-%m-%d %H:%M:%S")
log = logging.getLogger("essentia")

LOG_BUFFER = collections.deque(maxlen=500)
ANALYSIS_CACHE = collections.OrderedDict()  # path -> (result, mtime, ts)
MAX_CACHE_SIZE = 256
MAX_POST_BODY = 10 * 1024 * 1024  # 10 MB


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
LIBROSA_AVAILABLE = False
MODELS_LOADED = False
GENRE_MODEL = None
MOOD_MODEL = None
VOICE_MODEL = None
DANCE_MODEL = None
GENDER_MODEL = None
DEAM_MODEL = None
APPROACH_MODEL = None
ENGAGE_MODEL = None
TIMBRE_MODEL = None

# Full Discogs-400 taxonomy (loaded from model at startup, fallback to top classes).
GENRE_LABELS = [
    "Rock", "Pop", "Hip-Hop", "Electronic", "Jazz", "Classical", "R&B", "Country",
    "Folk", "Blues", "Reggae", "Punk", "Metal", "Funk", "Soul", "Latin", "World",
    "Film", "Stage", "TV", "Audiobook", "Podcast",
]

# Chord labels for chroma-based chord detection.
CHORD_LABELS = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    "Cm", "C#m", "Dm", "D#m", "Em", "Fm", "F#m", "Gm", "G#m", "Am", "A#m", "Bm",
    "C7", "C#7", "D7", "D#7", "E7", "F7", "F#7", "G7", "G#7", "A7", "A#7", "B7",
]


def download_model(url, dest, max_retries=3, retry_delay=5, timeout=60, min_size=1024):
    """Download a model file if it doesn't exist, with retries and timeout."""
    if os.path.exists(dest) and os.path.getsize(dest) >= min_size:
        return True
    # Remove stale/partial file
    if os.path.exists(dest):
        try:
            os.remove(dest)
        except OSError:
            pass
    for attempt in range(max_retries):
        try:
            log.info("Downloading model (attempt %d/%d): %s", attempt + 1, max_retries, url)
            req = urllib.request.Request(url, headers={"User-Agent": "essentia-sidecar/1.0"})
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                data = resp.read()
            if len(data) < min_size:
                log.warning("Download too small (%d bytes < %d), retrying: %s", len(data), min_size, url)
                if attempt < max_retries - 1:
                    time.sleep(retry_delay)
                continue
            with open(dest, "wb") as f:
                f.write(data)
            log.info("Downloaded %s (%d bytes)", os.path.basename(dest), len(data))
            return True
        except Exception as e:
            log.warning("Download failed (attempt %d/%d): %s", attempt + 1, max_retries, e)
            # Clean up partial file
            if os.path.exists(dest):
                try:
                    os.remove(dest)
                except OSError:
                    pass
            if attempt < max_retries - 1:
                time.sleep(retry_delay)
    log.warning("Failed to download %s after %d attempts", url, max_retries)
    return False


def load_models():
    global ESSENTIA_AVAILABLE, LIBROSA_AVAILABLE, GENRE_MODEL, MOOD_MODEL, VOICE_MODEL, GENRE_LABELS
    global DANCE_MODEL, GENDER_MODEL, DEAM_MODEL, APPROACH_MODEL, ENGAGE_MODEL, TIMBRE_MODEL
    try:
        import essentia
        import essentia.standard as es
        log.info("Essentia loaded successfully (v%s)", essentia.__version__)
        ESSENTIA_AVAILABLE = True
    except ImportError:
        log.warning("Essentia not installed - trying librosa fallback")
    try:
        import librosa
        log.info("librosa loaded successfully (v%s)", librosa.__version__)
        LIBROSA_AVAILABLE = True
    except ImportError:
        if not ESSENTIA_AVAILABLE:
            log.warning("Neither Essentia nor librosa available - returning empty predictions")
            return
    model_dir = os.environ.get("MODEL_DIR", os.path.expanduser("~/essentia_models"))
    os.makedirs(model_dir, exist_ok=True)

    # Auto-download models if not present
    EMB_URL = "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bs64-1.pb"
    GENRE_URL = "https://essentia.upf.edu/models/classification-heads/genre_discogs400/genre_discogs400-discogs-effnet-1.pb"
    GENRE_JSON_URL = "https://essentia.upf.edu/models/classification-heads/genre_discogs400/genre_discogs400-discogs-effnet-1.json"

    embedding_path = os.path.join(model_dir, "discogs-effnet-bs64-1.pb")
    genre_path = os.path.join(model_dir, "discogs_400_epCNN_discogs-hard_256.pb")
    genre_json_path = os.path.join(model_dir, "genre_discogs400-discogs-effnet-1.json")

    # Download models (blocks startup until available)
    download_model(EMB_URL, embedding_path)
    download_model(GENRE_URL, genre_path)
    download_model(GENRE_JSON_URL, genre_json_path)

    # Load full 400-class label list from JSON metadata
    if os.path.exists(genre_json_path):
        try:
            with open(genre_json_path) as f:
                meta = json.load(f)
            GENRE_LABELS = meta.get("classes", GENRE_LABELS)
            log.info("Loaded %d genre labels from metadata", len(GENRE_LABELS))
        except Exception as e:
            log.warning("Failed to load genre labels: %s", e)

    # Genre model: needs embedding model (EffNetDiscogs) + classification head (TensorflowPredict2D)
    if os.path.exists(genre_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            GENRE_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=genre_path, input="serving_default_model_Placeholder", output="PartitionedCall:0"),
            }
            log.info("Genre model loaded (EffNetDiscogs + Discogs400 classifier)")
        except Exception as e:
            log.warning("Genre model load failed: %s", e)
    else:
        log.warning("Genre models not available - genre classification disabled")

    # Mood model: needs MusiCNN embedding + Moods MIREX classifier (5 mood clusters)
    MOOD_URL = "https://essentia.upf.edu/models/classification-heads/moods_mirex/moods_mirex-msd-musicnn-1.pb"
    MOOD_JSON_URL = "https://essentia.upf.edu/models/classification-heads/moods_mirex/moods_mirex-msd-musicnn-1.json"
    MUSICNN_URL = "https://essentia.upf.edu/models/feature-extractors/musicnn/msd-musicnn-1.pb"

    mood_path = os.path.join(model_dir, "moods_mirex-msd-musicnn-1.pb")
    mood_json_path = os.path.join(model_dir, "moods_mirex-msd-musicnn-1.json")
    musicnn_path = os.path.join(model_dir, "msd-musicnn-1.pb")

    download_model(MOOD_URL, mood_path)
    download_model(MOOD_JSON_URL, mood_json_path)
    download_model(MUSICNN_URL, musicnn_path)

    if os.path.exists(mood_path) and os.path.exists(musicnn_path):
        try:
            import essentia.standard as es
            MOOD_MODEL = {
                "embedding": es.TensorflowPredictMusiCNN(graphFilename=musicnn_path, output="model/Placeholder"),
                "classifier": es.TensorflowPredict2D(graphFilename=mood_path, input="serving_default_model_Placeholder", output="PartitionedCall:0"),
            }
            log.info("Mood model loaded (MusiCNN + Moods MIREX classifier)")
        except Exception as e:
            log.warning("Mood model load failed: %s", e)
    else:
        log.warning("Mood models not available - mood classification disabled")

    # Voice/instrumental model: needs MusiCNN embedding + voice classifier (instrumental/voice)
    VOICE_URL = "https://essentia.upf.edu/models/classification-heads/voice_instrumental/voice_instrumental-msd-musicnn-1.pb"
    voice_path = os.path.join(model_dir, "voice_instrumental-msd-musicnn-1.pb")
    download_model(VOICE_URL, voice_path)

    if os.path.exists(voice_path) and os.path.exists(musicnn_path):
        try:
            import essentia.standard as es
            VOICE_MODEL = {
                "embedding": es.TensorflowPredictMusiCNN(graphFilename=musicnn_path, output="model/Placeholder"),
                "classifier": es.TensorflowPredict2D(graphFilename=voice_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Voice model loaded (MusiCNN + voice/instrumental classifier)")
        except Exception as e:
            log.warning("Voice model load failed: %s", e)
    else:
        log.warning("Voice models not available - voice/instrumental classification disabled")

    # Danceability model: EffNetDiscogs + binary classifier (danceable/not_danceable)
    DANCE_URL = "https://essentia.upf.edu/models/classification-heads/danceability/danceability-discogs-effnet-1.pb"
    dance_path = os.path.join(model_dir, "danceability-discogs-effnet-1.pb")
    download_model(DANCE_URL, dance_path)
    if os.path.exists(dance_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            DANCE_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=dance_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Danceability model loaded")
        except Exception as e:
            log.warning("Danceability model load failed: %s", e)

    # Voice gender model: EffNetDiscogs + binary classifier (female/male)
    GENDER_URL = "https://essentia.upf.edu/models/classification-heads/gender/gender-discogs-effnet-1.pb"
    gender_path = os.path.join(model_dir, "gender-discogs-effnet-1.pb")
    download_model(GENDER_URL, gender_path)
    if os.path.exists(gender_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            GENDER_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=gender_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Gender model loaded")
        except Exception as e:
            log.warning("Gender model load failed: %s", e)

    # Arousal/valence model: MusiCNN + regression (valence, arousal 1-9)
    DEAM_URL = "https://essentia.upf.edu/models/classification-heads/deam/deam-msd-musicnn-2.pb"
    deam_path = os.path.join(model_dir, "deam-msd-musicnn-2.pb")
    download_model(DEAM_URL, deam_path)
    if os.path.exists(deam_path) and os.path.exists(musicnn_path):
        try:
            import essentia.standard as es
            DEAM_MODEL = {
                "embedding": es.TensorflowPredictMusiCNN(graphFilename=musicnn_path, output="model/Placeholder"),
                "classifier": es.TensorflowPredict2D(graphFilename=deam_path, input="model/Placeholder", output="model/Identity"),
            }
            log.info("Arousal/valence model loaded (DEAM)")
        except Exception as e:
            log.warning("Arousal/valence model load failed: %s", e)

    # Approachability model: EffNetDiscogs + binary classifier (not_approachable/approachable)
    APPROACH_URL = "https://essentia.upf.edu/models/classification-heads/approachability/approachability_2c-discogs-effnet-1.pb"
    approach_path = os.path.join(model_dir, "approachability_2c-discogs-effnet-1.pb")
    download_model(APPROACH_URL, approach_path)
    if os.path.exists(approach_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            APPROACH_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=approach_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Approachability model loaded")
        except Exception as e:
            log.warning("Approachability model load failed: %s", e)

    # Engagement model: EffNetDiscogs + binary classifier (not_engaging/engaging)
    ENGAGE_URL = "https://essentia.upf.edu/models/classification-heads/engagement/engagement_2c-discogs-effnet-1.pb"
    engage_path = os.path.join(model_dir, "engagement_2c-discogs-effnet-1.pb")
    download_model(ENGAGE_URL, engage_path)
    if os.path.exists(engage_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            ENGAGE_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=engage_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Engagement model loaded")
        except Exception as e:
            log.warning("Engagement model load failed: %s", e)

    # Timbre model: EffNetDiscogs + binary classifier (bright/dark)
    TIMBRE_URL = "https://essentia.upf.edu/models/classification-heads/timbre/timbre-discogs-effnet-1.pb"
    timbre_path = os.path.join(model_dir, "timbre-discogs-effnet-1.pb")
    download_model(TIMBRE_URL, timbre_path)
    if os.path.exists(timbre_path) and os.path.exists(embedding_path):
        try:
            import essentia.standard as es
            TIMBRE_MODEL = {
                "embedding": es.TensorflowPredictEffnetDiscogs(graphFilename=embedding_path, output="PartitionedCall:1"),
                "classifier": es.TensorflowPredict2D(graphFilename=timbre_path, input="model/Placeholder", output="model/Softmax"),
            }
            log.info("Timbre model loaded")
        except Exception as e:
            log.warning("Timbre model load failed: %s", e)

    MODELS_LOADED = True
    loaded = sum(1 for m in [GENRE_MODEL, MOOD_MODEL, VOICE_MODEL, DANCE_MODEL,
                             GENDER_MODEL, DEAM_MODEL, APPROACH_MODEL, ENGAGE_MODEL, TIMBRE_MODEL] if m is not None)
    log.info("Model loading complete: %d/9 models loaded", loaded)


def load_audio(path, duration=120):
    if not os.path.exists(path):
        return None, "file not found"
    if ESSENTIA_AVAILABLE:
        try:
            import essentia.standard as es
            # MonoLoader: endTime removed — not supported in all Essentia versions.
            # Load full file; duration parameter is only used for librosa fallback.
            audio = es.MonoLoader(filename=path, sampleRate=44100)()
            return audio, None
        except Exception as e:
            log.error("Essentia audio load failed for %s: %s", path, e)
            return None, str(e)
    elif LIBROSA_AVAILABLE:
        try:
            import librosa
            audio, _ = librosa.load(path, sr=44100, duration=duration, mono=True)
            return audio, None
        except Exception as e:
            log.error("librosa audio load failed for %s: %s", path, e)
            return None, str(e)
    return None, "no audio backend available"


def _get_cache(path):
    """Return cached analysis result if valid (same mtime, < 1h old)."""
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        return None
    if path in ANALYSIS_CACHE:
        result, cached_mtime, ts = ANALYSIS_CACHE[path]
        if cached_mtime == mtime and (time.time() - ts) < 3600:
            ANALYSIS_CACHE.move_to_end(path)
            return result
        del ANALYSIS_CACHE[path]
    return None


def _set_cache(path, result):
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        return
    ANALYSIS_CACHE[path] = (result, mtime, time.time())
    while len(ANALYSIS_CACHE) > MAX_CACHE_SIZE:
        ANALYSIS_CACHE.popitem(last=False)


def _validate_path(path):
    """Check path exists and is a regular file."""
    if not path or not isinstance(path, str):
        return False
    try:
        return os.path.isfile(path)
    except (OSError, ValueError):
        return False


def analyze_audio(path, genres=True, moods=True, structure=False, chroma=False, bpm=False):
    result = {"genres": [], "moods": [], "energy": None}
    if not _validate_path(path):
        return result, "file not found"
    cached = _get_cache(path)
    if cached is not None:
        return cached, None
    audio, err = load_audio(path)
    if err:
        return result, err
    t0 = time.time()
    if ESSENTIA_AVAILABLE:
        result, err = _analyze_essentia(audio, path, genres, moods, structure, chroma, bpm)
    elif LIBROSA_AVAILABLE:
        result, err = _analyze_librosa(audio, path, genres, moods, structure, chroma, bpm)
    else:
        return result, "no analysis backend"
    if err:
        return result, err
    _set_cache(path, result)
    dt = time.time() - t0
    log.info("analyzed %s (%.1fs): %d genres, %d moods, structure=%s, chords=%s",
             os.path.basename(path), dt,
             len(result.get("genres", [])), len(result.get("moods", [])),
             "yes" if structure else "no",
             "yes" if chroma else "no")
    return result, None


def _analyze_essentia(audio, path, genres, moods, structure, chroma, bpm):
    result = {"genres": [], "moods": [], "energy": None}
    try:
        import essentia.standard as es
        import numpy as np
    except ImportError:
        return result, "Essentia import failed"

    # Pre-compute embeddings once for all classifiers.
    # EffNetDiscogs: genre, danceability, gender, approachability, engagement, timbre
    # MusiCNN: mood, voice, arousal/valence
    audio_16k = es.Resample(inputSampleRate=44100, outputSampleRate=16000)(audio)
    effnet_emb = None
    musicnn_emb = None

    # Genre prediction: EffNetDiscogs → classifier
    if genres and GENRE_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = GENRE_MODEL["classifier"](effnet_emb)[0]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:10]
            for idx, score in top:
                if idx < len(GENRE_LABELS) and score > 0.05:
                    result["genres"].append({"name": GENRE_LABELS[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("genre prediction failed for %s: %s", path, e)

    # Mood prediction: MusiCNN → classifier (5 mood clusters)
    if moods and MOOD_MODEL is not None:
        try:
            if musicnn_emb is None:
                musicnn_emb = MOOD_MODEL["embedding"](audio_16k)
            preds = MOOD_MODEL["classifier"](musicnn_emb)[0]
            mood_labels = [
                "passionate, rousing, confident, boisterous, rowdy",
                "rollicking, cheerful, fun, sweet, amiable/good natured",
                "literate, poignant, wistful, bittersweet, autumnal, brooding",
                "humorous, silly, campy, quirky, whimsical, witty, wry",
                "aggressive, fiery, tense/anxious, intense, volatile, visceral",
            ]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:5]
            for idx, score in top:
                if idx < len(mood_labels) and score > 0.05:
                    result["moods"].append({"name": mood_labels[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("mood prediction failed for %s: %s", path, e)

    # Voice/instrumental: MusiCNN → classifier
    if VOICE_MODEL is not None:
        try:
            if musicnn_emb is None:
                musicnn_emb = MOOD_MODEL["embedding"](audio_16k)
            preds = VOICE_MODEL["classifier"](musicnn_emb)[0]
            voice_labels = ["instrumental", "voice"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(voice_labels) and score > 0.05:
                    result["voice"] = voice_labels[idx]
                    result["voice_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("voice prediction failed for %s: %s", path, e)

    # Danceability: EffNetDiscogs → classifier
    if DANCE_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = DANCE_MODEL["classifier"](effnet_emb)[0]
            dance_labels = ["danceable", "not_danceable"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(dance_labels) and score > 0.05:
                    result["danceability"] = dance_labels[idx]
                    result["danceability_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("danceability prediction failed for %s: %s", path, e)

    # Voice gender: EffNetDiscogs → classifier
    if GENDER_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = GENDER_MODEL["classifier"](effnet_emb)[0]
            gender_labels = ["female", "male"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(gender_labels) and score > 0.05:
                    result["gender"] = gender_labels[idx]
                    result["gender_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("gender prediction failed for %s: %s", path, e)

    # Arousal/valence: MusiCNN → regression (valence, arousal 1-9)
    if DEAM_MODEL is not None:
        try:
            if musicnn_emb is None:
                musicnn_emb = MOOD_MODEL["embedding"](audio_16k)
            preds = DEAM_MODEL["classifier"](musicnn_emb)[0]
            result["valence"] = round(float(preds[0]), 2)
            result["arousal"] = round(float(preds[1]), 2)
        except Exception as e:
            log.warning("arousal/valence prediction failed for %s: %s", path, e)

    # Approachability: EffNetDiscogs → classifier
    if APPROACH_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = APPROACH_MODEL["classifier"](effnet_emb)[0]
            approach_labels = ["not_approachable", "approachable"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(approach_labels) and score > 0.05:
                    result["approachability"] = approach_labels[idx]
                    result["approachability_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("approachability prediction failed for %s: %s", path, e)

    # Engagement: EffNetDiscogs → classifier
    if ENGAGE_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = ENGAGE_MODEL["classifier"](effnet_emb)[0]
            engage_labels = ["not_engaging", "engaging"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(engage_labels) and score > 0.05:
                    result["engagement"] = engage_labels[idx]
                    result["engagement_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("engagement prediction failed for %s: %s", path, e)

    # Timbre: EffNetDiscogs → classifier (bright/dark)
    if TIMBRE_MODEL is not None:
        try:
            if effnet_emb is None:
                effnet_emb = GENRE_MODEL["embedding"](audio_16k)
            preds = TIMBRE_MODEL["classifier"](effnet_emb)[0]
            timbre_labels = ["bright", "dark"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            for idx, score in top:
                if idx < len(timbre_labels) and score > 0.05:
                    result["timbre"] = timbre_labels[idx]
                    result["timbre_score"] = round(float(score), 4)
        except Exception as e:
            log.warning("timbre prediction failed for %s: %s", path, e)

    if bpm:
        try:
            rhythm_extractor = es.RhythmExtractor2013(method="multifeature")
            bpm_val, beats, beats_confidence, bpm_intervals = rhythm_extractor(audio)
            result["bpm"] = round(float(bpm_val), 1)
            result["bpm_confidence"] = round(float(beats_confidence), 4)
            result["beat_count"] = len(beats)
            key_extractor = es.KeyExtractor()
            key, scale, key_strength = key_extractor(audio)
            result["key"] = str(key)
            result["mode"] = str(scale)
            result["key_confidence"] = round(float(key_strength), 4)
        except Exception as e:
            log.warning("bpm/key detection failed for %s: %s", path, e)
    if structure:
        try:
            result["structure"] = _detect_structure_essentia(audio, es)
        except Exception as e:
            log.warning("structure detection failed for %s: %s", path, e)
    if chroma:
        try:
            result["chords"] = _detect_chords_essentia(audio, es)
        except Exception as e:
            log.warning("chord detection failed for %s: %s", path, e)
    return result, None


def _analyze_librosa(audio, path, genres, moods, structure, chroma, bpm):
    result = {"genres": [], "moods": [], "energy": None}
    try:
        import librosa
        import numpy as np
    except ImportError:
        return result, "librosa import failed"
    if bpm:
        try:
            tempo, _ = librosa.beat.beat_track(y=audio, sr=44100)
            result["bpm"] = round(float(np.atleast_1d(tempo)[0]), 1)
            chroma_lib = librosa.feature.chroma_cqt(y=audio, sr=44100)
            if chroma_lib.shape[1] > 0:
                pitch_classes = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
                avg_chroma = np.mean(chroma_lib, axis=1)
                dominant_idx = int(np.argmax(avg_chroma))
                result["key"] = pitch_classes[dominant_idx % 12]
                result["mode"] = "major"
        except Exception as e:
            log.warning("librosa bpm/key failed for %s: %s", path, e)
    if structure:
        try:
            result["structure"] = _detect_structure_librosa(audio)
        except Exception as e:
            log.warning("librosa structure failed for %s: %s", path, e)
    if chroma:
        try:
            result["chords"] = _detect_chords_librosa(audio)
        except Exception as e:
            log.warning("librosa chords failed for %s: %s", path, e)
    return result, None


def _detect_structure_essentia(audio, es):
    window = es.Windowing(type="blackman-harris", normalize=False)
    spec = es.Spectrum()
    mfcc_comp = es.MFCC()
    frame_size = 2048
    hop_size = 512
    features = []
    for i, frame in enumerate(es.Frame(audio, frameSize=frame_size, hopSize=hop_size)):
        w = window(frame)
        s = spec(w)
        m = mfcc_comp(s)
        features.append(list(m))
    if len(features) < 10:
        return []
    import numpy as np
    feat_matrix = np.array(features)
    feat_matrix = (feat_matrix - feat_matrix.mean(axis=0)) / (feat_matrix.std(axis=0) + 1e-8)
    n_frames = len(feat_matrix)
    sim_matrix = np.dot(feat_matrix, feat_matrix.T) / feat_matrix.shape[1]
    segment_size = max(1, n_frames // 20)
    boundaries = [0]
    prev_score = 0
    for i in range(segment_size, n_frames - segment_size, segment_size):
        left = sim_matrix[i - segment_size:i, i - segment_size:i].mean()
        right = sim_matrix[i:i + segment_size, i:i + segment_size].mean()
        cross = sim_matrix[i - segment_size:i, i:i + segment_size].mean()
        score = cross - (left + right) / 2
        if abs(score - prev_score) > 0.1:
            boundaries.append(i)
        prev_score = score
    boundaries.append(n_frames - 1)
    sections = []
    for idx in range(len(boundaries) - 1):
        start_frame = boundaries[idx]
        end_frame = boundaries[idx + 1]
        start_time = round(start_frame * hop_size / 44100.0, 2)
        end_time = round(end_frame * hop_size / 44100.0, 2)
        if idx == 0:
            label = "intro"
        elif idx == len(boundaries) - 2:
            label = "outro"
        else:
            label = "section_%s" % chr(ord('A') + (idx - 1) % 25)
        sections.append({
            "label": label,
            "start": start_time,
            "end": end_time,
            "duration": round(end_time - start_time, 2),
        })
    return sections


def _detect_structure_librosa(audio):
    import librosa
    import numpy as np
    hop_length = 512
    mfcc = librosa.feature.mfcc(y=audio, sr=44100, hop_length=hop_length)
    n_frames = mfcc.shape[1]
    if n_frames < 10:
        return []
    try:
        bound_frames = librosa.segment.agglomerative(mfcc, k=None)
    except Exception:
        return []
    sections = []
    pitch_classes = ["A", "B", "C", "D", "E", "F", "G"]
    for idx in range(len(bound_frames) - 1):
        start_time = round(float(bound_frames[idx]) * hop_length / 44100.0, 2)
        end_time = round(float(bound_frames[idx + 1]) * hop_length / 44100.0, 2)
        if idx == 0:
            label = "intro"
        elif idx == len(bound_frames) - 2:
            label = "outro"
        else:
            label = "section_%s" % pitch_classes[(idx - 1) % len(pitch_classes)]
        sections.append({
            "label": label,
            "start": start_time,
            "end": end_time,
            "duration": round(end_time - start_time, 2),
        })
    return sections


def _detect_chords_essentia(audio, es):
    """Detect chords using frame-wise chroma features and template matching.
    Computes ChromaCQT over the full audio (frame-wise), then templates
    each chord against the median chroma profile for better accuracy."""
    import numpy as np
    hop_size = 512
    frame_size = 2048
    window = es.Windowing(type="blackman-harris", normalize=False)
    spec = es.Spectrum()
    chroma_comp = es.ChromaCQT()
    chroma_frames = []
    for frame in es.Frame(audio, frameSize=frame_size, hopSize=hop_size):
        w = window(frame)
        s = spec(w)
        c = chroma_comp(s)
        chroma_frames.append(list(c))
    if not chroma_frames:
        return {"key": "", "chord": "", "changes": []}
    chroma_matrix = np.array(chroma_frames)
    pitch_classes = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
    # Chord templates: major, minor, dominant 7
    major_template = np.zeros(12)
    minor_template = np.zeros(12)
    seventh_template = np.zeros(12)
    # Build templates for each root
    chord_templates = {}
    for i, root in enumerate(pitch_classes):
        major_t = np.zeros(12)
        major_t[i] = 1.0
        major_t[(i + 4) % 12] = 0.8
        major_t[(i + 7) % 12] = 0.6
        minor_t = np.zeros(12)
        minor_t[i] = 1.0
        minor_t[(i + 3) % 12] = 0.8
        minor_t[(i + 7) % 12] = 0.6
        seventh_t = np.zeros(12)
        seventh_t[i] = 1.0
        seventh_t[(i + 4) % 12] = 0.8
        seventh_t[(i + 7) % 12] = 0.6
        seventh_t[(i + 10) % 12] = 0.4
        chord_templates[root] = major_t
        chord_templates[root + "m"] = minor_t
        chord_templates[root + "7"] = seventh_t
    # Classify each frame using cosine similarity against templates
    def classify_frame(frame_chroma):
        profile = frame_chroma / (np.linalg.norm(frame_chroma) + 1e-8)
        best_chord = ""
        best_score = -1
        for name, template in chord_templates.items():
            t_norm = template / (np.linalg.norm(template) + 1e-8)
            score = float(np.dot(profile, t_norm))
            if score > best_score:
                best_score = score
                best_chord = name
        return best_chord if best_score > 0.3 else ""
    chords_over_time = []
    current_chord = None
    current_start = 0
    step = max(1, len(chroma_frames) // 50)
    for i in range(0, len(chroma_frames), step):
        frame_chord = classify_frame(chroma_matrix[i])
        if not frame_chord:
            continue
        time_sec = round(i * hop_size / 44100.0, 2)
        if frame_chord != current_chord:
            if current_chord is not None:
                chords_over_time.append({
                    "chord": current_chord,
                    "start": current_start,
                    "end": time_sec,
                })
            current_chord = frame_chord
            current_start = time_sec
    if current_chord is not None:
        chords_over_time.append({
            "chord": current_chord,
            "start": current_start,
            "end": round(len(chroma_frames) * hop_size / 44100.0, 2),
        })
    # Overall dominant chord from median profile
    median_profile = np.median(chroma_matrix, axis=0)
    dominant_idx = int(np.argmax(median_profile))
    dominant_pitch = pitch_classes[dominant_idx % 12]
    overall = classify_frame(median_profile)
    return {
        "key": dominant_pitch,
        "chord": overall if overall else dominant_pitch,
        "changes": chords_over_time[:30],
    }


def _detect_chords_librosa(audio):
    import librosa
    import numpy as np
    hop_length = 512
    chroma = librosa.feature.chroma_cqt(y=audio, sr=44100, hop_length=hop_length)
    pitch_classes = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
    major_t = np.zeros(12)
    minor_t = np.zeros(12)
    seventh_t = np.zeros(12)
    chord_templates = {}
    for i, root in enumerate(pitch_classes):
        major_t = np.zeros(12)
        major_t[i] = 1.0; major_t[(i + 4) % 12] = 0.8; major_t[(i + 7) % 12] = 0.6
        minor_t = np.zeros(12)
        minor_t[i] = 1.0; minor_t[(i + 3) % 12] = 0.8; minor_t[(i + 7) % 12] = 0.6
        seventh_t = np.zeros(12)
        seventh_t[i] = 1.0; seventh_t[(i + 4) % 12] = 0.8
        seventh_t[(i + 7) % 12] = 0.6; seventh_t[(i + 10) % 12] = 0.4
        chord_templates[root] = major_t
        chord_templates[root + "m"] = minor_t
        chord_templates[root + "7"] = seventh_t
    def classify_frame(frame_chroma):
        profile = frame_chroma / (np.linalg.norm(frame_chroma) + 1e-8)
        best_chord, best_score = "", -1
        for name, template in chord_templates.items():
            t_norm = template / (np.linalg.norm(template) + 1e-8)
            score = float(np.dot(profile, t_norm))
            if score > best_score:
                best_score, best_chord = score, name
        return best_chord if best_score > 0.3 else ""
    n_frames = chroma.shape[1]
    if n_frames == 0:
        return {"key": "", "chord": "", "changes": []}
    median = np.median(chroma, axis=1)
    dominant_idx = int(np.argmax(median))
    dominant_pitch = pitch_classes[dominant_idx % 12]
    overall = classify_frame(median)
    chords_over_time = []
    current_chord = None
    current_start = 0
    step = max(1, n_frames // 50)
    for i in range(0, n_frames, step):
        fc = classify_frame(chroma[:, i])
        if not fc:
            continue
        t = round(i * hop_length / 44100.0, 2)
        if fc != current_chord:
            if current_chord is not None:
                chords_over_time.append({"chord": current_chord, "start": current_start, "end": t})
            current_chord = fc
            current_start = t
    if current_chord is not None:
        chords_over_time.append({"chord": current_chord, "start": current_start, "end": round(n_frames * hop_length / 44100.0, 2)})
    return {
        "key": dominant_pitch,
        "chord": overall if overall else dominant_pitch,
        "changes": chords_over_time[:30],
    }


def compute_fingerprint(path):
    """Compute a time-series spectral fingerprint for duplicate/cover detection.
    Analyzes multiple frames across the first 30s for robust matching."""
    if not _validate_path(path):
        return None, "file not found"
    audio, err = load_audio(path, duration=30)
    if err:
        return None, err
    if ESSENTIA_AVAILABLE:
        return _fingerprint_essentia(audio)
    elif LIBROSA_AVAILABLE:
        return _fingerprint_librosa(audio)
    return None, "no fingerprint backend"


def _fingerprint_essentia(audio):
    try:
        import essentia.standard as es
        import numpy as np
    except ImportError:
        return None, "Essentia/NumPy not installed"
    hop_size = 512
    frame_size = 2048
    window = es.Windowing(type="blackman-harris", normalize=False)
    spec = es.Spectrum()
    peaks_comp = es.SpectralPeaks(maxPeaks=100, sampleRate=44100)
    peak_sets = []
    for frame in es.Frame(audio, frameSize=frame_size, hopSize=hop_size):
        w = window(frame)
        s = spec(w)
        freqs, mags = peaks_comp(s)
        if len(freqs) > 0:
            top_idx = np.argsort(mags)[-10:]
            for idx in sorted(top_idx):
                q = int(round(float(freqs[idx]) / 10.0) * 10)
                peak_sets.append(q)
        if len(peak_sets) >= 200:
            break
    if not peak_sets:
        return None, "no peaks found"
    fingerprint = sorted(set(peak_sets))
    duration = len(audio) / 44100.0
    return {"fingerprint": fingerprint[:60], "duration": round(duration, 2), "peak_count": len(fingerprint)}, None


def _fingerprint_librosa(audio):
    try:
        import librosa
        import numpy as np
    except ImportError:
        return None, "librosa/NumPy not installed"
    hop_length = 512
    S = np.abs(librosa.stft(audio, hop_length=hop_length, n_fft=2048))
    freqs = librosa.fft_frequencies(sr=44100, n_fft=2048)
    peak_values = []
    for t in range(min(S.shape[1], 25)):
        frame = S[:, t]
        top_idx = np.argsort(frame)[-10:]
        for idx in sorted(top_idx):
            q = int(round(float(freqs[idx]) / 10.0) * 10)
            peak_values.append(q)
    if not peak_values:
        return None, "no peaks found"
    fingerprint = sorted(set(peak_values))
    duration = len(audio) / 44100.0
    return {"fingerprint": fingerprint[:60], "duration": round(duration, 2), "peak_count": len(fingerprint)}, None


def compare_fingerprints(fp_a, fp_b):
    if not fp_a or not fp_b:
        return 0.0
    a = set(fp_a) if isinstance(fp_a, list) else set()
    b = set(fp_b) if isinstance(fp_b, list) else set()
    if not a or not b:
        return 0.0
    intersection = len(a & b)
    union = len(a | b)
    if union == 0:
        return 0.0
    return round(intersection / union, 4)


def check_instrumental(path):
    """Check if a track is truly instrumental using voice/instrumental classifier + librosa vocal separation.
    Returns: {isInstrumental: bool, confidence: float, vocalRatio: float, voicePrediction: str}
    """
    audio, err = load_audio(path)
    if err:
        return None, err

    # 1. Voice/instrumental classifier (primary method - most accurate)
    voice_prediction = None
    voice_score = 0.0
    if ESSENTIA_AVAILABLE and VOICE_MODEL is not None:
        try:
            import essentia.standard as es
            audio_16k = es.Resample(inputSampleRate=44100, outputSampleRate=16000)(audio)
            embeddings = VOICE_MODEL["embedding"](audio_16k)
            preds = VOICE_MODEL["classifier"](embeddings)[0]
            voice_labels = ["instrumental", "voice"]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:2]
            if top:
                voice_prediction = voice_labels[top[0][0]]
                voice_score = float(top[0][1])
        except Exception as e:
            log.warning("voice check failed for %s: %s", path, e)

    # 2. Librosa vocal separation — calculate vocal energy ratio
    vocal_ratio = 0.0
    if LIBROSA_AVAILABLE:
        try:
            import librosa
            import numpy as np
            y, sr = librosa.load(path, duration=30)
            harmonic, percussive = librosa.effects.hpss(y)
            vocal_energy = float(np.sum(percussive**2))
            total_energy = float(np.sum(y**2))
            if total_energy > 0:
                vocal_ratio = vocal_energy / total_energy
        except Exception as e:
            log.warning("librosa vocal check failed for %s: %s", path, e)

    # 3. Combined decision
    if voice_prediction is not None:
        # Use voice/instrumental classifier (most accurate)
        is_instrumental = voice_prediction == "instrumental"
        confidence = voice_score if is_instrumental else 1.0 - voice_score
    else:
        # Fallback to librosa vocal ratio
        is_instrumental = vocal_ratio < 0.1
        confidence = 1.0 - vocal_ratio

    return {
        "isInstrumental": is_instrumental,
        "confidence": round(confidence, 3),
        "vocalRatio": round(vocal_ratio, 4),
        "voicePrediction": voice_prediction,
        "voiceScore": round(voice_score, 4) if voice_prediction else None,
    }, None


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

    def _read_body(self):
        n = int(self.headers.get("Content-Length", 0))
        if n > MAX_POST_BODY:
            return None, "request too large"
        raw = self.rfile.read(n) if n > 0 else b"{}"
        return json.loads(raw or "{}"), None

    def do_GET(self):
        path = self.path.rstrip("/")
        if path == "/health":
            ver = ""
            try:
                ver = open(os.path.join(os.path.dirname(__file__), "VERSION")).read().strip()
            except Exception:
                pass
            self._send(200, {
                "ok": True, "service": SERVICE, "port": PORT,
                "version": ver,
                "essentia": ESSENTIA_AVAILABLE,
                "librosa": LIBROSA_AVAILABLE,
                "models_loaded": MODELS_LOADED,
                "genre_model": GENRE_MODEL is not None,
                "mood_model": MOOD_MODEL is not None,
                "voice_model": VOICE_MODEL is not None,
                "dance_model": DANCE_MODEL is not None,
                "gender_model": GENDER_MODEL is not None,
                "deam_model": DEAM_MODEL is not None,
                "approach_model": APPROACH_MODEL is not None,
                "engage_model": ENGAGE_MODEL is not None,
                "timbre_model": TIMBRE_MODEL is not None,
                "uptime": int(time.time() - STARTED),
            })
            return
        if path == "/status":
            self._send(200, {
                "service": SERVICE,
                "ok": ESSENTIA_AVAILABLE or LIBROSA_AVAILABLE,
                "uptime": int(time.time() - STARTED),
                "stats": {
                    "essentia_loaded": ESSENTIA_AVAILABLE,
                    "librosa_loaded": LIBROSA_AVAILABLE,
                    "genre_model": GENRE_MODEL is not None,
                    "mood_model": MOOD_MODEL is not None,
                    "genre_labels": len(GENRE_LABELS),
                    "chord_labels": len(CHORD_LABELS),
                    "cache_size": len(ANALYSIS_CACHE),
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
            req, err = self._read_body()
            if err:
                return self._send(400, {"error": err})
            audio_path = req.get("path", "")
            if not audio_path:
                return self._send(400, {"error": "path required"})
            if not _validate_path(audio_path):
                return self._send(200, {"ok": False, "error": "file not found"})
            result, err = analyze_audio(
                audio_path,
                req.get("genres", True),
                req.get("moods", True),
                req.get("structure", False),
                req.get("chroma", False),
                req.get("bpm", False),
            )
            if err:
                return self._send(200, {"ok": False, "error": err})
            self._send(200, {"ok": True, **result})
            return
        if path == "/fingerprint":
            req, err = self._read_body()
            if err:
                return self._send(400, {"error": err})
            audio_path = req.get("path", "")
            if not audio_path:
                return self._send(400, {"error": "path required"})
            result, err = compute_fingerprint(audio_path)
            if err:
                return self._send(200, {"ok": False, "error": err})
            self._send(200, {"ok": True, **result})
            return
        if path == "/compare":
            req, err = self._read_body()
            if err:
                return self._send(400, {"error": err})
            path_a = req.get("path_a", "")
            path_b = req.get("path_b", "")
            if not path_a or not path_b:
                return self._send(400, {"error": "path_a and path_b required"})
            fp_a, err = compute_fingerprint(path_a)
            if err:
                return self._send(200, {"ok": False, "error": f"path_a: {err}"})
            fp_b, err = compute_fingerprint(path_b)
            if err:
                return self._send(200, {"ok": False, "error": f"path_b: {err}"})
            similarity = compare_fingerprints(
                fp_a.get("fingerprint") if fp_a else None,
                fp_b.get("fingerprint") if fp_b else None,
            )
            self._send(200, {
                "ok": True,
                "similarity": similarity,
                "is_cover": 0.5 <= similarity < 0.95,
                "is_duplicate": similarity >= 0.95,
                "duration_a": fp_a.get("duration") if fp_a else None,
                "duration_b": fp_b.get("duration") if fp_b else None,
            })
            return
        if path == "/instrumental-check":
            req, err = self._read_body()
            if err:
                return self._send(400, {"error": err})
            audio_path = req.get("path", "")
            if not audio_path:
                return self._send(400, {"error": "path required"})
            result, err = check_instrumental(audio_path)
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
    import threading
    # Start model downloads in background so /health responds immediately
    threading.Thread(target=load_models, daemon=True).start()
    start_heartbeat()
    log.info("=" * 60)
    log.info("%s starting", SERVICE)
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("Models loading in background...")
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
