#!/usr/bin/env python3

import os
import shutil   # to create hardlinks
from torf import Torrent

def create_file_structure(base_dir):
    """
    Creates a file and folder structure in the given directory.

    Args:
        base_dir (str): The path of the root folder

    Returns:
        None
    """

    dirs_to_create = [
        os.path.join(base_dir, "big_folder_also_stays", "other_folder_stays"),
        os.path.join(base_dir, "big_folder_stays", "other_folder_stays"),
        os.path.join(base_dir, "big_folder_to_delete", "other_folder_to_delete"),
        os.path.join(base_dir, "folder_stays"),
        os.path.join(base_dir, "folder_to_delete")
    ]

    files_to_create = [
        os.path.join(base_dir, "file_stays.mkv"),
        os.path.join(base_dir, "file_to_delete.avi"),
        os.path.join(base_dir, "random_file.txt"),

        os.path.join(base_dir, "big_folder_also_stays", "stays.mkv"),
        os.path.join(base_dir, "big_folder_also_stays", "stays.nfo"),
        os.path.join(base_dir, "big_folder_also_stays", "random_file.mp4"),
        os.path.join(base_dir, "big_folder_also_stays", "other_folder_stays", "also_stays.mkv"),
        os.path.join(base_dir, "big_folder_also_stays", "other_folder_stays", "also_stays.nfo"),

        os.path.join(base_dir, "big_folder_stays","stays.mkv"),
        os.path.join(base_dir, "big_folder_stays","stays.nfo"),
        os.path.join(base_dir, "big_folder_stays","other_folder_stays","also_stays.mkv"),
        os.path.join(base_dir, "big_folder_stays","other_folder_stays","also_stays.nfo"),

        os.path.join(base_dir, "big_folder_to_delete","to_delete.mkv"),
        os.path.join(base_dir, "big_folder_to_delete","to_delete.nfo"),
        os.path.join(base_dir, "big_folder_to_delete","other_folder_to_delete","also_to_delete.mkv"),
        os.path.join(base_dir, "big_folder_to_delete","other_folder_to_delete","also_to_delete.nfo"),

        os.path.join(base_dir, "folder_stays","this_file_stays.mkv"),
        os.path.join(base_dir, "folder_stays","this_file_stays.nfo"),

        os.path.join(base_dir, "folder_to_delete","to_delete.mp4"),
        os.path.join(base_dir, "folder_to_delete","to_delete.nfo")
    ]

    # Checks if directories exist, if not creates them
    for directory in dirs_to_create:
        if not os.path.exists(directory):
            os.makedirs(directory)

    # Checks whether the files exist, if not creates them and writes "random data" in them
    for file_path in files_to_create:
        if not os.path.exists(file_path):
            with open(file_path, "w") as f:
                f.write("random data")

def generate_torrents(root_dir, tracker="http://random.tracker"):
    """
    Generates a torrent file for each file and folder at the root directory using Torf.

    Args:
        root_dir (str): The path to the root directory containing files and folders.
        tracker (str): The URL of the tracker to use. Default is "http://random.tracker".

    Returns:
        None
    """
    
    # Creates the "torrents" folder in the root if it doesn't exist
    torrents_dir = os.path.join(root_dir, "torrents")
    if not os.path.exists(torrents_dir):
        os.makedirs(torrents_dir)

    for item in os.listdir(root_dir):
        item_path = os.path.join(root_dir, item)
        if os.path.isfile(item_path):
            # create torrent for file
            torrent = Torrent(path=item_path, trackers=[tracker])
            torrent.private = True
            torrent.generate()
            torrent_file_path = os.path.join(torrents_dir, item + ".torrent")
            torrent.write(torrent_file_path)
        elif os.path.isdir(item_path):
            # create torrent for folder
            torrent = Torrent(path=item_path, trackers=[tracker])
            torrent.private = True
            torrent.generate()
            torrent_file_path = os.path.join(torrents_dir, item + ".torrent")
            torrent.write(torrent_file_path)

def create_hardlinks(base_directory, media_directory):
    """
    Creates hardlinks between files in base_directory and media_directory, and creates the necessary folders in media_directory.

    Args:
        base_directory (str): The path to the source directory containing the files to be linked.
        media_directory (str): The path to the destination directory where to create the hardlinks.

    Returns:
        None
    """

    # List of files to link with their source and destination paths
    files_to_link = [
        "file_stays.mkv",
        os.path.join("folder_stays", "this_file_stays.mkv"),
        os.path.join("big_folder_stays", "stays.mkv"),
        os.path.join("big_folder_stays", "other_folder_stays", "also_stays.mkv"),
        os.path.join("big_folder_also_stays", "stays.mkv"),
        os.path.join("big_folder_also_stays", "other_folder_stays", "also_stays.mkv")
    ]

    # Browse the list of files to link
    for source_file in files_to_link:
        # Full path to source and destination files
        source_path = os.path.join(base_directory, source_file)
        
        dest_path = os.path.join(media_directory, source_file)

        # Creates destination folders if they do not already exist
        os.makedirs(os.path.dirname(dest_path), exist_ok=True)

        # Creates a hardlink link from the source to the destination
        try:
            os.link(source_path, dest_path)
            print(f"--> Creates a hardlink from '{source_path}' to '{media_directory}/{source_file}'.")
        except FileExistsError:
            print(f"File '{media_directory}/{source_file}' already exists. Hardlink not created.")
        except FileNotFoundError:
            print(f"File '{source_path}' doesn't exists. Hardlink not created.")

if __name__ == "__main__":
    base_directory = "./data/complete"    # transmission /data/complete folder
    media_directory = "./medias"
    print("Creating files/folders structure...")
    create_file_structure(base_directory)
    print("\nCreating hardlinks...")
    create_hardlinks(base_directory, media_directory)
    print("\nGenerating torrents in data/complete/torrents/ folder...")
    generate_torrents(base_directory)
    print("Add these torrents file in transmission.")

