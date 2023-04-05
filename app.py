import os
from flask import Flask, jsonify, request, render_template

app = Flask(__name__)

completed_dir = '/data/completed'
media_dir = '/data/medias'

@app.route('/')
def index():
    return render_template('index.html')

@app.route('/files', methods=['GET'])
def list_files():
    completed_files = {}
    media_files = {}
    for root, dirs, files in os.walk(completed_dir):
        for file in files:
            path = os.path.join(root, file)
            inode = os.stat(path).st_ino
            completed_files[inode] = file
    for root, dirs, files in os.walk(media_dir):
        for file in files:
            path = os.path.join(root, file)
            inode = os.stat(path).st_ino
            media_files[inode] = file
    result = []
    for inode, file in completed_files.items():
        if inode not in media_files:
            result.append({'inode': inode, 'filename': file})
    return jsonify(result)

@app.route('/files/<int:inode>', methods=['DELETE'])
def delete_file(inode):
    filename = None
    for f in os.listdir(completed_dir):
        if os.stat(os.path.join(completed_dir, f)).st_ino == inode:
            filename = f
            break
    if filename is None:
        return jsonify({'error': 'File not found.'}), 404
    try:
        os.remove(os.path.join(completed_dir, filename))
        return jsonify({'message': 'File deleted successfully.'}), 200
    except Exception as e:
        return jsonify({'error': str(e)}), 500

if __name__ == '__main__':
    app.run(debug=True, host='0.0.0.0')

