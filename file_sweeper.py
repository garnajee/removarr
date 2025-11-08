#!/usr/bin/env python3

import os


def list_files_recursively(root_dir, extensions):
    """
    Retrieves the list of affected files in the root and the directories containing them.

    Args:
        root_dir (str): The path to the root folder.
        extensions (list): The list of file extensions to search for.

    Returns:
        tuple: A tuple containing the list of files in the root and the list of directories that contain the relevant files.
    """
    items_to_remove = []

    for dirpath, dirnames, filenames in os.walk(root_dir):
        # Check for video files with st_nlink == 1
        video_files = [f for f in filenames if f.endswith(tuple(extensions))]
        non_hardlinked_videos = [
            f for f in video_files 
            if os.stat(os.path.join(dirpath, f)).st_nlink == 1
        ]
        
        if not non_hardlinked_videos:
            continue
            
        # Torrent folder detection:
        # 1. Has Sample/Subs subdirectories = torrent folder (don't descend)
        # 2. Has multiple video files = torrent folder (don't descend)
        # 3. Otherwise add individual video files and continue descending
        
        has_sample_or_subs = any(
            subdir.lower() in ['sample', 'subs', 'subtitles', 'extras']
            for subdir in dirnames
        )
        
        if has_sample_or_subs:
            # Torrent folder with Sample/Subs - add whole directory
            items_to_remove.append(dirpath)
            dirnames.clear()
        elif len(video_files) > 1 and not dirnames:
            # Multiple video files in a leaf directory = torrent folder
            items_to_remove.append(dirpath)
        else:
            # Add individual video files (whether or not there are subdirectories)
            for video in non_hardlinked_videos:
                items_to_remove.append(os.path.join(dirpath, video))

    return [], items_to_remove


def list_hardlinked_files(root_dir, extensions):
    """
    Retrieves a list of hardlinked files with the specified extensions.

    Args:
        root_dir (str): The path to the root folder.
        extensions (list): The list of file extensions to search for.

    Returns:
        list: The list of hardlinked files with the specified extensions.
    """
    hardlinked_files = []

    for dirpath, dirnames, filenames in os.walk(root_dir):
        for filename in filenames:
            file_path = os.path.join(dirpath, filename)
            if (
                os.path.isfile(file_path)
                and os.stat(file_path).st_nlink != 1
                and os.path.splitext(file_path)[1] in extensions
            ):
                hardlinked_files.append(file_path)

    return hardlinked_files


def clean_parent_dirs_list(root_dir, parent_dirs_list):
    """
    Cleans up the parent directory list by removing the root_dir path and excluding files.

    Args:
        root_dir (str): The path to the root folder.
        parent_dirs_list (list): The list of parent directories.

    Returns:
        set: A set containing the names of the cleaned parent directories.
    """
    dirs_list = [dir_path.replace(root_dir + "/", "") for dir_path in parent_dirs_list]
    unique_dirs = set(dirs_list)
    unique_dirs.discard("")  # discard remove only if element exists
    unique_dirs.discard(".")
    unique_dirs.discard("..")
    return unique_dirs


def clean_hardlinked_list(root_dir, hardlinked_lists, extensions):
    """
    Cleans up the list of hardlinked files by removing the root_dir path and excluding files.

    Args:
        root_dir (str): The path to the root folder.
        hardlinked_lists (list): The list of hardlinked files.
        extensions (list): The list of file extensions to search for.

    Returns:
        set: A set containing the names of the cleaned hardlinked directories.
    """
    dirs_list = [os.path.dirname(dir_path.replace(root_dir + "/", "")) for dir_path in hardlinked_lists]
    unique_dirs = set(dirs_list) - {"."}
    return {
        dir_name for dir_name in unique_dirs if not dir_name.endswith(tuple(extensions))
    }


def main(root_dir, extensions):
    """
    Main function for cleaning up files and directories.

    Args:
        root_dir (str): The path to the root folder
        extensions (list): The list of file extensions to search for.

    Returns:
        list: A list containing the files to be deleted and the directories to be deleted.
    """
    _, items_to_remove = list_files_recursively(root_dir, extensions)
    
    # Get directories that contain hardlinked files - exclude these
    hardlinked_files_list = list_hardlinked_files(root_dir, extensions)
    hardlinked_dirs = set(os.path.dirname(f) for f in hardlinked_files_list)
    
    # Filter out items that are in directories containing ONLY hardlinked files
    # But keep individual files and folders that have non-hardlinked content
    filtered_items = []
    for item in items_to_remove:
        # If it's a file, check if it has st_nlink == 1
        if os.path.isfile(item):
            if os.stat(item).st_nlink == 1:
                filtered_items.append(item)
        # If it's a directory, check if it's not in hardlinked_dirs
        elif os.path.isdir(item):
            if item not in hardlinked_dirs:
                filtered_items.append(item)
    
    return filtered_items


if __name__ == "__main__":
    root_dir = "./tests/data/complete"
    extensions = [".mkv", ".mp4", ".avi"]
    to_remove = main(root_dir, extensions)
    print("Elements to delete:", to_remove)
