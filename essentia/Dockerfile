FROM python:3.11-slim

# Install system dependencies for audio processing
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswresample-dev \
    gcc g++ && \
    rm -rf /var/lib/apt/lists/*

COPY requirements.txt /app/requirements.txt

RUN pip install --no-cache-dir --break-system-packages -r /app/requirements.txt

COPY server.py /app/server.py
COPY VERSION /app/VERSION
WORKDIR /app
EXPOSE 8101
CMD ["python", "server.py", "8101"]
