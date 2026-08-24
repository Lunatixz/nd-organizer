FROM python:3.11-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    libessentia-dev ffmpeg libavcodec-dev && \
    rm -rf /var/lib/apt/lists/*
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt
COPY server.py /app/server.py
WORKDIR /app
EXPOSE 8101
CMD ["python", "server.py", "8101"]
