import os
from flask import Flask, jsonify, request, render_template

app = Flask(__name__)

completed_dir = '/tmp/test/completed'
media_dir = '/tmp/test/medias'

@app.route('/')
def index():
    return render_template('index.html')

@app.route('/files', methods=['GET'])
def list_files():
    completed_files = set(os.listdir(completed_dir))
    media_files = set(os.listdir(media_dir))
    result = []
    for filename in completed_files - media_files:
        inode = os.stat(os.path.join(completed_dir, filename)).st_ino
        result.append({'inode': inode, 'filename': filename})
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

