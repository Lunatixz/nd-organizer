FROM python:3.12-slim
RUN pip install --no-cache-dir pymysql
COPY server.py /app/server.py
WORKDIR /app
EXPOSE 8098
CMD ["python", "server.py", "8098"]
