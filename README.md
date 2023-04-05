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

