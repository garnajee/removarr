FROM python:alpine
ENV PYTHONUNBUFFERED=1
WORKDIR /app
COPY . /app/
RUN pip install --no-cache-dir -r requirements.txt
# Transmission and Jellyfin folders
VOLUME /data/completed /data/medias
EXPOSE 5000
CMD ["python", "app.py"]
