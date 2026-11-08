# MASTER PROMPT — removarr-v2

> **Fichier maître.** C'est l'unique point d'entrée pour créer l'application. Pour lancer la construction, demande à Claude Code : « Lis `MASTER-PROMPT.md` et exécute-le ».
> Il définit le besoin (sections 1 à 9) et **l'ordre précis de lancement des agents** de `.claude/agents/` (section 10). En cas de conflit entre un agent et ce fichier, ce fichier l'emporte. Le détail métier vient de `old/CONTEXTE_RACINE_APP_NAS.md`.

Act en tant qu'Architecte Logiciel Senior, Développeur Rust Expert et Spécialiste DevOps/Docker, et en tant qu'**orchestrateur** des agents.

Contexte & Objectif :
Après avoir effectué de nombreuses analyses, corrections et suppressions massives de fichiers sur mon infrastructure multimédia, je souhaite disposer d'un outil automatisé et pérenne pour tout gérer sans dépendre de sessions IA manuelles.
Je veux créer une application web containerisée ultra-performante, à très faible empreinte mémoire/CPU, capable d'analyser, de surveiller, de détecter et d'agir sur mon stockage multimédia lié à Radarr, Sonarr, qBittorrent et Jellyfin (tous tournant sous Docker).

Le produit suit toujours le cycle : **observer → proposer → planifier → approuver → exécuter → vérifier → rapporter**. Aucune étape ne doit être cachée dans une autre (un scan ne corrige rien, une recherche ARR ne déclenche aucun grab, un affichage ne supprime rien).

---

1. ÉTAPE PRÉALABLE : SOURCES DE VÉRITÉ ET ANALYSE DE L'EXISTANT

Avant d'écrire la moindre ligne de code ou de proposer une architecture finale, analyse **`old/` dans ce dépôt — source de vérité métier** :

- `old/CONTEXTE_RACINE_APP_NAS.md` : cahier des charges complet (invariants, choix rejetés, workflows, modèle de domaine, tests d'acceptation). En cas de contradiction avec ce prompt, **le CONTEXTE l'emporte** et la contradiction doit être signalée.
- `old/doublon_checker.py` puis `old/test_doublon_checker.py` (32 tests) : comportement logiciel de référence.
- `old/generate_final_audit.py` et `old/rapport-final.md` : dernier audit et corrélation fichiers/qBittorrent/ARR.

Aucune autre base de code n'est à consulter. Les leçons des outils précédents sont déjà intégrées dans ce fichier (sections 2, 5, 6 et 8).

Tu vas concevoir et réécrire l'ensemble du projet from scratch en Rust (et non en Python).

---

2. INVARIANTS NON NÉGOCIABLES

Ces règles priment sur toute fonctionnalité, option ou action en masse (détail : CONTEXTE §1.3, §2.12, §3.12–3.15 et §9) :

- **Preuve de hardlink = même `(st_dev, st_ino)` présent dans les deux arbres.** Jamais `st_nlink` seul, jamais le nom seul.
- **Égalité de contenu = comparaison intégrale octet par octet**, obtenue par entonnoir : index inode → index nom normalisé + taille → parsing scene/P2P → échantillons (pour éliminer seulement) → comparaison intégrale de la seule paire retenue. **Pas de hash SHA-256 de tout le NAS.**
- **qBittorrent ne supprime jamais de données** : retrait de tâche toujours avec `deleteFiles=false` ; aucune API interne ne doit permettre `true`. Ne jamais toucher aux fichiers internes de qBittorrent (dossier `BT_backup/` : `.torrent` et `.fastresume`).
- **Périmètre = chemins configurés uniquement** : l'application ne parcourt, ne lit et ne modifie que les racines déclarées dans `download_roots` / `media_roots` (compose ou `.env`). Aucun chemin n'est supposé par défaut ; un chemin non déclaré n'est jamais analysé. Les racines sont validées au démarrage (existantes, distinctes, sans chevauchement ambigu, sans symlink d'évasion) et tout accès est confiné sous elles.
- **Samples protégés par défaut** (jamais traités comme junk ou orphelins).
- **Sonarr/Radarr = seule autorité qualité** (profils, Custom Formats, scores, cutoff, motifs de rejet). Aucune hiérarchie codec/groupe codée en dur (pas de `AV1 > H265 > H264`). Absence d'ARR ou données ambiguës = décision manuelle.
- **Direction des réparations** : la source canonique est le fichier de `completed` ; la copie physique dans `medias` est remplacée par un hardlink vers cet inode (lien temporaire puis remplacement atomique).
- **Journal d'intention avant mutation** (écrit + `fsync`), puis résultat ou erreur. Plans immuables (hash), revalidés juste avant exécution (existence, type, symlink, confinement, dev, inode, taille, `mtime_ns`, mode, état des services). Toute différence invalide l'action.
- **Transaction qBittorrent** : identifier tous les hashes touchés (plusieurs torrents peuvent partager les mêmes fichiers : plusieurs hashes pour un même inode), arrêter, muter, rechecker avec timeout, reprendre les torrents intacts, laisser arrêtés et visibles ceux dont l'état est incertain.
- **Fail-closed** : scan incomplet, mapping ambigu, API indisponible, permission refusée ou erreur `ENOENT` dans un périmètre = aucune mutation automatique dans ce périmètre.
- **Un seul importeur par média** : ne jamais créer de hardlinks pendant que Sonarr/Radarr importe le même média.
- **Une action n'est réussie qu'après vérification** (rescan, inode, recheck). Sinon : « exécutée, non vérifiée ».

Anti-patterns interdits (déjà rencontrés, à ne jamais reproduire) :
- détecter un fichier « non hardlinké » via `st_nlink == 1` ;
- supprimer des données en demandant au client torrent de le faire (`delete_files=True`, `delete_data=True`) ;
- route HTTP destructive sans authentification, sans CSRF et sans plan approuvé ;
- liste d'exclusions stockée seulement dans le navigateur (`localStorage`) : elle doit être en base, avec motif et date ;
- chemins de racines codés en dur, serveur en mode debug, conteneur exécuté en root ;
- image publiée à chaque push sur la branche principale, actions CI non épinglées.

---

3. ARCHITECTURE TECHNIQUE, PERFORMANCE & PRÉSERVATION DES DISQUES (I/O)

- Langage Backend & Frontend : Rust à 100 % côté serveur.
  - Backend : Axum + Tokio.
  - Frontend : rendu HTML côté serveur via Askama, couplé à HTMX pour la réactivité sans SPA (seul JavaScript embarqué : HTMX). Rendu instantané, RAM/CPU minimes, compatibilité mobile parfaite.
  - Graphiques : générés en **SVG côté serveur** (aucune librairie de graphiques JS).
- Base de données : SQLite embarquée via sqlx.
  - Scalable pour 20 000+ fichiers et 3 000+ torrents.
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
- `path_mappings` : correspondance chemins conteneur qBittorrent/Sonarr/Radarr/Jellyfin ↔ chemins vus par l'application (exemple : qBittorrent voit `/downloads`, l'application voit `/data/completed` ; fournir un exemple par service dans le `docker-compose.yml`). Préfixe validé lexicalement, résultat confiné sous sa racine après résolution, sans remplacement non borné.
- `expected_owner` : UID/GID attendus des services (exemple `1000:1000`).
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
5. **Permissions & propriétaire** : propriétaire/groupe différent de `expected_owner` (ex. `root` ou un autre UID au lieu de `1000:1000`), et surtout **dossier parent non inscriptible** par l'identité du service (empêche renommage/suppression). Détection seulement pendant le scan ; correction ciblée uniquement via un plan approuvé, jamais de `chmod/chown` récursif implicite.
6. **Fichiers inutiles / junk** : `.rar`, `.zip`, `.nfo`, `.txt`, sous-titres orphelins (sous-titre dont aucune vidéo du même dossier ne partage le nom de base), etc. **Uniquement s'ils n'appartiennent à aucun torrent qBittorrent** (sinon les supprimer casse le seed). Les samples sont exclus du junk.
7. **Protection stricte de qBittorrent (allowlist, fail-closed)** : un torrent n'est éligible à une action destructive que si `progress == 1` **et** son état est dans { `uploading`, `stalledUP`, `queuedUP`, `forcedUP`, `pausedUP` / `stoppedUP` }. Tout autre état est protégé, notamment `downloading`, `metaDL`, `forcedDL`, `queuedDL`, `stalledDL`, `checkingDL`, `checkingUP`, `checkingResumeData`, `allocating`, `moving`, `pausedDL` / `stoppedDL`, `error`, `missingFiles`, `unknown`. Gérer les noms qBittorrent 4.x (`paused*`) et 5.x (`stopped*`).

**Rapprochement et unités de traitement** :
- Un élément de `completed` est rattaché à ses torrents par **chemin de contenu** (`save_path` + `content_path`, après path mapping), jamais par le nom du torrent, qui peut différer du dossier réel.
- **Plusieurs torrents, même contenu** : un même contenu peut correspondre à plusieurs torrents (même release ajoutée depuis plusieurs trackers, par exemple). L'UI affiche une ligne par contenu, avec le nombre de torrents associés. Toute action traite tous les hashes concernés.
- **Dossier de release** : un dossier qui contient des sous-dossiers `Sample`/`Subs`/`Extras`, ou plusieurs vidéos, est proposé comme une unité (plus petit dossier entièrement admissible, tout ou rien, avec l'aperçu des annexes). L'UI distingue visuellement dossier et fichier.
- Après suppression, un dossier parent devenu vide, ou ne contenant plus que des annexes déjà listées dans le plan, est retiré dans la même transaction, uniquement sous `completed`.

**Remplacement de release (qualité ARR)** : pour deux versions différentes d'un même média, la décision vient exclusivement de Sonarr/Radarr (profil, Custom Formats, scores, cutoff, motifs). L'instantané ARR est enregistré dans le plan et l'invalide s'il change. ffprobe (durée, pistes, langue audio réelle) complète l'explication et peut bloquer un remplacement. Numérotation absolue/aired/TVDB et specials gérés par mapping explicite. Un remplacement de saison n'est exécuté qu'après preuve de couverture complète, inode par inode.

---

6. INTÉGRATIONS API & INTERACTIVITÉ (WEB UI)

- Web UI Responsive (Mobile-First) :
  - Parfaite visibilité et utilisabilité complète sur smartphone (listes adaptées plutôt que tableaux compressés, boutons tactiles clairs, modales fluides).
  - Dashboard de Synthèse : métriques clés (nombre de soucis par catégorie, espace potentiellement récupérable, espace partagé par hardlinks, santé et version des services, fraîcheur du dernier scan, tâches en cours) avec code couleur d'urgence (Vert, Orange, Rouge), jamais la couleur seule.
  - Vues détaillées par Onglets : un onglet par type de souci, avec filtres, recherche, preuves et éléments bloquants.
  - Dans chaque liste :
    - colonnes nom, âge (jours), taille, nombre de torrents ;
    - tri par nom, taille et âge ;
    - total de la liste et total de la sélection ;
    - bouton « tout sélectionner » (sans présélection par défaut) ;
    - filtre « masquer les éléments récents » (seuil d'âge configurable) ;
    - bouton de rafraîchissement.
  - **Liste d'exclusions (« ignorés »)** : chaque élément peut être marqué « ignoré » (motif optionnel). Il disparaît alors des listes et des actions en masse, et apparaît dans un onglet « Ignorés » d'où il peut être restauré. Ces exclusions sont stockées en base et respectées par le mode automatique.
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
  - Aucun processus root, y compris au démarrage : l'identité est fixée par `user: "${UID}:${GID}"` dans le compose (exemple `1000:1000`, à aligner sur l'identité de qBittorrent/Sonarr/Radarr). Pas de variables PUID/PGID ni d'entrypoint qui abaisse ses privilèges.
  - Image multi-stage minimale : binaire Rust release statique (musl) + runtime minimal **contenant `ffprobe`** (donc pas `scratch`).
  - Image **multi-arch `linux/amd64` + `linux/arm64`**.
  - **Topologie des volumes (critique pour les hardlinks)** : monter le **parent commun** de `completed`, `incomplete` et `medias` en un seul volume (exemple `/srv/media:/data`). Deux bind mounts séparés font échouer `link()` avec `EXDEV`, même sur le même système de fichiers. Conséquence : le conteneur voit tout le parent monté ; seules les racines déclarées sont analysées.
  - Volume séparé (SSD) pour la configuration et la base SQLite.
  - L'utilisateur final ne modifie qu'un `docker-compose.yml` template (volumes, `user:`, path mappings, URLs et clés API) et un `.env`.
  - Healthcheck léger qui ne réveille pas les disques.
  - Port publié par défaut sur `127.0.0.1` uniquement dans le compose (exemple `127.0.0.1:8012:8080`). Exposition sur le réseau = choix explicite de l'utilisateur, idéalement derrière un reverse proxy.
  - Livrer un `.env.example` (sans valeur réelle), un `docker-compose.yml` commenté, un `README.md` (installation, volumes, path mapping, capture d'écran) et des modèles d'issue GitHub (bug, feature).

- GitHub Actions & GHCR (CI/CD) :
  - Sur PR et push sur `main` : `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, build.
  - Publication sur GHCR **uniquement sur tag `v*.*.*`** : tag de version + `latest`, multi-arch. Jamais sur push de `.md`, de branches `fix/*` ou `feat/*`.
  - Actions tierces épinglées par SHA, permissions minimales.

---

9. HORS PÉRIMÈTRE

- Transmission et tout autre client torrent que qBittorrent : non supportés.
- Prowlarr, cross-seed et autres outils tiers : aucune intégration (seule la présence de plusieurs torrents pour un même contenu est gérée, côté qBittorrent).
- Grab de release ARR sans approbation explicite.
- Import des anciens rapports JSON/CSV comme état courant.
- Lecture directe des bases SQLite de Sonarr/Radarr (utiliser leurs APIs).

---

10. ORCHESTRATION — ORDRE DE LANCEMENT DES AGENTS

Les 28 agents de `.claude/agents/` construisent l'application. Leur numéro est un identifiant, **pas** l'ordre d'exécution. Tu (session principale) es l'orchestrateur : tu lances les agents, tu contrôles les portes de qualité et tu ne codes pas toi-même à leur place.

Règles :
1. Exécuter **vague par vague**, dans l'ordre du tableau. Les agents d'une même vague sont lancés **en parallèle** (ils possèdent des modules disjoints, définis par `system-arch` dans `docs/architecture.md`). Ne jamais démarrer une vague avant que la précédente soit terminée et validée.
2. Chaque agent lit les handoffs de ses dépendances dans `docs/handoffs/` avant de commencer, et écrit le sien (`docs/handoffs/<nom-du-fichier-agent>.md`) en fin de tâche.
3. **Porte de qualité** après chaque vague qui produit du code : `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. En cas d'échec, relancer l'agent propriétaire du module avec l'erreur. Ne jamais affaiblir un test pour passer.
4. `21-unit-tests` tourne en continu à partir de la vague 3 : après chaque vague, il met à jour la matrice de parité legacy et comble les trous. Les agents de feature livrent leurs propres tests unitaires.
5. **Points d'arrêt utilisateur (obligatoires)** : tu t'arrêtes et attends ma validation :
   - après la vague 1 : présente l'analyse des sources (`docs/legacy-parity.md`), l'architecture (`docs/architecture.md`) et toute contradiction entre ce fichier et le CONTEXTE ;
   - avant d'activer chaque palier de risque 3, 4 et 5 (voir plus bas) ;
   - à la fin, avec le verdict de `27-release-gate`.
6. À la fin de chaque vague, propose-moi un commit. Le commit se fait sous mon identité git, sans aucune mention ni trailer Claude. Jamais de push, de tag ou de publication sans demande explicite.

| Vague | Agents (en parallèle) | Dépend de |
|---|---|---|
| 0 | `00-legacy-audit` | — |
| 1 | `01-system-arch` | 00 |
| — | **Point d'arrêt utilisateur** | |
| 2 | `02-rust-scaffold` | 01 |
| 3 | `03-domain-model` | 00, 01, 02 |
| 4 | `04-db-layer`, `05-config-secrets` | 03 (05 aussi 02) |
| 5 | `06-fs-engine`, `08-qbit-api`, `09-arr-api`, `10-jellyfin-api` | 03, 05 (06 aussi contrats de 04) |
| 6 | `07-media-match` | 03, 06, interfaces de 09 |
| 7 | `11-issue-engine` | 04, 06, 07, 08, 09, 10 |
| 8 | `12-plan-engine` | 03, 04, 11 |
| 9 | `13-action-runner` | 04, 07, 08, 09, 10, 12 |
| 10 | `14-job-runner` | 04, 11, 13 |
| 11 | `15-reporting` | 04, 11, 13, schémas d'événements de 14 |
| 12 | `16-backend-api` | 13, 14, 15 |
| 13 | `17-auth-guard`, `18-ux-design`, `20-devops` | 16 (17 aussi 04, 05 ; 20 a besoin des routes de santé) |
| 14 | `19-web-ui` | 15, 16, 17, 18 |
| 15 | `22-integration-tests` | 04 à 16 |
| 16 | `23-e2e-perf` | 17, 19, 20, 22 |
| 17 | `24-sec-setup` | tous les tests au vert |
| 18 | `25-sec-audit` | 24 (ou repli explicite sur `/security-review`) |
| 19 | `26-sec-fix`, puis ré-audit ciblé par `25-sec-audit` | 25 |
| 20 | `27-release-gate` | tout ce qui précède |
| — | **Point d'arrêt utilisateur : verdict final** | |

**Paliers de risque (dans le périmètre complet de la V1).** Toutes les fonctionnalités sont en V1, mais les capacités destructives sont construites et validées par risque croissant (CONTEXTE §7.2). `12-plan-engine`, `13-action-runner` et `22-integration-tests` terminent un palier avant d'entamer le suivant :
1. **Lecture seule** : scan, soucis, rapports, chemin dry-run complet.
2. **Consolidation de copie exacte** : test de permission, comparaison intégrale, hardlink temporaire, remplacement atomique, stop/recheck/reprise qBittorrent.
3. **Suppressions sous `completed`** : fichiers non hardlinkés, junk hors torrent, torrents incomplets stagnants, retrait de tâche avec `deleteFiles=false`.
4. **Remplacement de release ARR** : instantanés qualité, preuve de couverture, contrôles ffprobe, mapping de numérotation.
5. **Mode automatique** : uniquement les plans qui satisfont toutes les règles automatiques ; désactivé par défaut.

Un palier n'est terminé que si ses tests de points de crash et de « dry-run sans mutation » passent.

---

**Démarrage :** lance la vague 0 (`00-legacy-audit`), puis la vague 1 (`01-system-arch`), puis arrête-toi au premier point d'arrêt utilisateur.
