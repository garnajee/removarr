FROM python:alpine

ENV PYTHONUNBUFFERED=1

WORKDIR /app

COPY app.py
COPY templates/index.html .
COPY favicon.ico /

RUN pip install --no-cache-dir Flask

# Transmission and Jellyfin folders
VOLUME /data/completed /data/medias /data/movies /data/series
EXPOSE 5000
CMD ["python", "app.py"]
