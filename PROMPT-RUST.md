Act en tant qu'Architecte Logiciel Senior, Développeur Rust Expert et Spécialiste DevOps/Docker.

Contexte & Objectif :
Après avoir effectué de nombreuses analyses, corrections et suppressions massives de fichiers sur mon infrastructure multimédia, je souhaite disposer d'un outil automatisé et pérenne pour tout gérer sans dépendre de sessions IA manuelles.
Je veux créer une application web containerisée ultra-performante, à très faible empreinte mémoire/CPU, capable d'analyser, de surveiller, de détecter et d'agir sur mon stockage multimédia lié à Radarr, Sonarr, qBittorrent et Jellyfin (tous tournant sous Docker).

Le produit suit toujours le cycle : **observer → proposer → planifier → approuver → exécuter → vérifier → rapporter**. Aucune étape ne doit être cachée dans une autre (un scan ne corrige rien, une recherche ARR ne déclenche aucun grab, un affichage ne supprime rien).

---

1. ÉTAPE PRÉALABLE : SOURCES DE VÉRITÉ ET ANALYSE DE L'EXISTANT

Avant d'écrire la moindre ligne de code ou de proposer une architecture finale, analyse les sources suivantes, dans cet ordre de priorité :

1. **`old/` dans ce dépôt — source de vérité métier.**
   - `old/CONTEXTE_RACINE_APP_NAS.md` : cahier des charges complet (invariants, choix rejetés, workflows, modèle de domaine, tests d'acceptation). En cas de contradiction avec ce prompt, **le CONTEXTE l'emporte** et la contradiction doit être signalée.
   - `old/doublon_checker.py` puis `old/test_doublon_checker.py` (32 tests) : comportement logiciel de référence.
   - `old/generate_final_audit.py` et `old/rapport-final.md` : dernier audit et corrélation fichiers/qBittorrent/ARR.
2. **`/Users/jean/Documents/projets/mon-github/removarr` — ancienne application web (Python/Flask).**
   - À utiliser uniquement comme inspiration pour l'UX et le déploiement (Docker, compose, GHCR).
   - Sa logique métier est un **anti-pattern à ne pas reproduire** : elle détecte les non-hardlinkés via `st_nlink == 1` et supprime via qBittorrent `delete_files=True` / Transmission `delete_data=True`. Ces deux approches sont explicitement rejetées (CONTEXTE §2.12).

Tu vas concevoir et réécrire l'ensemble du projet from scratch en Rust (et non en Python).

---

2. INVARIANTS NON NÉGOCIABLES

Ces règles priment sur toute fonctionnalité, option ou action en masse (détail : CONTEXTE §1.3, §2.12, §3.12–3.15 et §9) :

- **Preuve de hardlink = même `(st_dev, st_ino)` présent dans les deux arbres.** Jamais `st_nlink` seul, jamais le nom seul.
- **Égalité de contenu = comparaison intégrale octet par octet**, obtenue par entonnoir : index inode → index nom normalisé + taille → parsing scene/P2P → échantillons (pour éliminer seulement) → comparaison intégrale de la seule paire retenue. **Pas de hash SHA-256 de tout le NAS.**
- **qBittorrent ne supprime jamais de données** : retrait de tâche toujours avec `deleteFiles=false` ; aucune API interne ne doit permettre `true`. Ne jamais toucher `BT_backup/`.
- **Racines exclues** (ex. `/volume3/chill/vrac`) : jamais parcourues, jamais normalisées dans le périmètre, même par un audit global. Exclusion lexicale ET physique, validée au démarrage.
- **Samples protégés par défaut** (jamais traités comme junk ou orphelins).
- **Sonarr/Radarr = seule autorité qualité** (profils, Custom Formats, scores, cutoff, motifs de rejet). Aucune hiérarchie codec/groupe codée en dur (pas de `AV1 > H265 > H264`). Absence d'ARR ou données ambiguës = décision manuelle.
- **Direction des réparations** : la source canonique est le fichier de `completed` ; la copie physique dans `medias` est remplacée par un hardlink vers cet inode (lien temporaire puis remplacement atomique).
- **Journal d'intention avant mutation** (écrit + `fsync`), puis résultat ou erreur. Plans immuables (hash), revalidés juste avant exécution (existence, type, symlink, confinement, dev, inode, taille, `mtime_ns`, mode, état des services). Toute différence invalide l'action.
- **Transaction qBittorrent** : identifier tous les hashes touchés (cross-seed = plusieurs hashes par inode), arrêter, muter, rechecker avec timeout, reprendre les torrents intacts, laisser arrêtés et visibles ceux dont l'état est incertain.
- **Fail-closed** : scan incomplet, mapping ambigu, API indisponible, permission refusée ou erreur `ENOENT` dans un périmètre = aucune mutation automatique dans ce périmètre.
- **Un seul importeur par média** : ne jamais créer de hardlinks pendant que Sonarr/Radarr importe le même média.
- **Une action n'est réussie qu'après vérification** (rescan, inode, recheck). Sinon : « exécutée, non vérifiée ».

---

3. ARCHITECTURE TECHNIQUE, PERFORMANCE & PRÉSERVATION DES DISQUES (I/O)

- Langage Backend & Frontend : Rust à 100 % côté serveur.
  - Backend : Axum + Tokio.
  - Frontend : rendu HTML côté serveur via Askama, couplé à HTMX pour la réactivité sans SPA (seul JavaScript embarqué : HTMX). Rendu instantané, RAM/CPU minimes, compatibilité mobile parfaite.
  - Graphiques : générés en **SVG côté serveur** (aucune librairie de graphiques JS).
- Base de données : SQLite embarquée via sqlx.
  - Scalable pour 20 000+ fichiers et 3 000+ torrents (cross-seed inclus).
  - Stocke le suivi temporel (date de première apparition, statut, état des hardlinks), les sessions d'audit, les preuves, les plans, le journal d'actions et les instantanés ARR/qBittorrent.
  - Historique complet des actions (horodatage, cible, motif, résultat, gain attribuable avec **identifiant de gain unique** pour empêcher tout double comptage).
  - Fichier SQLite placé sur le volume de configuration Docker (SSD), pas sur les disques médias. WAL, commits par lot.
- Optimisation des I/O (crucial pour les HDD) :
  - Les lectures de contenu (comparaisons intégrales, ffprobe) sont ce qui use les disques : elles sont bornées en concurrence et ne s'exécutent que pour des candidats retenus.
  - Le suivi de `incomplete` s'appuie d'abord sur l'API qBittorrent (`added_on`, état, progression), sans I/O disque.
  - Les scans du système de fichiers sont planifiés (intervalle/cron configurable, ex. nocturne) ou déclenchés manuellement, jamais en boucle. Aucun rafraîchissement d'UI ne doit réveiller les disques.
  - Logs et SQLite sans flush frénétique ; seul le journal d'intention exige un `fsync`.
- Multi-threading & Async : analyse non-bloquante avec workers en tâche de fond, file de tâches persistante, annulation sûre, reprise après redémarrage.

---

4. MODÈLE DE RACINES & CONFIGURATION

L'application raisonne sur des racines typées, configurées dans un fichier de configuration (secrets en variables d'environnement) :

- `download_roots` : `completed`, `incomplete` (qBittorrent).
- `media_roots` : bibliothèque organisée par Radarr/Sonarr et lue par Jellyfin (souvent un seul arbre `medias`).
- `excluded_roots` : racines interdites (ex. `vrac`).
- `path_mappings` : correspondance chemins conteneur qBittorrent/Sonarr/Radarr/Jellyfin ↔ chemins vus par l'application (ex. `/data` → `/volume3/chill`, `/downloads` → `/volume3/chill`). Préfixe validé lexicalement, résultat confiné sous sa racine après résolution, sans remplacement non borné.
- `expected_owner` : UID/GID attendus des services (ex. `1030:100`).
- Seuils : jours de stagnation dans `incomplete`, âge minimal avant action automatique (historiquement 48 h), extensions vidéo, intervalles de scan.
- `dry_run` : **activé par défaut**. `auto_mode` : **désactivé par défaut**.

Jellyfin n'a pas de « dossiers » propres : sa bibliothèque est l'arbre `media_roots`. Son API sert à la santé, au rafraîchissement ciblé et à la vérification de reconnaissance.

---

5. LOGIQUE MÉTIER & DÉTECTIONS (« LES SOUCIS »)

L'application scanne selon des intervalles configurables ou des déclencheurs les racines ci-dessus et communique avec les APIs officielles de qBittorrent, Radarr, Sonarr et Jellyfin.

Chaque souci stocke : catégorie, sévérité, preuve et niveau de preuve, provenance, date d'observation, âge, et éléments bloquants. Les espaces « potentiellement récupérables » ne sont jamais présentés comme des gains garantis et ne comptent chaque inode qu'une fois.

1. **Fichiers incomplets stagnants** : torrents/fichiers dans `incomplete` depuis plus de X jours (seuil configurable, alerte visuelle), croisés avec l'état qBittorrent. Un fichier de `incomplete` sans aucun torrent est un orphelin. Action possible : retrait de la tâche (`deleteFiles=false`) puis suppression des données par l'application après revalidation.
2. **Défauts de hardlinks** : fichier terminé dans `completed` sans même `(st_dev, st_ino)` dans `media_roots`. Selon la preuve, trois issues : **consolider** (copie exacte prouvée dans `medias`), **supprimer** (média remplacé et validé par ARR), ou **conserver/manuel** (ambigu, non géré, sample).
3. **Doublons réels** : deux fichiers distincts (inodes différents) au contenu identique, prouvé par l'entonnoir puis comparaison intégrale. Action : consolidation par hardlink (direction `completed` → `medias`).
4. **Fichiers orphelins** :
   - fichiers de `completed` sans torrent associé, et torrents dont les fichiers sont absents (`missingFiles`) ;
   - fichiers de `media_roots` sans source dans `completed` : **catégorie informative**. C'est souvent normal (torrent retiré après le seed). **Jamais de suppression automatique dans `media_roots`.**
5. **Permissions & propriétaire** : propriétaire/groupe différent de `expected_owner` (ex. `1026:100` au lieu de `1030:100`, ou `root`), et surtout **dossier parent non inscriptible** par l'identité du service (empêche renommage/suppression). Détection seulement pendant le scan ; correction ciblée uniquement via un plan approuvé, jamais de `chmod/chown` récursif implicite.
6. **Fichiers inutiles / junk** : `.rar`, `.zip`, `.nfo`, `.txt`, sous-titres orphelins (sous-titre dont aucune vidéo du même dossier ne partage le nom de base), etc. **Uniquement s'ils n'appartiennent à aucun torrent qBittorrent** (sinon les supprimer casse le seed). Les samples sont exclus du junk.
7. **Protection stricte de qBittorrent (allowlist, fail-closed)** : un torrent n'est éligible à une action destructive que si `progress == 1` **et** son état est dans { `uploading`, `stalledUP`, `queuedUP`, `forcedUP`, `pausedUP` / `stoppedUP` }. Tout autre état est protégé, notamment `downloading`, `metaDL`, `forcedDL`, `queuedDL`, `stalledDL`, `checkingDL`, `checkingUP`, `checkingResumeData`, `allocating`, `moving`, `pausedDL` / `stoppedDL`, `error`, `missingFiles`, `unknown`. Gérer les noms qBittorrent 4.x (`paused*`) et 5.x (`stopped*`).

**Remplacement de release (qualité ARR)** : pour deux versions différentes d'un même média, la décision vient exclusivement de Sonarr/Radarr (profil, Custom Formats, scores, cutoff, motifs). L'instantané ARR est enregistré dans le plan et l'invalide s'il change. ffprobe (durée, pistes, langue audio réelle) complète l'explication et peut bloquer un remplacement. Numérotation absolue/aired/TVDB et specials gérés par mapping explicite. Un remplacement de saison n'est exécuté qu'après preuve de couverture complète, inode par inode.

---

6. INTÉGRATIONS API & INTERACTIVITÉ (WEB UI)

- Web UI Responsive (Mobile-First) :
  - Parfaite visibilité et utilisabilité complète sur smartphone (listes adaptées plutôt que tableaux compressés, boutons tactiles clairs, modales fluides).
  - Dashboard de Synthèse : métriques clés (nombre de soucis par catégorie, espace potentiellement récupérable, espace partagé par hardlinks, santé et version des services, fraîcheur du dernier scan, tâches en cours) avec code couleur d'urgence (Vert, Orange, Rouge), jamais la couleur seule.
  - Vues détaillées par Onglets : un onglet par type de souci, avec filtres, recherche, preuves et éléments bloquants.
  - Onglet Historique & Statistiques : toutes les actions (horodatage exact, cible, motif, taille libérée) et graphiques SVG des gains dans le temps, en distinguant gain potentiel, attendu, attribuable et observé (les snapshots Btrfs peuvent retenir l'espace).
  - Onglet de Logs : journaux clairs dans l'UI et émis vers `stdout` (Docker), niveau configurable (`DEBUG`, `INFO`, `WARN`, `ERROR`), sans aucun secret.

- Actions Flexibles & Sécurisées (Unitaire ou en Masse) :
  - Pour chaque type de correction : **fichier par fichier (1 à 1)** ou **en masse en une seule action**. Le traitement en masse suit exactement les mêmes validations que l'unitaire : les éléments bloqués sont exclus et listés, rien n'est présélectionné.
  - Suppression de dossier : tout ou rien, avec aperçu de toutes les annexes (sous-titres, NFO, images, samples, sous-dossiers).
  - Mode Dry-Run (activé par défaut) : emprunte toute la logique réelle (scan, plan, validations) sauf les adaptateurs de mutation. Aucun fichier, aucune pause/reprise/recheck/retrait de torrent, aucun appel ARR mutateur.
  - Fenêtre de confirmation explicite obligatoire avant toute action destructive : cible, nombre, impact en espace, effets irréversibles.
  - Boutons pour forcer un rescan (qBittorrent, Radarr, Sonarr, Jellyfin).

- Mode automatique (désactivé par défaut) : exécute seulement des plans qui satisfont toutes les règles automatiques du CONTEXTE §3.11 (âge minimal, validation ARR, comparaison intégrale, scan/API/mapping/permissions fiables). Il s'arrête net à la moindre incertitude.

- APIs Radarr, Sonarr, qBittorrent & Jellyfin :
  - APIs REST officielles, avec path mapping configurable (section 4).
  - qBittorrent : support 4.x et 5.x (dont login HTTP 204, `pause`/`stop`).
  - ARR : la lecture d'une recherche ne donne jamais le droit de grab ; grab, import manuel, retrait de file, modification de monitoring sont des capacités séparément autorisées. Une alerte de file ARR ne prouve pas l'absence de hardlink.

---

7. INTERNATIONALISATION (i18n) & CODE

- Langue du projet & Code : code source, commentaires, documentation, logs et commits TOUJOURS en anglais.
- Internationalisation de la Web UI :
  - Anglais par défaut, Français disponible.
  - Traductions externalisées dans `locales/en.json` et `locales/fr.json` (embarquées à la compilation), avec repli sur l'anglais.

---

8. SÉCURITÉ & DÉPLOIEMENT DOCKER / CI-CD

- Sécurité Web UI :
  - Authentification mono-utilisateur par formulaire : la variable d'environnement contient un **hash Argon2** du mot de passe, plus une clé secrète de session. Session chiffrée (HttpOnly, SameSite), CSRF sur toute requête mutatrice, limitation des tentatives de connexion.
  - Aucun secret (mot de passe, clé API, passkey, URL de tracker) dans les logs, rapports, pages HTML ou base.

- Conteneurisation Docker Rootless :
  - Aucun processus root, y compris au démarrage : l'identité est fixée par `user: "${UID}:${GID}"` dans le compose (exemple `1030:100`, identique à qBittorrent/Sonarr). Pas de variables PUID/PGID ni d'entrypoint qui abaisse ses privilèges.
  - Image multi-stage minimale : binaire Rust release statique (musl) + runtime minimal **contenant `ffprobe`** (donc pas `scratch`).
  - Image **multi-arch `linux/amd64` + `linux/arm64`**.
  - **Topologie des volumes (critique pour les hardlinks)** : monter le **parent commun** de `completed`, `incomplete` et `medias` en un seul volume (ex. `/volume3/chill:/data`). Deux bind mounts séparés font échouer `link()` avec `EXDEV`, même sur le même système de fichiers. Conséquence : les racines exclues (ex. `/data/vrac`) sont visibles dans le conteneur et leur exclusion est appliquée et vérifiée par l'application.
  - Volume séparé (SSD) pour la configuration et la base SQLite.
  - L'utilisateur final ne modifie qu'un `docker-compose.yml` template (volumes, `user:`, path mappings, URLs et clés API) et un `.env`.
  - Healthcheck léger qui ne réveille pas les disques.

- GitHub Actions & GHCR (CI/CD) :
  - Sur PR et push sur `main` : `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, build.
  - Publication sur GHCR **uniquement sur tag `v*.*.*`** : tag de version + `latest`, multi-arch. Jamais sur push de `.md`, de branches `fix/*` ou `feat/*`.
  - Actions tierces épinglées par SHA, permissions minimales.

---

9. HORS PÉRIMÈTRE

- Transmission (supporté par l'ancien removarr) : non supporté.
- Prowlarr et indexeurs : aucune intégration directe.
- cross-seed : lecture des relations (plusieurs hashes par inode) uniquement ; aucune modification de sa configuration.
- Grab de release ARR sans approbation explicite.
- Import des anciens rapports JSON/CSV comme état courant.
- Lecture directe des bases SQLite de Sonarr/Radarr (utiliser leurs APIs).

---

Commence par me présenter ton analyse des sources (section 1) et le plan d'architecture technique du projet Rust, en signalant toute contradiction entre ce prompt et `old/CONTEXTE_RACINE_APP_NAS.md`.
