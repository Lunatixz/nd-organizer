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
GENRE_MODEL = None
MOOD_MODEL = None

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


def load_models():
    global ESSENTIA_AVAILABLE, LIBROSA_AVAILABLE, GENRE_MODEL, MOOD_MODEL, GENRE_LABELS
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
    genre_path = os.path.join(model_dir, "discogs_400_epCNN_discogs-hard_256.pb")
    if os.path.exists(genre_path):
        try:
            import essentia.standard as es
            GENRE_MODEL = es.TensorflowPredictCNN(model=genre_path)
            log.info("Genre model loaded from %s", genre_path)
        except Exception as e:
            log.warning("Genre model load failed: %s", e)
    mood_path = os.path.join(model_dir, "mtg_jamendo_mood_256.pb")
    if os.path.exists(mood_path):
        try:
            import essentia.standard as es
            MOOD_MODEL = es.TensorflowPredictCNN(model=mood_path)
            log.info("Mood model loaded from %s", mood_path)
        except Exception as e:
            log.warning("Mood model load failed: %s", e)


def load_audio(path, duration=120):
    if not os.path.exists(path):
        return None, "file not found"
    if ESSENTIA_AVAILABLE:
        try:
            import essentia.standard as es
            audio = es.MonoLoader(filename=path, sampleRate=44100, endTime=duration)()
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
    # VGGish pooled features: compute once, reuse for both genre and mood.
    pooled = None
    if (genres and GENRE_MODEL is not None) or (moods and MOOD_MODEL is not None):
        try:
            pooled = es.TensorflowPredictVGGish()(audio)
        except Exception as e:
            log.warning("VGGish pooling failed for %s: %s", path, e)
    if genres and GENRE_MODEL is not None and pooled is not None:
        try:
            preds = GENRE_MODEL(pooled)[0]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:10]
            for idx, score in top:
                if idx < len(GENRE_LABELS) and score > 0.05:
                    result["genres"].append({"name": GENRE_LABELS[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("genre prediction failed for %s: %s", path, e)
    if moods and MOOD_MODEL is not None and pooled is not None:
        try:
            preds = MOOD_MODEL(pooled)[0]
            # Full MTG-Jamendo mood taxonomy (56 classes).
            mood_labels = [
                "happy", "sad", "angry", "fear", "tender", "excited", "energetic",
                "dark", "boring", "calm", "cheerful", "romantic", "melancholic",
                "aggressive", "uplifting", "inspiring", "mysterious", "playful",
                "sentimental", "nostalgic", "epic", "dramatic", "peaceful",
                "dreamy", "triumphant", "haunting", "ethereal", "powerful",
                "gentle", "somber", "bittersweet", "euphoric", "anxious",
                "relaxing", "intense", "lively", "solemn", "whimsical",
                "reflective", "yearning", "brooding", "soothing", "stirring",
                "gritty", "luscious", "raw", "lush", "spacious",
                "crunchy", "shimmering", "warm", "cold", "bright",
                "dark_harsh", "smooth",
            ]
            top = sorted(enumerate(preds), key=lambda x: x[1], reverse=True)[:10]
            for idx, score in top:
                if idx < len(mood_labels) and score > 0.005:
                    result["moods"].append({"name": mood_labels[idx], "score": round(float(score), 4)})
        except Exception as e:
            log.warning("mood prediction failed for %s: %s", path, e)
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
                "genre_model": GENRE_MODEL is not None,
                "mood_model": MOOD_MODEL is not None,
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
    backend = "essentia" if ESSENTIA_AVAILABLE else ("librosa" if LIBROSA_AVAILABLE else "none")
    log.info("=" * 60)
    log.info("%s starting (backend: %s)", SERVICE, backend)
    log.info("listening on 0.0.0.0:%d", PORT)
    log.info("Essentia: %s, librosa: %s", ESSENTIA_AVAILABLE, LIBROSA_AVAILABLE)
    log.info("Genre model: %s, Mood model: %s", GENRE_MODEL is not None, MOOD_MODEL is not None)
    log.info("Features: structure, chords, fingerprint, compare, caching")
    log.info("=" * 60)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
