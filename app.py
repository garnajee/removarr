import os
from flask import Flask, jsonify, request, render_template

# create Flask app instance
app = Flask(__name__)

completed_dir = "/data/completed"
medias_dir = "/data/medias"
series_dir = "/data/series"
file_extension = ('.mkv', '.mp4', '.avi', '.mov')

# get puid and pgid from docker env variables
pu_id = int(os.environ.get('PUID', 1000))
pg_id = int(os.environ.get('PGID', 100))

# change default user and group according to puid and pgid
os.setuid(pu_id)
os.setgid(pg_id)

num_volumes = 0
if os.path.isdir(medias_dir):
    if not os.listdir(medias_dir):
        num_volumes = 3
        medias_dir = "/data/movies"
else:
    print("ERROR: directory",medias_dir,"doesn't exist")

# default route for the homepage
@app.route('/')
def index():
    return render_template('index.html')

def sizeof_fmt(num, suffix="B"):
    # convert a size to human readable string
    for unit in ["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "Zi"]:
        if abs(num) < 1024.0:
            return f"{num:3.1f} {unit}{suffix}"
        num /= 1024.0
    return f"{num:.1f} Yi{suffix}"

# define route for listing files
@app.route('/files', methods=['GET'])
def list_files():
    completed_files = {}    # dictionary to store the inodes and filenames in completed_dir
    media_files = {}        # dictionary to store the medias_dir inodes and filenames
    result = []             # list to store the result of (inodes) files not in medias_dir
    total_size = 0          # total size of all files found (completed_files)
    file_size = {}          # dictionary to store the file size of an inode

    # recursive loop through all directories and files 
    # os.walk return a 3-tuple containing:
    # current directory path, list of subdirectories in the current directory, and list of files in the current directory.
    for dirpath, dirnames, filenames in os.walk(completed_dir):
        # loop through all files in the current directory
        for filename in filenames:
            # check if the file has the desired file extension
            if filename.lower().endswith(file_extension):
                # store the inode of the file
                inode = os.stat(os.path.join(dirpath, filename)).st_ino
                # store the inode and filename in the dictionary
                completed_files[inode] = filename
                # add the size of an inode file
                file_size[inode] = os.path.getsize(os.path.join(dirpath, filename))

    for dirpath, dirnames, filenames in os.walk(medias_dir):
        for filename in filenames:
            if filename.lower().endswith(file_extension):
                inode = os.stat(os.path.join(dirpath, filename)).st_ino
                media_files[inode] = filename

    if num_volumes == 3:
        for dirpath, dirnames, filenames in os.walk(series_dir):
            for filename in filenames:
                if filename.lower().endswith(file_extension):
                    inode = os.stat(os.path.join(dirpath, filename)).st_ino
                    media_files[inode] = filename

    # Check for files in completed_dir that are not in media_dir
    for inode, filename in completed_files.items():
        if inode not in media_files:
            result.append({'inode': inode, 'filename': filename, 'file_size': sizeof_fmt(file_size[inode])})
            # calculate the total size
            total_size += file_size[inode]
    
    # convert size to human readable
    total_size = sizeof_fmt(total_size)
    
    return jsonify({'total_size': total_size, 'result': result})

def check_delete_empty_folder(file_path):
    # get the parent folder of the file that was just deleted
    folder_path = os.path.dirname(file_path)

    # only proceed if the folder is not the completed_dir folder
    if folder_path == completed_dir:
        return

    # check if the folder is empty or only contains files with certain extensions
    files_to_remove = []
    for f in os.listdir(folder_path):
        file_ext = os.path.splitext(f)[-1].lower()
        if file_ext not in file_extension:
            files_to_remove.append(f)

    # check if the folder is empty or only contains files with certain extensions
    files_to_remove = []
    for f in os.listdir(folder_path):
        f_path = os.path.join(folder_path, f)
        # if it is a folder, do nothing
        if os.path.isdir(f_path):
            return
        else:
            file_ext = os.path.splitext(f)[-1].lower()
            if file_ext not in file_extension:
                files_to_remove.append(f)

    if len(os.listdir(folder_path)) == len(files_to_remove):
        # all files have extensions different from the ones in file_extension, remove them
        for f in files_to_remove:
            os.remove(os.path.join(folder_path, f))
        # if the folder is empty, delete it
        if not os.listdir(folder_path):
            os.rmdir(folder_path)
    elif len(os.listdir(folder_path)) == 0:
        # the folder is empty, delete it
        os.rmdir(folder_path)

# define route for deleting a file
@app.route('/files/<int:inode>', methods=['DELETE'])
def delete_file(inode):
    filename = None
    # Loop through completed_dir to find file to delete
    for dirpath, dirnames, filenames in os.walk(completed_dir):
        for f in filenames:
            # Check if the inode of the file matches the requested inode
            if os.stat(os.path.join(dirpath, f)).st_ino == inode:
                # If there's a match, set the filename variable to the path of the file
                filename = os.path.join(dirpath, f)
                break
        if filename:
            break
    # If no file with the specified inode was found, return an error response
    if filename is None:
        return jsonify({'error': 'File not found.'}), 404
    try:
        # Attempt to delete the file
        os.remove(filename)
        # Call the check_delete_empty_folder function to delete the parent folder if it is empty
        check_delete_empty_folder(filename)
        # If successful, return a success response
        return jsonify({'message': 'File deleted successfully.'}), 200
    except Exception as e:
        # If an exception occurs during deletion, return an error response with the exception message
        return jsonify({'error': str(e)}), 500

# define route for deleting selected files
@app.route('/files/', methods=['DELETE'])
def delete_selected_files():
    selected_inodes = []
    # get the selected inodes from the javascript function
    data = request.get_json()
    selected_inodes = data['inodes']
    # delete the selected files
    for inode in selected_inodes:
        delete_file(inode)
    # return a success response
    return jsonify({'message': 'Files deleted successfully.'}), 200

if __name__ == '__main__':
    app.run(debug=True, host='0.0.0.0', port="5000")

