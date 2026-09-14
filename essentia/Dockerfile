FROM python:3.11-slim

# Install system dependencies for audio processing
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswresample-dev \
    gcc g++ && \
    rm -rf /var/lib/apt/lists/*

COPY requirements.txt /app/requirements.txt

# Install essentia-tensorflow (includes TensorFlow support for genre/mood models)
RUN pip install --no-cache-dir --break-system-packages -r /app/requirements.txt && \
    python -c "import tensorflow; print('TensorFlow', tensorflow.__version__)" && \
    python -c "import essentia.standard as es; print([x for x in dir(es) if 'Tensorflow' in x])" && \
    echo "TensorFlow models verified OK"

# Models are auto-downloaded on first startup (no pre-download needed)
# Genre: discogs_400_epCNN_discogs-hard_256.pb (~2MB)
# Embedding: discogs-effnet-bs64-1.pb (~18MB)
# Mood: mtg_jamendo_mood_256.pb (optional, user-configurable)

COPY server.py /app/server.py
COPY VERSION /app/VERSION
WORKDIR /app
EXPOSE 8101
CMD ["python", "server.py", "8101"]
