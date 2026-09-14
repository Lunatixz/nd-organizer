FROM python:3.11-slim

# Install system dependencies for audio processing
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswresample-dev \
    gcc g++ && \
    rm -rf /var/lib/apt/lists/*

COPY requirements.txt /app/requirements.txt

# Install essentia-tensorflow (includes TensorFlow support for genre/mood models)
RUN pip install --no-cache-dir --break-system-packages -r /app/requirements.txt && \
    python -c "import essentia.standard as es; assert hasattr(es, 'TensorflowPredictCNN'), 'TensorflowPredictCNN not found'" && \
    echo "TensorFlow models verified OK"

COPY server.py /app/server.py
COPY VERSION /app/VERSION
WORKDIR /app
EXPOSE 8101
CMD ["python", "server.py", "8101"]
