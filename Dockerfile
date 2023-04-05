FROM python:alpine
ENV PYTHONUNBUFFERED=1
WORKDIR /app
COPY requirements.txt /app/
RUN pip install --no-cache-dir -r requirements.txt
COPY . /app/
VOLUME /tmp/test/
EXPOSE 5000
CMD ["python", "app.py"]
