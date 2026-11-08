# Rapport final — files ARR, imports et nettoyage

Audit terminé le 3 novembre 2026. `/volume3/chill/vrac` a été exclu de toutes les opérations.

## Résultat

- Files d'activité Sonarr : **0 élément restant**.
- Files d'activité Radarr : **0 élément restant**.
- Torrents qBittorrent `stoppedUP` : **0**.
- Torrents qBittorrent `missingFiles` : **0**.
- Seeds vérifiés puis repris : **62**.
- Jellyfin a été rafraîchi après les opérations.
- Les 32 tests automatisés du projet réussissent.

## Espace récupéré

| Opération | Gain attribuable |
|---|---:|
| Nettoyage de 7 fichiers validés | 10,03 GiB |
| Suppression des 10 anciens épisodes H264 de *The Handmaid's Tale* S05 | 31,08 GiB |
| Total brut des données supprimées | **41,10 GiB** |
| Nouvelle saison H265 CHiLL ajoutée | −7,75 GiB |
| **Gain net attribuable de la session** | **33,35 GiB** |

La suppression H264 a produit une variation libre observée de 31,07 GiB, très proche de son gain attribuable de 31,08 GiB.

## Sonarr et Radarr : cause des alertes bleues

Il ne s'agissait pas d'un problème de droits `1026:100` ou de mode `755`. Les chemins examinés étaient généralement accessibles en `1030:100` et mode `777`. Les messages réels étaient principalement :

- titre de série non reconnu ou différent (`Series title mismatch`) ;
- import manuel demandé par Sonarr ;
- release refusée car elle n'était pas considérée comme une amélioration.

Sur les 24 torrents affichés dans les files ARR, 23 étaient déjà correctement importés par hardlink. Le dernier, *This Means War* PopHD, faisait partie des suppressions explicitement demandées. Les entrées devenues obsolètes ont été retirées des files sans supprimer les médias.

À noter : Sonarr 4.0.20 renvoie une erreur interne lorsqu'on retire certaines entrées de série inconnue avec `skipRedownload=true`. La reprise sûre a été faite avec `skipRedownload=false`. Cela n'a entraîné aucune perte de données.

## Importations et réparations vérifiées

### Blood & Treasure S01E02

L'épisode CHiLL est correctement importé dans Jellyfin et partage exactement le même inode que sa source dans `completed` : inode **74441**.

### McFarland

Le film présent dans Jellyfin correspondait exactement en taille au fichier principal attendu par qBittorrent. La source a été restaurée par hardlink, puis le torrent a été rechecké et réannoncé. qBittorrent indique désormais :

- progression : **100 %** ;
- données restantes : **0** ;
- état : `stalledUP` — torrent complet, en attente de peers, et non en erreur.

Seuls un petit NFO et la fin de la dernière pièce ont dû être récupérés ; le film de 9,84 GiB n'a pas été retéléchargé.

### The Handmaid's Tale — saison 5

Les 10 épisodes H265 de la release CHiLL ont été importés et leurs hardlinks Jellyfin vérifiés. Les 10 anciennes sources H264 et leurs tâches qBittorrent ont ensuite été retirées avec `deleteFiles=false` côté qBittorrent. La release H265 est complète à 100 % et en seed.

## Suppressions validées

Sept fichiers ont été supprimés, avec quatre tâches qBittorrent retirées en conservant `deleteFiles=false` :

- cinq épisodes 720p devenus inutiles de *La face cachée de la scientologie* : E01, E05, E06, E07 et E08 ;
- la copie PopHD de *This Means War* ; la version ENIGMA est conservée ;
- la copie mHDgz de *The Faculty* ; la version PopHD est conservée.

## Reliquat protégé

Il reste exactement **10 MKV non hardlinkés dans `completed`**, soit **811,10 MiB**. Ce sont exclusivement les samples que tu as demandé de ne pas supprimer : *The Mummy*, *Robin Hood*, *Le Grand Blond*, *Ratatouille*, *Monsters Inc.*, *Casablanca*, les deux *Gendarme*, *The Purge* et *Quantum of Solace*.

Il reste aussi **243 MKV propres à Jellyfin**, soit 409,05 GiB. Ce ne sont pas des doublons démontrés : ils n'ont simplement pas de source partageant leur inode dans `completed`. Parmi eux, 240 sont rattachés à Sonarr ou Radarr et restent disponibles pour la future étude des releases candidates, sans téléchargement lancé dans cette session.

## Fichiers de contrôle

- [Audit final lisible](final-readable-audit-20261103T214112Z/rapport.md)
- [MKV restant dans completed](final-readable-audit-20261103T214112Z/completed-a-classer.csv)
- [Médias Jellyfin sans source completed](final-readable-audit-20261103T214112Z/jellyfin-sans-source-completed.csv)
- [Torrents arrêtés ou manquants](final-readable-audit-20261103T214112Z/qbittorrent-arretes-ou-manquants.csv)
- [Résultat du nettoyage ARR](arr-queue-cleanup-result-20261103T212634Z.json)
- [Résultat du remplacement Handmaid S05](handmaid-s05-h265-finalize-result-20261103T213900Z.json)

