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
    root_files = []
    parent_dirs = []
    
    for dirpath, dirnames, filenames in os.walk(root_dir):
        contains_files = any(file.endswith(tuple(extensions)) and os.stat(os.path.join(dirpath, file)).st_nlink == 1 for file in filenames)
        if contains_files:
            parent_dirs.append(dirpath)
        for filename in filenames:
            file_path = os.path.join(dirpath, filename)
            if os.path.isfile(file_path) and os.stat(file_path).st_nlink == 1 and os.path.dirname(file_path) == root_dir and file_path.endswith(tuple(extensions)):
                root_files.append(file_path)
    
    return root_files, parent_dirs

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
            if os.path.isfile(file_path) and os.stat(file_path).st_nlink != 1 and os.path.splitext(file_path)[1] in extensions:
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
    dirs_list = [dir_path.replace(root_dir+"/", '') for dir_path in parent_dirs_list]
    unique_dirs = set([i.split('/')[0] for i in dirs_list])
    unique_dirs.discard('.') # discard remove only if element exists
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
    dirs_list = [dir_path.replace(root_dir + "/", '') for dir_path in hardlinked_lists]
    first_dirs = [dir_path.split('/')[0] for dir_path in dirs_list]
    unique_dirs = set(first_dirs) - {'.'} 
    return {dir_name for dir_name in unique_dirs if not dir_name.endswith(tuple(extensions))}

def main(root_dir, extensions):
    """
    Main function for cleaning up files and directories.

    Args:
        root_dir (str): The path to the root folder
        extensions (list): The list of file extensions to search for.

    Returns:
        list: A list containing the files to be deleted and the directories to be deleted.
    """
    root_files_list, parent_dirs_list = list_files_recursively(root_dir, extensions)
    set_uniq_folders = clean_parent_dirs_list(root_dir, parent_dirs_list)

    hardlinked_files_list = list_hardlinked_files(root_dir, extensions)
    set_uniq_folders_to_remove = clean_hardlinked_list(root_dir, hardlinked_files_list, extensions)

    files_to_remove = root_files_list
    folders_to_remove = list(root_dir+"/"+i for i in (set_uniq_folders - set_uniq_folders_to_remove))

    return files_to_remove + folders_to_remove

if __name__ == "__main__":
    root_dir = "./tests/data/complete"
    extensions = [".mkv", ".mp4", ".avi"]
    to_remove = main(root_dir, extensions)
    print("Elements to delete:", to_remove)


