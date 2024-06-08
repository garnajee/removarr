import os  
from flask import Flask, jsonify, request, render_template
from main import TransmissionClientManager  # import the class from main.py

# create Flask app instance
app = Flask(__name__)

completed_dir = "/data/completed"
medias_dir = "/data/medias"
series_dir = "/data/series"
extensions = [".mkv", ".avi", ".mp4", ".mov"]   # for list_files()

# Initialize TransmissionClientManager
tr_manager = TransmissionClientManager()

num_volumes = 0
# check if the directory medias_dir exists
if os.path.isdir(medias_dir):
    # if the directory exists, check if it is empty
    if not os.listdir(medias_dir):
        # if if its empty, movies and series directory are not in medias directory
        num_volumes = 3
        medias_dir = "/data/movies"
else:
    print("ERROR: directory",medias_dir,"doesn't exist")

# default route for the homepage
@app.route('/')
def index():
    return render_template('index.html')

# define route for listing files
@app.route('/files', methods=['GET'])
def list_files():
    """
    List files not hardlinked

    Returns:
        list: A list of tuples containing the id and the filename associated of the torrent not hardlinked
    """

    result = tr_manager.main(completed_dir,extensions)

    return jsonify(result)

# define route for deleting a file
@app.route('/files/<int:id>', methods=['DELETE'])
def delete_file(id):
    tr_manager.delete_torrent_and_data(int(id))
    return jsonify({'message': 'File deleted successfully'}), 200

# define route for deleting selected files
@app.route('/files/', methods=['DELETE'])
def delete_selected_files():
    # get the selected id from the javascript function
    selected_ids = request.json.get('id', [])

    # converts IDs to integers
    selected_ids = [int(tid) for tid in selected_ids]

    if not selected_ids:
        return jsonify({'error': 'No files selected'}), 400

    tr_manager.delete_torrent_and_data(list(selected_ids))

    return jsonify({'message': 'Selected files deleted successfully'}), 200

if __name__ == '__main__':
    app.run(debug=True, host='0.0.0.0', port="5000")

