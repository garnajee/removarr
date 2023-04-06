# removarr

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

## Todo

- [ ] get hash of torrent file to delete it (need to mount one more volume)
- [ ] create a checkbox to delete multiples files at once
- [ ] create a buton to delete all files at once

