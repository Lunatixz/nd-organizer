FROM python:3.12-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends libchromaprint-tools ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY server.py /app/server.py
WORKDIR /app
EXPOSE 8097
CMD ["python", "server.py", "8097"]
