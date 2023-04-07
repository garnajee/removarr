# removarr

*build status:* ![build check](https://github.com/garnajee/removarr/actions/workflows/publish-image.yml/badge.svg)

This is a web application, created to help you manually delete files present in the Transmission `completed/` download folder.

A recursive comparison according to the inodes of the files is made between the `completed/` folder (on the Transmission side) and the folders where the medias are stored (on the Jellyfin side)

```
├── completed       # Transmission side
│   ├── folder1
│   ├── ...
│   ├── files
│   └── ...
└── medias          # Jellyfin side
    ├── movies
    │   ├── folders
    │   └── ...
    └── series
        ├── folders
        └── ...
```

All the files present on the side of Jellyfin are the latest versions that we want to keep. So, all we want to remove is everything that is present on the Transmission side but not on the Jellyfin side.

If the deleted file was in a folder : 

- if the folder is now empty, then the folder is deleted
- if the folder contains other files different than '.mkv', '.mp4', '.avi', '.mov', then the folder is deleted
- if the folder contains subfolder, the folder is *not* deleted

## Todo

- [ ] modify README (setup installation, how to use, screenshot,...)
- [ ] get hash of torrent file to delete it (need to mount one more volume)
- [x] delete empty folder (ignore other files differents than .mkv/avi/...)
- [ ] create a checkbox to delete multiples files at once
- [ ] create a buton to delete all files at once

# License

This project is under [MIT](LICENSE) License.

