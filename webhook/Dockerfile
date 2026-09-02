FROM python:3.12-slim
WORKDIR /app
COPY server.py .
COPY VERSION .
EXPOSE 8099
CMD ["python", "server.py", "8099", "/data/webhook.log"]
