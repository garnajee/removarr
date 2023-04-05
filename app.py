import os
from flask import Flask, jsonify, request, render_template

# create Flask app instance
app = Flask(__name__)

completed_dir = "/data/completed"
medias_dir = "/data/medias"
file_extension = ('.mkv', '.mp4', '.avi', '.mov')

# default route for the homepage
@app.route('/')
def index():
    return render_template('index.html')

# define route for listing files
@app.route('/files', methods=['GET'])
def list_files():
    completed_files = {}    # dictionary to store the inodes and filenames in completed_dir
    media_files = {}        # dictionary to store the medias_dir inodes and filenames
    result = []             # list to store the result of (inodes) files not in medias_dir

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

    for dirpath, dirnames, filenames in os.walk(medias_dir):
        for filename in filenames:
            if filename.lower().endswith(file_extension):
                inode = os.stat(os.path.join(dirpath, filename)).st_ino
                media_files[inode] = filename

    # Check for files in completed_dir that are not in media_dir
    for inode, filename in completed_files.items():
        if inode not in media_files:
            result.append({'inode': inode, 'filename': filename})

    return jsonify(result)

# define route for deleting a file
@app.route('/files/<int:inode>', methods=['DELETE'])
def delete_file(inode):
    filename = None
    # Loop through completed_dir to find file to delete
    for dirpath, dirnames, filenames in os.walk(completed_dir):
        for f in filenames:
            if os.stat(os.path.join(dirpath, f)).st_ino == inode:
                filename = os.path.join(dirpath, f)
                break
        if filename:
            break
    if filename is None:
        return jsonify({'error': 'File not found.'}), 404
    try:
        os.remove(filename)
        return jsonify({'message': 'File deleted successfully.'}), 200
    except Exception as e:
        return jsonify({'error': str(e)}), 500

if __name__ == '__main__':
    app.run(debug=True, host='0.0.0.0', port="5000")

