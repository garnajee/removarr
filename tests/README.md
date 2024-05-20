# Tests

In order to tests if the docker works, I created a python script that:

- create a the files and folders structure
- generate the `.torrent` files for each files and folders
- create hardlinks for specific files and folders
- add the torrents to the docker transmission
- list the files and folder that can be deleted

## Run tests

Create the python virtual environnement:

```bash
python3 -m venv venv
source venv/bin/activate
pip install -r requirements.txt
```

Run the docker-compose file:

`docker compose up -d`

Run the python script:

`python3 run-tests.py`

Go to the Transmission web-ui [http://localhost:9091](http://localhost:9091) and manually add the torrents files (they are located in `data/complete/torrents/`).

Run the Removarr Flask web-ui:

- create a python virtual environnement
- install the requirements
- run `app.py`

## Expected results

Files shown in the web-ui:

- `file_to_delete.avi`: not hardlinked
- `folder_to_delete/`: the `.mkv` file is not hardlinked, and the `.nfo` is not an allowed extension for the comparison
- `big_folder_to_delete/`: only to try if the recursive comparison is working

Also, `random_file.txt` is not hardlinked, but `.txt` is not an allowed extension, so it won't be displayed in the web-ui.

You can now try to delete the files and folder, and they should be deleted in `data/complete/` and in `tr-config/torrents/`.

