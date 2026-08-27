FROM python:3.11-slim

# Install system dependencies for audio processing
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswresample-dev \
    gcc g++ && \
    rm -rf /var/lib/apt/lists/*

COPY requirements.txt /app/requirements.txt

# Try installing essentia-tensorflow; if it fails, install essentia separately
# The server.py gracefully handles missing essentia (returns empty predictions)
RUN pip install --no-cache-dir --break-system-packages numpy && \
    (pip install --no-cache-dir --break-system-packages essentia-tensorflow 2>/dev/null || \
     pip install --no-cache-dir --break-system-packages essentia 2>/dev/null || \
     echo "WARN: essentia not installed, will return empty predictions")

COPY server.py /app/server.py
WORKDIR /app
EXPOSE 8101
CMD ["python", "server.py", "8101"]
