#!/usr/bin/env bash

replace_docker_compose () {
  echo "Replacing docker-compose.yml"
  cat << EOF >> removarr-master/docker-compose.yml
  services:
    removarr:
      image: ghcr.io/garnajee/removarr:latest
      build: .
      container_name: removarr
      restart: always
      volumes:
        - '/your/path/completed/:/data/completed'
        - '/your/path/medias/:/data/medias'
      ports:
        - '127.0.0.1:8012:5000'
  EOF
    echo "Done."
}

download_repo () {
  echo "Downloading files..."
  curl -LOk https://github.com/garnajee/removarr/archive/master.tar.gz
  echo "Extracting and removing files..."
  tar xvzf master.tar.gz && rm -f master.tar.gz && echo "Done."; tree
}

create_archi () {
  echo "Creating directory structure..."
  mkdir -p {completed,medias/{films/myfilm,series/myserie/season1}}
  echo "Creating files..."
  touch completed/{film-{old,new}.mkv,serie-{old,new}.mkv}
  echo "Hard linking files..."
  ln completedfilm-new.mkv medias/films/myfilm
  ln completed/serie-new.mkv medias/series/myserie/season1
  echo "Done."
  tree
}

create_archi
download_repo

echo "Modify docker compose with correct path."
echo "pwd: $(pwd)"
echo "run:"
echo "docker compose up -d --build"

