import file_sweeper
from transmission_rpc import Client
import os

class TransmissionClientManager:
    def __init__(self, ip=os.getenv("TR_IP"), port=os.getenv("TR_PORT"), 
                 username=os.getenv("TR_USERNAME"), password=os.getenv("TR_PASSWORD")):
        self.ip = ip
        self.port = port
        self.username = username
        self.password = password
        self.transmission_client = self.connect_to_transmission()

    def connect_to_transmission(self):
        """
        Connects to the Transmission RPC server.
        """
        return Client(host=self.ip, port=self.port, username=self.username, password=self.password)

    def get_torrents_list(self):
        """
        Gets the list of torrents from the Transmission RPC server.
        """
        return self.transmission_client.get_torrents(arguments=["id", "name"])

    def delete_torrent_and_data(self, torrent_id):
        """
        Deletes a torrent and its associated data from the Transmission RPC server.
        """
        self.transmission_client.remove_torrent(ids=torrent_id, delete_data=True)

    @staticmethod
    def check_torrents_existence(rpc_list, my_list, root_dir):
        """
        Checks if all elements of torrents_list are present in the list of torrents returned by get_torrents_list(client).

        Args:
            torrents_list (list): List of torrents to check.
            root_dir (str): Root directory to be removed from the paths in torrents_list.

        Returns:
            bool: True if all elements are present, False otherwise.
        """

        # Extract torrent names from the Transmission list
        transmission_names = [torrent.name for torrent in rpc_list]

        # Extract torrent names from file_sweeper.main list by deleting root_dir
        cleaned_torrents = [path.replace(root_dir+'/', '') for path in my_list]

        # Checks whether all the elements in cleaned_torrents are present in transmission_names
        return all(torrent_name in transmission_names for torrent_name in cleaned_torrents)

    def main(self, root_dir, extensions):
        """
        Main function to call other functions and return the final list.

        Args:
            root_dir (str): Root directory.
            extensions (list): List of file extensions.

        Returns:
            list: Final list of elements from my_list with their IDs if check_torrents_existence is True.
        """

        rpc_list = self.get_torrents_list()
        my_list = file_sweeper.main(root_dir, extensions)

        final_list = []

        if self.check_torrents_existence(rpc_list, my_list, root_dir):
            for torrent in rpc_list:
                torrent_name = torrent.name.replace(root_dir+'/', '')
                if any(item.endswith(torrent_name) for item in my_list):
                    final_list.append((torrent.id, torrent.name))

        return final_list
##############################
if __name__ == "__main__":
    ip = "localhost"
    port = 9091
    username = "admin"
    password = "admin"
    root_dir = "./tests/data/complete" # for file_sweeper
    extensions = [".mkv", ".avi", ".mp4"] # for file_sweeper

    # to try without the docker removarr
    # you need to source your .env file
    # commands
    # $ set -a
    # $ source .env
    # $ set +a
    # python3 app.py

    tr_manager = TransmissionClientManager()

    print(" --- debug ","-"*10,"\n")
    torrents = tr_manager.get_torrents_list()
    #print("full list:",torrents)
    torrents_info = [(torrent.id, torrent.name) for torrent in torrents]
    print("list of torrents via RPC :")
    print(torrents_info)
    print()
    to_remove = file_sweeper.main(root_dir,extensions)
    print("List of torrents (folders) via final script :")
    print(to_remove)
    print()
    check_existence = tr_manager.check_torrents_existence(torrents, to_remove, root_dir)
    print("Check if all elements from file_sweeper.main are in rpc list:",check_existence)
    print()
    result = tr_manager.main(root_dir, extensions)
    print("final result:", result)
    print("-"*10)

