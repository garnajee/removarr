import os
from flask import Flask, jsonify, request, render_template
from main import TransmissionClientManager, QbitClientManager

# create Flask app instance
app = Flask(__name__)

completed_dir = "/data/completed"
medias_dir = "/data/medias"
series_dir = "/data/series"
extensions = [".mkv", ".avi", ".mp4", ".mov"]  # for list_files()

# Initialize the appropriate client based on environment variables
client_type = os.getenv("CLIENT_TYPE", "transmission").lower()
if client_type == "qbittorrent":
    client = QbitClientManager()
else:
    client = TransmissionClientManager()

num_volumes = 0
# check if the directory medias_dir exists
if os.path.isdir(medias_dir):
    # if the directory exists, check if it is empty
    if not os.listdir(medias_dir):
        # if if its empty, movies and series directory are not in medias directory
        num_volumes = 3
        medias_dir = "/data/movies"
else:
    print("ERROR: directory", medias_dir, "doesn't exist")


# default route for the homepage
@app.route("/")
def index():
    return render_template("index.html")


# define route for listing files
@app.route("/files", methods=["GET"])
def list_files():
    """
    List files not hardlinked

    Returns:
        list: A list of tuples containing the id/hash and the filename associated of the torrent not hardlinked
    """
    result = client.main(completed_dir, extensions)
    return jsonify(result)


# define route for deleting a file
@app.route("/files", methods=["DELETE"])
def delete_selected_files():
    # request.json.get("ids") reçoit une liste de listes (ex: [[id1, id2], [id3]])
    selected_ids_groups = request.json.get("ids", [])
    
    # On aplatit la liste (au cas où il y a des cross-seeds)
    flat_ids = []
    for group in selected_ids_groups:
        if isinstance(group, list):
            flat_ids.extend(group)
        else:
            flat_ids.append(group)

    if client_type == "qbittorrent":
        flat_ids = [str(tid) for tid in flat_ids]
    else:
        flat_ids = [int(tid) for tid in flat_ids]

    if not flat_ids:
        return jsonify({"error": "No files selected"}), 400

    client.delete_torrent_and_data(flat_ids)

    return jsonify({"message": "Selected files deleted successfully"}), 200


if __name__ == "__main__":
    app.run(debug=True, host="0.0.0.0", port="5000")
