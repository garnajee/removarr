import os
from flask import Flask, jsonify, request, render_template

app = Flask(__name__)

completed_dir = "/data/completed"
medias_dir = "/data/medias"
file_extension = ('.mkv', '.mp4', '.avi', '.mov')

@app.route('/')
def index():
    return render_template('index.html')

@app.route('/files', methods=['GET'])
def list_files():
    completed_files = {}
    media_files = {}
    result = []

    for dirpath, dirnames, filenames in os.walk(completed_dir):
        for filename in filenames:
            if filename.lower().endswith(file_extension):
                inode = os.stat(os.path.join(dirpath, filename)).st_ino
                completed_files[inode] = filename

    for dirpath, dirnames, filenames in os.walk(medias_dir):
        for filename in filenames:
            if filename.lower().endswith(file_extension):
                inode = os.stat(os.path.join(dirpath, filename)).st_ino
                media_files[inode] = filename

    for inode, filename in completed_files.items():
        if inode not in media_files:
            result.append({'inode': inode, 'filename': filename})

    return jsonify(result)

@app.route('/files/<int:inode>', methods=['DELETE'])
def delete_file(inode):
    filename = None
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

