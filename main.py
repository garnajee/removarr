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

    def main(self, root_dir, extensions):
        """
        Main function to find unlinked torrents and return them with their IDs.

        Args:
            root_dir (str): Root directory where completed torrents are.
            extensions (list): List of file extensions to consider.

        Returns:
            list: A list of tuples (id, name) for torrents found in `root_dir` that are not hardlinked.
        """

        # 1. Obtenir la liste de tous les torrents de Transmission
        rpc_list = self.get_torrents_list()
        
        # 2. Créer un dictionnaire pour un accès rapide aux torrents par leur nom
        # C'est beaucoup plus efficace que de parcourir la liste à chaque fois.
        torrents_map = {torrent.name: torrent.id for torrent in rpc_list}

        # 3. Obtenir la liste des fichiers/dossiers non-hardlinkés depuis le disque
        unlinked_items = file_sweeper.main(root_dir, extensions)

        final_list = []
        
        # 4. Pour chaque élément trouvé sur le disque, chercher un torrent correspondant
        for item_path in unlinked_items:
            # Nettoyer le chemin pour obtenir le nom tel qu'il apparaîtrait dans Transmission
            # ex: "/data/completed/Mon.Film.2023" -> "Mon.Film.2023"
            item_name = item_path.replace(root_dir, '').lstrip('/')
            
            # Vérifier si ce nom existe dans notre dictionnaire de torrents
            if item_name in torrents_map:
                # Si oui, on a trouvé une correspondance !
                torrent_id = torrents_map[item_name]
                final_list.append((torrent_id, item_name))

        return final_list

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

