# Rapport de Synthèse Contextuelle et Stratégique — Doublon Checker

> Contexte racine destiné à la conception d'une future application de gestion,
> d'audit et de déduplication du stockage multimédia du NAS.

**Important :** ce document n'est pas le résultat d'un nouveau scan en direct du
NAS. Les nombres décrivant l'état courant sont ceux des derniers rapports
horodatés disponibles dans ce projet.

---

## 0. Mode d'emploi de ce document

Ce document est à la fois :

- la synthèse du problème initial et de son évolution ;
- le cahier des charges vivant de la solution ;
- la mémoire des décisions techniques et des incidents ;
- la description de l'existant logiciel ;
- le contexte racine à donner à une autre IA avant de concevoir une application ;
- une base de tests d'acceptation et de règles de sécurité.

Il ne faut pas interpréter toutes les informations avec le même statut. Les
qualificatifs suivants sont employés dans le texte :

| Qualificatif | Signification |
|---|---|
| **Règle permanente** | Invariant de sécurité ou préférence durable à respecter dans toute évolution. |
| **État actuel vérifié** | Fait observé dans le dernier audit du 3 novembre 2026. À revalider avant toute nouvelle mutation. |
| **État historique** | Mesure ou situation vraie à une date antérieure, mais qui ne décrit plus nécessairement le NAS. |
| **Décision ponctuelle** | Choix validé pour un média, une release ou une intervention particulière. Ce n'est pas une règle générale. |
| **Choix rejeté** | Approche étudiée puis explicitement écartée. |
| **Action restante** | Travail non exécuté ou contrôle encore souhaitable. |

### 0.1 Hiérarchie des sources de vérité

En cas de contradiction, appliquer l'ordre suivant :

1. **Pour le comportement logiciel :** code actuel de `doublon_checker.py`, puis
   tests automatisés dans `test_doublon_checker.py`.
2. **Pour l'état du NAS :** rapport final le plus récent, puis inventaire CSV du
   même audit.
3. **Pour une mutation déjà effectuée :** journal JSONL synchronisé et résultat
   JSON de l'opération.
4. **Pour l'intention métier :** décisions explicites de l'utilisateur dans la
   conversation.
5. **Pour l'historique :** anciens rapports et anciennes mesures, en conservant
   toujours leur date.

Une future application doit enregistrer la date, la provenance et le niveau de
preuve de chaque observation. Une donnée historique ne doit jamais être traitée
comme un état courant sans nouveau scan.

### 0.2 Protection des secrets

Les mots de passe, clés API, passkeys et URLs privées de trackers ne doivent pas
être copiés dans les rapports, journaux ou interfaces. Ils sont représentés par
des variables telles que :

- `<QBITTORRENT_URL>`, `<QBITTORRENT_USERNAME>`, `<QBITTORRENT_PASSWORD>` ;
- `<SONARR_URL>`, `<SONARR_API_KEY>` ;
- `<RADARR_URL>`, `<RADARR_API_KEY>` ;
- `<JELLYFIN_URL>`, `<JELLYFIN_API_KEY>` ;
- `<PROWLARR_URL>`, `<PROWLARR_API_KEY>` ;
- `<TRACKER_URL>`, `<TRACKER_PASSKEY>`.

Des secrets ont été visibles dans certaines sorties de diagnostic historiques.
Le renouvellement du mot de passe WebUI qBittorrent et des clés API concernées
reste recommandé.

---

# 1. Vision globale et objectifs fondamentaux

## 1.1 Problème macroscopique

Le NAS héberge une bibliothèque multimédia d'environ 20 TiB. Les téléchargements
qBittorrent (dans mon cas) sont stockés sous :

```text
/volume3/chill/completed
```

La bibliothèque consommée par Jellyfin est organisée sous :

```text
/volume3/chill/medias
```

Le fonctionnement attendu est qu'un fichier téléchargé et le fichier présenté à
Jellyfin soient deux noms de répertoire pointant vers le **même inode** grâce à
un hardlink. Le fichier peut alors :

- rester disponible dans l'arborescence qBittorrent pour le seed ;
- être classé proprement dans l'arborescence Jellyfin ;
- n'occuper qu'une seule fois les blocs physiques du volume.

Le problème initial était la présence de fichiers qui semblaient exister des
deux côtés, ou qui n'existaient que dans `completed`, sans partager réellement
leur inode. Ces copies consommaient inutilement plusieurs centaines de GiB, puis
plusieurs TiB à l'échelle de l'ensemble des lots étudiés.

## 1.2 Modèle conceptuel du stockage

Un hardlink n'est pas une copie. Deux chemins sont correctement consolidés si,
sur le même système de fichiers, leurs `st_dev` et `st_ino` sont identiques :

```text
identité physique = (st_dev, st_ino)
```

`st_nlink` indique le nombre total de noms pointant vers un inode, mais ne dit
pas où se trouvent ces noms. Un fichier avec `st_nlink == 2` peut avoir son autre
lien ailleurs que dans Jellyfin. Inversement, une politique fondée uniquement
sur cette valeur ne prouve pas la relation `completed ↔ medias`.

Trois mesures doivent toujours être distinguées :

- **taille logique :** somme des tailles visibles des fichiers ;
- **espace alloué unique :** blocs réellement alloués, comptés une seule fois
  par inode ;
- **variation libre observée :** différence de l'espace libre du volume avant
  et après l'opération.

La variation observée peut être affectée par les snapshots Btrfs, les fichiers
encore ouverts, les téléchargements et autres écritures simultanées. Le gain le
plus explicable est donc le **gain attribuable**, calculé à partir des blocs des
inodes dont le dernier lien utile a effectivement disparu.

## 1.3 Objectifs non négociables

1. **Règle permanente — aucune perte de média Jellyfin.** Une suppression dans
   `completed` ne doit jamais être décidée sans comprendre si le contenu existe
   ailleurs, s'il est correctement importé et quelle version doit être conservée.
2. **Règle permanente — pas de décision fondée uniquement sur le nom.** Le nom et
   la taille servent à trouver des candidats ; ils ne prouvent pas l'égalité du
   contenu.
3. **Règle permanente — pas de décision fondée uniquement sur `st_nlink`.** La
   présence effective du même `(st_dev, st_ino)` dans les deux arbres est la
   preuve d'un hardlink vers Jellyfin.
4. **Règle permanente — direction des réparations.** La source canonique du lien
   est le fichier de `completed`; le lien est créé vers le chemin classé de
   `medias`. En pratique, la copie physique déjà présente dans Jellyfin est
   remplacée par un nom pointant vers l'inode de `completed`.
5. **Règle permanente — qBittorrent ne supprime jamais les données à la place du
   système d'audit.** Le retrait d'une tâche utilise exclusivement
   `deleteFiles=false`.
6. **Règle permanente — ne jamais manipuler directement** les fichiers
   `BT_backup/<hash>.torrent`.
7. **Règle permanente — journaliser l'intention avant la mutation**, synchroniser
   le journal sur disque, puis consigner le résultat ou l'erreur.
8. **Règle permanente — `/volume3/chill/vrac` est hors périmètre.** (dossier perso) Aucun scan,
   calcul, recommandation ou nettoyage de ce projet ne doit le parcourir.
9. **Règle permanente — la politique de qualité vient de Sonarr/Radarr.** Les
   profils de qualité, Custom Formats, scores, cutoffs et décisions de rejet ARR
   sont la source de vérité pour décider quelle release est préférable.
10. **Règle permanente — l'analyse technique reste explicative.** Codec,
    résolution, langue, groupe, source et taille doivent être affichés, mais ne
    forment pas une hiérarchie de qualité concurrente de Sonarr/Radarr.
11. **Règle permanente — absence d'ARR = décision manuelle.** Si le média n'est
    pas géré, si l'API est indisponible ou si les données sont ambiguës, aucune
    suppression automatique fondée sur la qualité n'est autorisée.

## 1.4 Politique de qualité : clarification essentielle

La préférence historique souvent formulée comme :

```text
AV1 > H265/x265 > H264/x264
```

est un exemple de politique recherchée, pas une vérité à coder dans
l'application. Elle doit être exprimée dans les **Custom Formats** et profils de
qualité de Sonarr/Radarr. La même règle s'applique aux pénalités de groupes de
release comme `GL0P` ou `BATGirl` : elles appartiennent aux Custom Formats ARR.

L'application future doit :

- lire le profil attaché au film ou à la série ;
- lire les Custom Formats et scores applicables ;
- afficher le score de la version actuelle et de chaque candidate ;
- afficher les motifs d'acceptation ou de rejet fournis par ARR ;
- respecter le cutoff et les règles d'upgrade ;
- conserver un instantané de ces données dans le plan d'action.

Elle ne doit pas décider qu'un AV1 est automatiquement meilleur qu'un H265, ou
qu'un H265 est automatiquement meilleur qu'un H264, indépendamment du profil,
de la langue, de l'édition, de la source, de la résolution et du score ARR.

---

# 2. Architecture technique et choix validés

## 2.1 Écosystème actuel

```text
                         Prowlarr / indexeurs
                                  │
                    ┌─────────────┴─────────────┐
                    │                           │
                 Sonarr                      Radarr
                    │                           │
                    └─────────────┬─────────────┘
                                  │ décisions qualité/import
                                  ▼
                         qBittorrent + cross-seed
                                  │
                       /volume3/chill/completed
                                  │
                     hardlinks vérifiés par inode
                                  │
                       /volume3/chill/medias
                                  │
                               Jellyfin
```

### Composants

| Composant | Rôle |
|---|---|
| Synology / volume 3 | Stockage des téléchargements et de la bibliothèque multimédia. |
| qBittorrent | Téléchargement, seed, recheck et état des torrents. |
| Sonarr | Identification, recherche, qualité et import des séries. |
| Radarr | Identification, recherche, qualité et import des films. |
| Prowlarr | Agrégation des indexeurs utilisés par Sonarr/Radarr. |
| Jellyfin | Consommation et indexation de la bibliothèque classée. |
| cross-seed | Recherche et injection de torrents compatibles partageant les mêmes données. |
| Doublon Checker | Audit des inodes, rapprochement, consolidation, nettoyage et rapports. |

## 2.2 Arborescences et frontières

| Chemin | Rôle | Politique |
|---|---|---|
| `/volume3/chill/completed` | Téléchargements terminés et sources de seed | Lecture et mutations contrôlées autorisées. |
| `/volume3/chill/incomplete` | Téléchargements incomplets | Examiner avec qBittorrent ; ne pas assimiler à des médias terminés. |
| `/volume3/chill/medias` | Bibliothèque Jellyfin | Lecture par défaut ; mutation seulement pour un import ou remplacement explicitement planifié et vérifié. |
| `/volume3/chill/vrac` | Données volontairement hors périmètre | Exclusion absolue, y compris des audits globaux. |
| `/volume3/chill/doublon-checker-reports` | Rapports et journaux sur le NAS | Ne doit être contenu ni dans `completed` ni dans `medias`. |
| `analyse/` dans le projet local | Rapports lisibles et contexte durable | Index humain, CSV et synthèses horodatées. |
| `/volume2/docker/chill-extra` | Déploiement cross-seed | Ne pas modifier sans accord explicite. |

L'intention initiale était de considérer `medias` comme entièrement en lecture
seule. L'usage a ensuite été précisé : les **audits** n'y mutent rien, mais les
workflows explicitement validés peuvent y créer un hardlink, remplacer une copie
ou retirer une ancienne release. Cette exception doit être transactionnelle et
journalisée.

## 2.3 Mappings de chemins

Le mapping qBittorrent validé est conceptuellement :

```text
chemin conteneur : /data
chemin hôte      : /volume3/chill
```

Des historiques Sonarr/Radarr peuvent aussi utiliser `/downloads`. La future
application doit normaliser les chemins à travers une table de mappings
configurable et refuser toute valeur qui sort de la racine déclarée.

Exemples de conversions :

```text
/data/completed/X      → /volume3/chill/completed/X
/downloads/completed/X → /volume3/chill/completed/X
```

Une conversion ne doit jamais être faite par simple remplacement non borné. Le
préfixe doit être validé lexicalement et le chemin résultant doit rester sous la
racine autorisée après résolution.

## 2.4 Identités et permissions

Les services qBittorrent et cross-seed fonctionnent avec l'identité attendue (docker) :

```text
UID 1030
GID 100
```

Sonarr utilise également `1030:100` dans les opérations examinées. Des fichiers
ou dossiers historiques appartenaient à `1026:100`, ce qui a provoqué des
échecs de renommage ou de suppression de noms même lorsque les fichiers étaient
lisibles.

Les permissions sont à contrôler à deux niveaux :

- lecture du fichier source ;
- droit d'écriture et de renommage dans le dossier parent de destination.

Le mode `755` n'est pas universellement suffisant : seul le propriétaire peut
alors écrire. Le mode et le propriétaire doivent être cohérents avec l'identité
du service qui importe. Les corrections historiques ont utilisé `1030:100` et,
selon le besoin, un mode de dossier `775`. Une future application ne doit pas
appliquer récursivement `chmod` ou `chown` sans plan précis et approbation.

## 2.5 Moteur Python existant

Le cœur se trouve dans `doublon_checker.py` et cible Python 3.11.15. Il utilise
la bibliothèque standard pour :

- le parcours de fichiers ;
- les statistiques et identités d'inodes ;
- les requêtes HTTP vers qBittorrent ;
- l'interface terminal ;
- les rapports JSON, JSONL et CSV ;
- la comparaison de contenu ;
- l'orchestration des opérations.

`ffprobe` est une dépendance externe **facultative**, appelée par
`subprocess` lorsque la vérification de durée ou de pistes est activée.

### Modèles principaux existants

- `Config` : configuration explicitement passée aux fonctions ;
- `EntryRecord` : chemin, inode, appareil, taille, blocs, mode, date et type ;
- `TreeScan` : entrées et erreurs d'un scan ;
- `ParsedName` : titre, année, épisode, résolution, codec, source, langues et
  éditions ;
- `Similarity` : rapprochement entre une source et une version possible ;
- `DeletionCandidate` : fichier ou dossier proposé avec snapshot et annexes ;
- `HardlinkRepair` : paire `completed`/`medias` à consolider ;
- `AutoCandidateDecision` : admissibilité, action et motifs ;
- `QbitTorrent`, `QbitFile`, `QbitIndex` : état qBittorrent et chemins mappés ;
- `DeletionResult` : compteurs et mesures finales.

### Configuration historique du script

Les valeurs sont regroupées dans :

```python
if __name__ == "__main__":
    ...
```

Le script ne prend pas d'arguments de lancement. Les fonctions reçoivent un
objet `Config`, ce qui évite les variables globales dispersées.

Paramètres structurants :

- extensions vidéo, historiquement `{'.mkv'}` ;
- seuil de similarité d'identification, historiquement `0.60` ;
- timeout de recheck, historiquement 600 secondes ;
- échantillons rapides, historiquement cinq blocs de 1 MiB ;
- blocs de comparaison intégrale, historiquement 16 MiB ;
- modes `interactive`, `dry-run`, `auto` ;
- âge automatique minimal, historiquement 48 heures ;
- seuil historique automatique de similarité, initialement `0.90` ;
- option de retrait des torrents entièrement vides ;
- paramètres `ffprobe` et tolérances de durée.

Le seuil de similarité identifie un média possible. Il ne mesure pas la qualité
et ne doit jamais remplacer le score Sonarr/Radarr.

## 2.7 Interface terminal existante

L'interface interactive est paginée. Les commandes historiques sont :

- `n` : page suivante ;
- `p` : page précédente ;
- `s IDS` : sélectionner des identifiants ou plages ;
- `u IDS` : désélectionner ;
- `d ID` : afficher le détail ;
- `x` : exécuter la sélection après validations ;
- `q` : quitter ou ignorer la phase proposée.

Aucun élément n'est présélectionné. Une future interface peut remplacer cette
syntaxe, mais doit conserver la distinction entre affichage, sélection, plan et
exécution.

## 2.8 Rapports existants

Une session du moteur générique produit notamment :

- `scan-<date>.json` ;
- `candidates-<date>.csv` ;
- `actions-<date>.jsonl` ;
- `summary-<date>.txt` ;
- inventaires bidirectionnels des fichiers sans inode partagé ;
- `auto-plan-<date>.json` en mode automatique ;
- rapport après réparation et rapport de vérification de durée le cas échéant.

Les opérations ciblées ont ajouté des plans et résultats JSON spécialisés ainsi
que des synthèses dans `analyse/<horodatage>/`.

## 2.9 Couverture de tests existante

Le projet comprend 32 tests automatisés couvrant notamment :

- vrais hardlinks et comptage unique des blocs ;
- faux positif `st_nlink` dû à un lien extérieur à Jellyfin ;
- calcul de gain lorsque plusieurs liens sont concernés ;
- parsing scene/P2P et score de similarité ;
- refus d'éditions différentes ;
- inventaires bidirectionnels ;
- contrôle de durée ;
- proposition d'une copie de même nom et taille sans la considérer comme preuve ;
- obligation de comparaison intégrale avant réparation ;
- direction `completed → medias` ;
- test de permission avant comparaison longue ;
- plus petit dossier entièrement orphelin et annexes ;
- changement entre scan et suppression ;
- non-suivi des symlinks ;
- règles du mode automatique ;
- interruption et restauration des torrents ;
- torrents partiellement touchés laissés arrêtés ;
- parsing des plages de sélection ;
- mapping et API qBittorrent 4.x, 5.x et 5.2 ;
- garantie d'absence de mutation en `dry-run` ;
- réparation automatique et retrait sûr d'un torrent vide.

## 2.10 Scripts spécialisés et dette d'architecture

Le dépôt contient de nombreux scripts d'audit et d'exécution ciblés : import de
séries, correction de files Sonarr, recherche ARR, remplacement de saisons,
audit cross-seed, correction de Lupin, Love Hina, Snowfall, Handmaid, etc.

Ils représentent une somme importante de logique et de preuves opérationnelles,
mais ne constituent pas une API applicative cohérente. Beaucoup :

- réimplémentent l'accès HTTP ou qBittorrent ;
- portent des chemins et titres en constantes ;
- lisent les bases ou configurations ARR directement ;
- mélangent audit, plan et exécution ;
- sont idempotents seulement pour le cas particulier traité.

La future application doit extraire les mécanismes communs dans des services de
domaine testés. Les scripts restent des références historiques et des scénarios
de non-régression.

## 2.11 Autorité de Sonarr/Radarr sur la qualité

Pour chaque média géré, Sonarr ou Radarr doit être l'autorité pour :

- le profil de qualité affecté ;
- les qualités autorisées ;
- le cutoff ;
- l'autorisation d'upgrade ;
- les Custom Formats ;
- le score de chaque Custom Format ;
- le score total de release ;
- les langues et attributs exprimés par les Custom Formats ;
- les motifs de rejet ;
- le statut monitored et la couverture d'épisodes.

La future application peut détecter et présenter des métadonnées indépendantes
pour expliquer ou contrôler une décision. Elle ne doit pas recalculer une
politique parallèle. Si ARR considère une candidate comme rejetée, une
automatisation ne doit pas la substituer silencieusement. Une dérogation reste
possible uniquement comme décision manuelle explicite et auditée.

## 2.12 Choix explicitement rejetés

1. **Choix rejeté :** considérer `st_nlink == 2` comme preuve d'import.
2. **Choix rejeté :** calculer un SHA-256 complet pour tous les fichiers du NAS.
3. **Choix rejeté :** comparer intégralement chaque fichier à chaque fichier.
4. **Choix rejeté :** considérer `même nom + même taille` comme égalité certaine.
5. **Choix rejeté :** retirer manuellement les fichiers `BT_backup`.
6. **Choix rejeté :** appeler qBittorrent avec `deleteFiles=true`.
7. **Choix rejeté :** supprimer automatiquement un orphelin sans remplaçant
   Jellyfin fiable.
8. **Choix rejeté :** poursuivre un mode automatique si le scan, le mapping ou
   une API est incomplet.
9. **Choix rejeté :** coder en dur `AV1 > H265 > H264` dans l'application.
10. **Choix rejeté :** gérer les pénalités de groupes hors des Custom Formats ARR.
11. **Choix rejeté :** imposer un dossier `cross-seed-links` alors que le montage
    et les hardlinks existants sont adaptés.
12. **Choix rejeté :** abandonner le matching strict de cross-seed sans besoin
    démontré.
13. **Choix rejeté :** modifier la configuration cross-seed sans accord.

---

# 3. Cahier des charges vivant

## 3.1 Principes d'exécution

Toute fonctionnalité doit appartenir à l'un des niveaux suivants :

1. **observer** : lecture seule, aucune mutation distante ou locale ;
2. **proposer** : produire des candidats et expliquer les preuves ;
3. **planifier** : figer une liste d'actions et les états attendus ;
4. **approuver** : recueillir une validation explicite ou appliquer une politique
   automatique déjà autorisée ;
5. **exécuter** : effectuer uniquement les actions du plan immuable ;
6. **vérifier** : rescanner et prouver le résultat ;
7. **rapporter** : enregistrer gains, erreurs et actions restantes.

Une action de niveau supérieur ne doit pas être cachée dans une fonction de
niveau inférieur. Une recherche ARR ne doit pas déclencher un grab. Un scan ne
doit pas corriger des permissions. Un affichage ne doit pas supprimer une tâche.

## 3.2 Inventaire des fichiers

### Parcours

- Utiliser un parcours récursif efficace fondé sur `os.scandir` ou un mécanisme
  équivalent exposant les mêmes garanties.
- Ne jamais suivre un lien symbolique pendant le scan ou la suppression.
- Collecter pour chaque entrée : chemin relatif, type, taille, blocs alloués,
  appareil, inode, nombre de liens, `mtime_ns`, mode et extension.
- Capturer chaque erreur avec son chemin et son message.
- Marquer le sous-arbre concerné comme incomplet.
- Exclure lexicalement et physiquement `/volume3/chill/vrac` avant toute entrée
  dans l'arbre.

### Identités et ensembles

Construire au minimum :

- index `identity → chemins completed` ;
- index `identity → chemins medias` ;
- index `nom normalisé + taille → entrées` ;
- index par titre/année ;
- index par titre/saison/épisode ;
- inventaire des identités partagées ;
- inventaire `completed` sans identité Jellyfin ;
- inventaire Jellyfin sans identité `completed`.

### Statistiques

Afficher :

- nombre total de fichiers ;
- nombre de vidéos ciblées ;
- taille logique ;
- espace alloué unique ;
- nombre d'inodes partagés ;
- espace partagé ;
- espace potentiellement récupérable ;
- espace libre du volume ;
- erreurs et zones protégées.

L'espace récupérable n'est pas une autorisation de suppression. Il représente
un potentiel soumis à classification et validation.

## 3.3 Détection des correspondances

### Niveau 1 — preuve physique

Même `(st_dev, st_ino)` : le hardlink existe réellement. Aucune comparaison de
contenu supplémentaire n'est nécessaire pour prouver l'identité physique.

### Niveau 2 — candidat exact rapide

Même nom de fichier normalisé et même taille : doublon potentiel très fort,
mais non encore prouvé. Cette recherche doit être faite par index et rester
rapide sur plusieurs dizaines de milliers de fichiers.

### Niveau 3 — candidat sémantique

Analyser la nomenclature scene/P2P :

- titre ;
- année ;
- `SxxExx` ou autres formes d'épisode ;
- résolution ;
- codec vidéo ;
- source ;
- langue ;
- édition (`EXTENDED`, `IMAX`, `UNRATED`, etc.) ;
- groupe de release ;
- taille.

Retourner au maximum quelques candidats pertinents, historiquement trois, avec
les raisons et le score. Pour les séries, l'identité de saison/épisode domine le
score de texte. Pour les films, le titre et l'année sont structurants.

### Niveau 4 — échantillonnage

Pour les noms différents, des échantillons répartis dans le fichier peuvent
éliminer rapidement un faux candidat. Un échantillonnage positif ne prouve
jamais l'égalité complète.

### Niveau 5 — comparaison intégrale

Avant de remplacer une copie par un hardlink, comparer tous les octets par
blocs. Les deux fichiers doivent rester inchangés pendant l'opération. Une
différence, même tardive, annule la réparation.

### Vérifications multimédias complémentaires

Pour des versions différentes mais représentant le même média, `ffprobe` peut
fournir :

- durée ;
- codec réel ;
- résolution ;
- pistes audio ;
- langues déclarées ;
- sous-titres.

Ces données aident à détecter un mauvais cut, une édition différente ou une
absence de français. Elles ne prouvent pas l'égalité binaire.

## 3.4 Évaluation de la qualité par ARR

Pour chaque remplacement envisagé :

1. identifier le film Radarr ou la série/épisode Sonarr ;
2. récupérer le profil de qualité affecté ;
3. récupérer les paramètres d'upgrade et le cutoff ;
4. récupérer les Custom Formats et leurs scores ;
5. obtenir, si possible, la décision ARR sur la release candidate ;
6. enregistrer les motifs de rejet, notamment langue, qualité, taille, seeds,
   release déjà supérieure, série inconnue ou score insuffisant ;
7. comparer la version actuelle et la candidate dans le référentiel ARR ;
8. afficher les métadonnées techniques comme explication ;
9. autoriser l'automatisation seulement si ARR considère la candidate admissible
   et améliorante selon le profil concerné.

L'instantané du plan doit contenir :

- identifiant ARR du média ;
- profil et cutoff ;
- Custom Formats correspondants ;
- détail des scores ;
- score total actuel et candidat ;
- décision et motifs ARR ;
- date de lecture de la configuration.

Si le profil ou les scores changent entre planification et exécution, le plan est
obsolète et doit être revalidé.

## 3.5 Réparation d'une copie exacte par hardlink

### Préconditions

- source sous `completed` ;
- destination sous `medias` ;
- source et destination sur le même `st_dev` ;
- aucun chemin symbolique dans la chaîne utilisée ;
- scan complet des deux zones ;
- source et destination inchangées depuis le scan ;
- droit de créer et renommer une entrée dans le dossier Jellyfin ;
- torrents concernés identifiés ;
- espace de rapport disponible.

### Transaction

1. Écrire le plan et le synchroniser.
2. Tester la création d'un hardlink temporaire dans le dossier cible, puis
   retirer ce test.
3. Arrêter ou mettre en pause les torrents concernés selon la version qBittorrent.
4. Revalider `dev`, inode, taille, date, mode et chemin.
5. Comparer intégralement les contenus.
6. Écrire et synchroniser l'intention d'action.
7. Créer un hardlink temporaire depuis la source `completed` dans le dossier
   Jellyfin.
8. Remplacer atomiquement l'ancienne destination.
9. Vérifier que la destination et la source partagent `(st_dev, st_ino)`.
10. Rechecker les torrents concernés.
11. Restaurer l'état initial des torrents non modifiés ; conserver arrêtés ceux
    dont l'état des données est incertain.
12. Rescanner Jellyfin et ARR si nécessaire.
13. Mesurer le gain et journaliser le résultat.

Le test de permission précède la comparaison intégrale afin d'éviter plusieurs
minutes de lecture pour découvrir ensuite un `Operation not permitted`.

## 3.6 Import d'une version différente dans Jellyfin

Une version différente ne peut pas être « consolidée » par comparaison binaire.
Elle constitue un remplacement de média.

Le workflow doit :

- identifier le média et la couverture exacte ;
- vérifier le classement ARR et les motifs de décision ;
- vérifier langue, durée, édition et résolution ;
- s'assurer que chaque épisode attendu existe ;
- protéger les specials et numérotations alternatives ;
- planifier le chemin Jellyfin final ;
- créer les nouveaux hardlinks ;
- déclencher un rescan ciblé ;
- prouver l'import inode par inode ;
- retirer l'ancienne version seulement après cette preuve ;
- nettoyer les tâches qBittorrent uniquement avec `deleteFiles=false` ;
- calculer le gain net : anciennes données libérées moins nouvelles données
  réellement ajoutées.

## 3.7 Suppression sous `completed`

### Candidats fichiers

Un fichier ne peut être supprimé automatiquement que si :

- sa cible reste sous `completed` ;
- il est inchangé depuis le scan ;
- le scan est complet ;
- il n'est pas un sample protégé ;
- il n'est pas impliqué dans une réparation échouée ;
- le média de remplacement est identifié sans ambiguïté ;
- sa couverture film/épisode est correcte ;
- Sonarr/Radarr valide le remplacement selon le profil du média ;
- les exigences d'âge et de stabilité sont satisfaites ;
- les torrents associés sont connus ou l'absence de torrent est explicitement
  établie.

### Candidats dossiers

Proposer le plus petit sous-dossier dont tous les MKV sont admissibles. Afficher
également :

- sous-titres ;
- NFO ;
- images ;
- samples ;
- fichiers texte ;
- sous-dossiers ;
- tout autre fichier annexe qui disparaîtrait.

Le dossier est tout ou rien. Un MKV ambigu, récent, protégé ou non identifié
rend le dossier non admissible au mode automatique.

### Samples

**Règle permanente issue des décisions utilisateur :** tout fichier dont le nom
indique un sample est conservé, sauf instruction explicite contraire ciblant ce
sample. Les samples ne doivent pas être assimilés à des films orphelins.

## 3.8 Coordination qBittorrent

### Indexation

- authentifier via l'API Web ;
- supporter les différences qBittorrent 4.x et 5.x ;
- indexer les torrents et leurs fichiers ;
- mapper leurs chemins conteneur vers l'hôte ;
- signaler toute erreur de mapping ;
- bloquer les mutations automatiques si le mapping est incomplet.

### Avant mutation

- déterminer tous les hashes touchés ;
- enregistrer leur état initial ;
- arrêter ou pauser les tâches ;
- vérifier qu'une tâche ne couvre pas des fichiers hors du plan.

### Après mutation

- rechecker ;
- attendre la fin du recheck avec timeout ;
- ne retirer automatiquement que les tâches dont tous les fichiers demandés ont
  réellement disparu et dont le retrait est prévu ;
- toujours utiliser `deleteFiles=false` ;
- laisser les torrents partiellement touchés arrêtés ;
- reprendre ceux qui n'ont subi aucune modification ;
- rapporter les torrents restant arrêtés.

`stalledUP` désigne généralement un torrent complet en attente de peers ; ce
n'est pas une erreur. `missingFiles` exige une investigation. `stoppedUP` peut
être volontaire ou être un état résiduel après recheck ; il ne doit pas être
supprimé aveuglément.

## 3.9 Coordination Sonarr/Radarr

La chaîne de corrélation recommandée est :

```text
fichier → chemin qBittorrent → hash → historique ARR → film/épisode → fichier Jellyfin
```

Les recherches de releases sont séparées des grabs :

- une recherche peut être lancée pour produire un rapport ;
- aucune candidate n'est téléchargée sans approbation ou politique automatique
  explicitement autorisée ;
- les résultats indiquent indexeur, seeders, taille, qualité, score, Custom
  Formats et motifs de rejet ;
- les résultats sont triés selon la décision ARR et le gain estimé, pas selon un
  simple ordre de codecs.

### Imports et files d'activité

Une entrée bleue ou en erreur dans `/activity/queue` ne prouve pas l'absence du
hardlink. Les causes rencontrées incluent :

- `Series title mismatch` ;
- série inconnue ;
- numérotation absolue différente de TVDB ;
- import manuel requis ;
- release non considérée comme amélioration ;
- plusieurs cross-seeds du même chemin présentés comme nouveaux téléchargements ;
- permissions du dossier parent ;
- entrée de file devenue obsolète alors que l'import est déjà correct.

Avant de corriger les droits ou de relancer un import, vérifier les inodes
réels. Une entrée de file obsolète peut être retirée sans supprimer le média.

### Concurrence

Ne pas laisser Sonarr importer automatiquement pendant qu'un script crée les
mêmes hardlinks. Choisir une seule méthode :

- laisser Sonarr terminer l'import puis vérifier ; ou
- détacher temporairement la catégorie/tâche, effectuer l'import contrôlé, puis
  rescanner Sonarr.

## 3.10 Coordination Jellyfin

- Jellyfin ne détermine pas la qualité à conserver ; il expose la bibliothèque.
- Un rafraîchissement peut être demandé après un lot validé.
- Vérifier que le média est reconnu sous le bon film, la bonne série, saison et
  épisode.
- Les specials doivent utiliser une numérotation compatible avec l'agent de
  métadonnées choisi.
- Une organisation manuelle validée peut nécessiter de désactiver le monitoring
  ARR et/ou les mises à jour automatiques de métadonnées pour éviter un nouveau
  déplacement.
- La désactivation doit être ciblée et documentée, pas appliquée globalement.

## 3.11 Modes d'exécution

### `interactive`

- mode par défaut ;
- aucune présélection ;
- détail accessible avant sélection ;
- sélection individuelle, multiple et par plages ;
- confirmation renforcée si le remplacement est absent ou ambigu ;
- exécution uniquement après validation explicite.

### `dry-run`

- scan complet ;
- connexion et indexation en lecture seule des services nécessaires ;
- calcul des réparations et suppressions théoriques ;
- écriture des rapports et du plan ;
- aucune création de hardlink ;
- aucune suppression ;
- aucune pause, reprise, recheck, suppression de torrent ou grab ARR.

### `auto`

- aucun prompt interactif ;
- plan écrit avant mutation ;
- réparations exactes comparées intégralement ;
- nouveau scan après réparation ;
- suppression seulement si toutes les règles automatiques sont satisfaites ;
- ancienneté minimale historique de 48 heures ;
- validation de la qualité par Sonarr/Radarr ;
- blocage total si scan, API, mapping, authentification ou permissions ne sont
  pas fiables ;
- aucune décision autonome de qualité en l'absence d'ARR ;
- torrents vides retirés seulement si cette politique est activée et toujours
  avec `deleteFiles=false`.

Le score de similarité textuelle reste un mécanisme d'identification. Il ne doit
pas devenir un score de qualité ou remplacer les Custom Formats.

## 3.12 Revalidation juste avant action

Pour chaque cible, vérifier au minimum :

- existence ;
- type d'entrée ;
- absence de symlink ;
- confinement sous la racine ;
- appareil et inode ;
- taille ;
- `mtime_ns` ;
- mode ;
- état du dossier et de ses annexes ;
- absence d'erreur de scan sous la cible ;
- état des services externes et de leurs configurations pertinentes.

Toute différence invalide l'action, sans essayer de « s'adapter » silencieusement
au nouvel état.

## 3.13 Journalisation et durabilité

Avant une mutation :

1. écrire une entrée d'intention contenant la cible, l'état observé, la preuve,
   le gain attendu et les services concernés ;
2. vider les buffers ;
3. appeler `fsync` sur le fichier ;
4. si nécessaire, synchroniser le dossier parent.

Après mutation, écrire :

- succès ou erreur ;
- état réel final ;
- inode final ;
- hashes qBittorrent ;
- identifiants ARR ;
- mesure de stockage ;
- action compensatoire éventuelle.

Un plan doit avoir un identifiant stable et être immuable. Une reprise doit lire
le journal et déterminer quelles intentions n'ont pas encore de résultat.

## 3.14 Mesure du gain

Le bilan présente toujours :

- espace supprimé logiquement ;
- gain attribuable aux suppressions ;
- gain attribuable aux consolidations par hardlink ;
- coût des nouvelles données ajoutées ;
- gain net attribuable ;
- espace libre avant ;
- espace libre après mutation locale ;
- espace libre après opérations qBittorrent ;
- variation observée ;
- nombre de fichiers, dossiers, liens et torrents concernés.

Une consolidation libère les blocs de l'ancienne copie Jellyfin uniquement si
aucun autre hardlink ne les retient. Une suppression sous `completed` ne libère
rien si le même inode a encore un lien ailleurs.

## 3.15 Gestion des erreurs et interruptions

- Scanner en lecture seule reste autorisé si certains services sont indisponibles,
  mais le résultat doit être qualifié d'incomplet.
- Une mutation automatique est interdite après la moindre erreur affectant son
  périmètre.
- En cas d'interruption, reprendre les torrents qui n'ont subi aucune mutation.
- Conserver arrêtés les torrents modifiés jusqu'à un recheck réussi.
- Ne pas masquer une erreur en supprimant une entrée de file.
- Un timeout de recheck ne signifie pas automatiquement que les données sont
  mauvaises ; il impose un état manuel ou une nouvelle observation.
- Une erreur `ENOENT` pendant un scan peut provenir d'un téléchargement ou d'un
  déplacement concurrent. Elle doit protéger la zone et conduire à un nouveau
  scan, pas à une suppression.

---

# 4. Historique des itérations et ajustements

## 4.1 Première formulation

**État historique :** l'hypothèse initiale était que tous les MKV de
`completed` et `medias` devaient avoir un nombre de liens égal à deux. Le besoin
portait sur :

- les tailles des deux arbres ;
- la liste des fichiers à un seul lien ;
- la suppression interactive ;
- la conservation d'une trace des suppressions ;
- le nettoyage ultérieur des torrents qBittorrent.

L'analyse a montré que `st_nlink` ne suffisait pas. La règle a été remplacée par
la présence du même inode dans les deux racines.

## 4.2 Première architecture robuste

Le script unique a adopté :

- `os.scandir` ;
- l'identité `(st_dev, st_ino)` ;
- des rapports JSON/CSV/JSONL/TXT ;
- une interface paginée ;
- des contrôles de confinement ;
- un snapshot de l'état avant suppression ;
- l'intégration qBittorrent avec mapping Docker ;
- les mesures attribuables et observées.

## 4.3 Problème de performance sur environ 20 TiB

Les premières comparaisons complètes entre gros fichiers étaient trop lentes.
L'approche a évolué vers un entonnoir :

1. index des inodes ;
2. index `nom + taille` ;
3. parsing sémantique ;
4. échantillonnage facultatif ;
5. comparaison intégrale seulement pour la paire finalement retenue.

Cette évolution conserve la véracité de la preuve finale sans lire plusieurs
fois l'ensemble des 20 TiB.

## 4.4 Apparition de la réparation de hardlinks

Le besoin a évolué : un fichier à un seul lien n'est pas nécessairement inutile.
Une copie identique peut déjà être présente dans Jellyfin sous un inode séparé.
Le script a donc appris à proposer une consolidation plutôt qu'une suppression.

La direction validée est `completed → medias`. L'ancienne copie Jellyfin n'est
remplacée qu'après égalité intégrale.

## 4.5 Erreur de permission après comparaison longue

Plusieurs fichiers de plusieurs GiB ont été comparés intégralement, puis la
création du lien temporaire dans `medias` a échoué avec
`[Errno 1] Operation not permitted`.

La correction de conception a été de tester la permission de créer un hardlink
**avant** la lecture intégrale. L'utilisation de `sudo` peut rendre une opération
possible, mais ne remplace pas une configuration cohérente des propriétaires,
groupes et ACL des conteneurs.

## 4.6 Authentification qBittorrent et versions 5.x

L'URL par défaut du script ne correspondait pas à l'instance réelle. Une réponse
d'authentification vide a d'abord été interprétée comme un refus. Les tests ont
ensuite couvert le comportement qBittorrent 5.2, notamment les réponses HTTP 204,
ainsi que les différences `pause`/`stop` entre versions.

La leçon durable est de tester séparément :

- joignabilité ;
- authentification ;
- version ;
- récupération d'une liste de torrents ;
- mapping de chemins.

## 4.7 Interface et compréhension des commandes

L'interface initiale était compacte mais peu explicite (`n/p`, `s`, `u`, `d`,
`x`, `q`). La future application doit fournir libellés, aide contextuelle, compte
de sélection, aperçu du gain et explication de chaque conséquence.

## 4.8 Mode automatique prudent

L'utilisateur a demandé une automatisation. Trois modes ont été séparés :

- interaction manuelle ;
- simulation sans mutation ;
- automatisation prudente.

Le mode automatique a d'abord utilisé un ensemble de critères locaux : âge,
similarité, épisode/année, résolution et langue. L'expérience ultérieure avec
Sonarr/Radarr a précisé que les critères de **qualité** ne doivent plus être une
politique locale : ARR doit fournir la décision via profils et Custom Formats.

Les critères locaux restent nécessaires pour l'identité, la sûreté des chemins,
la stabilité du fichier et la compatibilité structurelle.

## 4.9 Scan incomplet et concurrence

Des dossiers ont disparu pendant un scan avec `ENOENT`. Le mode automatique a
correctement été bloqué. Un nouveau scan sans erreur a ensuite permis de
continuer.

**Règle durable :** un fichier qui disparaît pendant l'analyse indique une
concurrence avec qBittorrent, Sonarr ou un autre processus. L'automatisation doit
attendre un état stable.

## 4.10 Passage du nettoyage générique aux opérations ARR

Après les premiers gains, le reliquat n'était plus composé de simples doublons
exacts. Il fallait répondre à des questions de domaine :

- quel film ou épisode est concerné ?
- une autre release existe-t-elle ?
- est-elle complète ?
- est-elle une amélioration selon Sonarr/Radarr ?
- quelle langue et quelle édition faut-il conserver ?
- pourquoi l'import est-il en erreur ?

Des scripts spécialisés ont alors été créés. Ils ont démontré la faisabilité des
connecteurs et workflows, mais ont aussi révélé le besoin d'une application
unifiée.

## 4.11 Incidents représentatifs devenus règles

### Jujutsu Kaisen — numérotation absolue

Le pack utilisait `S02E25` à `S02E47`, alors que Sonarr attendait `S02E01` à
`S02E23`. Les épisodes ont dû être remappés vers la numérotation TVDB/Jellyfin.

**Leçon :** le nom scene ne suffit pas. L'application doit gérer la numérotation
absolue, aired et TVDB, avec un mapping explicite et vérifiable.

### Marvel's The Punisher — permissions

Les 26 épisodes étaient déjà correctement hardlinkés, mais un dossier de saison
appartenait à l'UID 1026. Sonarr en UID 1030 ne pouvait pas modifier les noms.

**Leçon :** vérifier d'abord les inodes, puis les permissions du dossier parent.
Une alerte d'import n'implique pas que le contenu n'a pas été importé.

### Love Hina — course entre deux importeurs

Sonarr a commencé son import automatique pendant que le script créait les
hardlinks. Des noms redondants ont été produits pour certains épisodes.

**Leçon :** un seul acteur doit posséder la transaction d'import. Love Hina a
également montré le besoin de mapper les specials : épisodes réguliers, E25 et
spéciaux Noël/Printemps doivent suivre la numérotation reconnue.

### Lupin — parties Netflix et saisons ARR

Les fichiers ont été déplacés ou renumérotés à cause d'une divergence entre les
parties Netflix, la structure de dossiers et les saisons attendues par Sonarr.

**Leçon :** le modèle canonique doit venir de l'identifiant de série et des
épisodes ARR. Une organisation manuelle doit être protégée contre les futurs
rescans ou imports automatiques concurrents.

### McFarland — restauration depuis Jellyfin

qBittorrent signalait des fichiers manquants alors que le film complet existait
dans Jellyfin. La source attendue a été restaurée par hardlink, puis le torrent a
été rechecké et réannoncé sans retélécharger le film de 9,84 GiB.

**Leçon :** une relation inverse Jellyfin → source qBittorrent peut réparer un
torrent, à condition de prouver taille, contenu et chemin attendu.

### La face cachée de la scientologie — audio réel

Le nom de release ne suffisait pas à connaître la langue. `ffprobe` a confirmé
que certains épisodes 1080p ne contenaient qu'une piste anglaise ; des versions
720p françaises ont été préférées pour ces épisodes.

**Leçon :** la langue audio réelle peut être plus importante que la résolution.
Les tags `und` nécessitent une prudence particulière.

### The Handmaid's Tale S05 — remplacement H265

Les dix épisodes H265 CHiLL ont été importés et vérifiés par inode avant le
retrait des anciennes sources H264.

**Leçon :** un remplacement de saison est une transaction globale ; chaque
épisode doit être prouvé avant le retrait de l'ancienne couverture.

### Consolidation de copies exactes

Des paires comme *Black 47*, *Meg 2*, *Apocalypse Z* et *Astérix* ont été
comparées intégralement puis consolidées. Le nettoyage exact du 2 novembre a
libéré 33,70 GiB sans nouveau `missingFiles`.

**Leçon :** les copies exactes constituent le cas le plus automatisable si la
transaction qBittorrent et les permissions sont maîtrisées.

### Avatar / cross-seed — torrent hybride refusé

qBittorrent répondait à l'ajout mais ne créait aucune tâche pour deux torrents
hybrides précis. Ni le changement de `skipRecheck`, ni le redémarrage, ni un test
cross-seed v7 n'ont corrigé le cas.

**Leçon :** un échec d'injection peut venir de la compatibilité des métadonnées
torrent/libtorrent, pas du stockage ou des identifiants. La bonne réponse peut
être une quarantaine et une blocklist, pas une boucle de retries.

## 4.12 Évolution des préférences qualité

Au cours des opérations, l'utilisateur a souvent préféré H265/x265 ou AV1 à
H264/x264 et a souhaité pénaliser certains groupes. Cette préférence a d'abord
été appliquée au cas par cas. La décision finale est de centraliser ces choix
dans les Custom Formats et profils de Sonarr/Radarr.

Une future application doit donc expliquer :

- pourquoi une release obtient un score ;
- quel Custom Format correspond ;
- pourquoi ARR l'accepte ou la rejette ;
- si l'amélioration atteint le cutoff ;
- si une dérogation manuelle est demandée.

Elle ne doit pas réimplémenter ces préférences avec des constantes.

---

# 5. État consolidé et résultats

## 5.1 Précaution de lecture des gains

Les rapports ont été produits par plusieurs générations de scripts et certains
lots se recouvrent. Il est incorrect d'additionner sans contrôle :

- un cumul interne du moteur ;
- le bilan d'un lot manuel ;
- un rapport final qui inclut déjà ce lot ;
- une variation libre mesurée sur la même période.

Deux nombres ne sont comparables que si leur fenêtre temporelle et leur ensemble
d'actions sont disjoints.

## 5.2 Lots documentés

| Date / lot | Gain attribuable documenté | Variation observée | Remarques |
|---|---:|---:|---|
| 25 oct. 2026 — premiers modes automatiques | cumul interne jusqu'à 565,34 GiB dans la sortie de session | variable | Cumul du moteur, à ne pas réadditionner à ses propres sessions antérieures. |
| 27 oct. 2026 — nettoyage NAS | 312,42 GiB | 312,42 GiB | 179 hardlinks, 265 fichiers supprimés, 39 tâches retirées. |
| 2 nov. 2026 — nettoyage exact | 33,70 GiB | 33,70 GiB | Quatre consolidations exactes et retrait ciblé de The Other Woman. |
| 3 nov. 2026 — lot approuvé principal | minimum 699,26 GiB | 851,11 GiB | 57 hardlinks, 177 imports vérifiés, au moins 158 anciens fichiers retirés. |
| 3 nov. 2026 — imports et nettoyage suivant | 213,84 GiB | 198,02 GiB | Punisher/Jujutsu/Love Hina, lot ciblé et Scientologie. |
| 3 nov. 2026 — nettoyage final / Handmaid | 33,35 GiB net | proche du gain attribuable pour la partie H264 | 41,10 GiB supprimés, moins 7,75 GiB H265 ajoutés. |

Ces valeurs sont des sous-totaux documentés. Certaines opérations du 3 novembre
sont des continuations d'inventaires et doivent être rapprochées de leurs
journaux avant de produire un chiffre comptable unique.

**État historique de conversation :** un ordre de grandeur consolidé d'environ
2,48 TiB attribuables et une hausse libre observée d'environ 3,62 TiB ont été
évoqués après l'ensemble des nettoyages. Ces valeurs sont utiles comme bilan
humain, mais ne doivent pas devenir une métrique de base de données sans refaire
la consolidation des journaux et éliminer les chevauchements.

**Exigence future :** chaque action doit avoir un identifiant unique de gain et
chaque bilan doit référencer les identifiants qu'il agrège. Cela rendra impossible
le double comptage.

## 5.3 Évolution des inventaires

Quelques jalons illustrent la réduction :

| Date | MKV `completed` sans inode Jellyfin | MKV Jellyfin sans inode `completed` |
|---|---:|---:|
| 2 nov. avant nettoyage exact | 227 / environ 875,71 GiB | 403 / environ 740,78 GiB |
| 2 nov. après nettoyage exact | 222 / 842,01 GiB | 403 / 740,78 GiB |
| 3 nov. après lot principal | 102 / 250,00 GiB | 245 / 413,13 GiB |
| 3 nov. audit intermédiaire | 18 / 11,48 GiB | 243 / 409,05 GiB |
| 3 nov. audit final | 10 / 811,10 MiB | 243 / 409,05 GiB |

## 5.4 Dernier état vérifié au 3 novembre 2026

### completed

- 10 MKV sans inode dans Jellyfin ;
- volume total : 811,10 MiB ;
- ce sont les samples explicitement protégés ;
- aucune suppression supplémentaire n'est autorisée sur la base de ce rapport.

Les samples concernent notamment *The Mummy*, *Robin Hood*, *Le Grand Blond*,
*Ratatouille*, *Monsters Inc.*, *Casablanca*, deux films du *Gendarme*, *The
Purge* et *Quantum of Solace*.

### Jellyfin

- 243 MKV sans inode présent dans `completed` ;
- volume total : 409,05 GiB ;
- 240 sont reliés à une entrée Sonarr ou Radarr et peuvent faire l'objet d'une
  recherche de release ;
- leur absence de source hardlinkée ne signifie pas qu'ils sont des doublons ou
  qu'ils doivent être supprimés.

Trois médias n'étaient pas rattachés à ARR dans le dernier audit :

1. `Thalassa, aventures extrêmes` — documentaire, environ 1,91 GiB ;
2. `The Real Oppenheimer (2023)` — documentaire, environ 1,84 GiB ;
3. `Prison Break S04E23-24 The Final Break` — environ 1,74 GiB.

### incomplete

**État actuel vérifié :** nettoyé et vide lors des audits finaux concernés. Un
nouveau téléchargement peut évidemment modifier cet état après le 3 novembre.

### qBittorrent

Dernier audit :

- version 5.3.0 ;
- `stoppedUP` : 0 ;
- `missingFiles` : 0 ;
- 62 seeds avaient été vérifiés puis repris lors du dernier nettoyage ARR ;
- McFarland était revenu à 100 %, données restantes 0, état `stalledUP`.

### Sonarr/Radarr

Dernier bilan :

- file Sonarr : 0 élément ;
- file Radarr : 0 élément ;
- les recherches de remplacement restantes n'ont pas à être considérées comme
  automatiquement approuvées ;
- les futurs choix doivent être recalculés avec les profils et Custom Formats au
  moment de la recherche.

### Jellyfin

- rafraîchissement complet accepté en HTTP 204 après les derniers lots ;
- les imports vérifiés partagent les inodes attendus ;
- l'état logique de reconnaissance doit néanmoins être revalidé lors d'un nouvel
  audit, car les métadonnées et rescans peuvent évoluer.

### cross-seed

- configuration stable restaurée ;
- nouveau tracker chargé après redémarrage ;
- environ 3 072 torrents indexés au moment du diagnostic ;
- file d'injection vidée ;
- deux torrents hybrides Avatar conservés en quarantaine ;
- infohash incompatible bloqué ;
- sauvegarde de diagnostic cross-seed v7 d'environ 518 MiB encore présente sur
  le volume 2.

### Système

- Docker utilise `/volume2/@docker`, pas le volume 3 ;
- les autres consommateurs visibles du volume 3 étaient négligeables lors de
  l'audit ;
- l'occupation des snapshots Btrfs n'a pas pu être lue avec le compte SSH ;
- aucun snapshot n'a été modifié ;
- l'occupation doit être consultée dans DSM Snapshot Replication.

## 5.5 Actions restantes

1. **Action restante :** analyser les 243 fichiers propres à Jellyfin avec les
   données ARR actuelles.
2. **Action restante :** pour les 240 médias gérés, rechercher les releases et
   afficher profils, Custom Formats, scores, cutoffs et motifs de rejet.
3. **Action restante :** traiter manuellement les trois médias non gérés ou les
   ajouter proprement à ARR si souhaité.
4. **Action restante :** vérifier l'espace occupé par les snapshots dans DSM.
5. **Action restante :** décider si la sauvegarde cross-seed v7 de 518 MiB doit
   être conservée.
6. **Action restante :** renouveler les secrets qui ont pu apparaître pendant les
   diagnostics.
7. **Action restante :** consolider les journaux historiques si un total de gain
   comptable exact est nécessaire.

---

# 6. Exigences pour la future application

## 6.1 Principe d'architecture neutre

Ce document ne choisit pas entre application Web locale, application desktop ou
interface terminal. Le domaine doit être indépendant de l'interface et exposer
des opérations explicites de lecture, planification, approbation, exécution et
vérification.

Une architecture en couches est recommandée :

```text
Interface utilisateur / API
          │
Orchestrateur de workflows et tâches longues
          │
Services de domaine (inodes, matching, plans, politiques, gains)
          │
Connecteurs (fichiers, qBittorrent, Sonarr, Radarr, Jellyfin, cross-seed)
          │
Stockage durable des audits, plans, journaux et exceptions
```

## 6.2 Capacités fonctionnelles

### Tableau de bord

- capacité, utilisé et libre ;
- taille logique et allouée des racines ;
- espace partagé par hardlinks ;
- potentiel à examiner, sans le présenter comme gain garanti ;
- santé et version de chaque service ;
- files Sonarr/Radarr ;
- états qBittorrent problématiques ;
- dernier scan complet et erreurs ;
- tâches longues en cours.

### Explorateur de stockage

- recherche par chemin, titre, épisode, inode ou hash ;
- affichage de tous les chemins d'un inode ;
- distinction copie physique/hardlink ;
- taille logique et blocs ;
- annexes ;
- provenance qBittorrent et ARR ;
- version Jellyfin actuelle.

### Inventaires bidirectionnels

- `completed` sans Jellyfin ;
- Jellyfin sans `completed` ;
- copies exactes non consolidées ;
- versions différentes d'un même média ;
- torrents sans fichiers ;
- fichiers sans torrent ;
- imports ARR en erreur ou obsolètes.

### Comparateur

- niveau de preuve physique, lexical, sémantique, échantillonné ou intégral ;
- métadonnées `ffprobe` ;
- couverture film/épisode ;
- version actuelle et candidates ;
- décision Sonarr/Radarr ;
- Custom Formats et détail des scores ;
- gain net estimé ;
- risques et éléments bloquants.

### Planificateur

- ajout/retrait explicite d'actions ;
- aucune sélection implicite en mode manuel ;
- aperçu des fichiers, torrents et médias touchés ;
- dépendances entre actions ;
- plan immuable signé par son hash ;
- expiration si l'état ou la configuration ARR change ;
- simulation complète.

### Exécuteur

- file de tâches persistante ;
- progression des comparaisons longues ;
- annulation sûre ;
- journal temps réel ;
- reprise après redémarrage ;
- compensation/restauration des états qBittorrent ;
- verrouillage pour empêcher deux imports concurrents du même média.

### Rapports

- synthèse humaine Markdown/HTML ;
- données structurées JSON/CSV ;
- preuve inode par inode ;
- gains attribuables et observés ;
- différences entre plan et résultat ;
- actions restantes ;
- export sans secrets.

## 6.3 Modèle de domaine minimal

### `AuditSession`

- identifiant ;
- dates de début/fin ;
- racines et exclusions ;
- versions des services ;
- état complet/incomplet ;
- erreurs ;
- métriques.

### `FilesystemEntry`

- chemin ;
- racine logique ;
- type ;
- taille et blocs ;
- `dev`, inode, nombre de liens ;
- mode, propriétaire, groupe ;
- dates ;
- signature de revalidation.

### `PhysicalIdentity`

- `(dev, ino)` ;
- ensemble de chemins ;
- blocs attribuables ;
- présence dans chaque racine.

### `MediaIdentity`

- type film/épisode/spécial ;
- identifiant Sonarr/Radarr ;
- identifiants externes ;
- titre, année, saison, épisode ;
- couverture.

### `TorrentIdentity`

- hash v1/v2 ;
- nom ;
- état ;
- catégorie ;
- save path ;
- fichiers demandés ;
- tracker masqué ou identifiant non secret.

### `ArrQualitySnapshot`

- service et version ;
- média ;
- profil ;
- cutoff ;
- upgrade autorisé ;
- Custom Formats et scores ;
- score total ;
- qualité ;
- décision et motifs ;
- date de capture.

### `MatchCandidate`

- source et destination ;
- média identifié ;
- niveau de preuve ;
- score d'identification ;
- comparaison de contenu ;
- métadonnées techniques ;
- état ARR ;
- risques.

### `ActionPlan`

- identifiant et hash ;
- auteur ou politique ;
- actions ordonnées ;
- préconditions ;
- preuves ;
- gains attendus ;
- instantanés ARR et qBittorrent ;
- statut d'approbation ;
- date d'expiration.

### `ActionJournal`

- intention ;
- synchronisation confirmée ;
- début/fin ;
- résultat ;
- erreur ;
- état avant/après ;
- compensation ;
- identifiant de gain unique.

### `Policy` et `Exception`

- règles de sécurité locales ;
- modes autorisés ;
- racines interdites ;
- protection des samples ;
- ancienneté minimale ;
- dérogations ciblées ;
- date, motif et auteur.

La politique de qualité n'est pas dupliquée ici : elle est référencée par
`ArrQualitySnapshot`.

## 6.4 Machine d'état des candidats

```text
observé
  └─→ identifié
        ├─→ ambigu / manuel
        └─→ candidat
              ├─→ rejeté
              └─→ planifié
                    ├─→ plan expiré
                    └─→ approuvé
                          ├─→ en exécution
                          │     ├─→ échec compensé
                          │     ├─→ échec à examiner
                          │     └─→ exécuté
                          └────────────→ vérifié → clôturé
```

Une action n'est « réussie » qu'après vérification. Une mutation système sans
preuve finale reste « exécutée, non vérifiée ».

## 6.5 Exigences de sécurité applicative

- secrets stockés hors des rapports ;
- permissions minimales par connecteur ;
- mode lecture seule utilisable sans droits de mutation ;
- élévation de privilèges ciblée, jamais un processus entier lancé inutilement
  en root ;
- validation stricte des racines et mappings ;
- protection CSRF/authentification si interface réseau ;
- journal d'audit non modifiable depuis l'interface standard ;
- aucune commande shell construite par concaténation de chemins ;
- timeouts et limites de taille ;
- redaction des URLs de trackers ;
- sauvegarde et restauration de la configuration avant modification de service.

## 6.6 Intégration ARR : contrat fonctionnel

Le connecteur doit pouvoir lire :

- séries, films, épisodes et fichiers ;
- monitoring ;
- profils de qualité ;
- Custom Formats ;
- scores du profil ;
- files d'activité et messages ;
- historique de téléchargement/import ;
- recherches interactives ;
- décisions de rejet ;
- commandes de rescan et leur état.

Les appels mutateurs suivants doivent être séparément autorisés :

- grab d'une release ;
- import manuel ;
- modification du monitoring ;
- modification de métadonnées ;
- suppression d'une entrée de file ;
- modification d'un profil ou Custom Format.

La lecture d'une recherche ne donne jamais implicitement le droit de grab.

## 6.7 Tests d'acceptation de la future application

### Système de fichiers

- deux chemins même inode reconnus comme hardlink ;
- `nlink=2` avec lien externe refusé comme preuve Jellyfin ;
- symlink jamais suivi ;
- changement après scan bloque l'action ;
- erreur dans un sous-arbre bloque ce sous-arbre ;
- `vrac` n'est jamais ouvert, même par un audit global ;
- calcul de blocs exact avec plusieurs liens.

### Matching

- même nom/taille reste un candidat non prouvé ;
- échantillons positifs ne court-circuitent pas la comparaison intégrale ;
- éditions différentes refusées ;
- épisodes et multi-épisodes correctement couverts ;
- numérotation absolue mappée explicitement ;
- langue réelle `ffprobe` peut bloquer un remplacement.

### ARR

- profil et Custom Formats lus et archivés ;
- candidate rejetée par ARR non sélectionnée automatiquement ;
- changement de score invalide un ancien plan ;
- absence d'ARR force la décision manuelle ;
- aucune hiérarchie codec interne n'écrase le score ARR ;
- recherche interactive garantie sans grab.

### qBittorrent

- support versions 4.x et 5.x ;
- réponse 204 d'authentification acceptée ;
- mapping hors racine refusé ;
- `deleteFiles=false` vérifié ;
- torrent partiel laissé arrêté ;
- torrent intact repris après interruption ;
- tâche retirée seulement si tous ses fichiers planifiés ont disparu.

### Workflows

- `dry-run` sans mutation de fichiers ni service ;
- intention durable avant mutation ;
- reprise après crash entre intention et résultat ;
- hardlink temporaire puis remplacement atomique ;
- gain net calculé pour un remplacement ;
- suppression de dossier tout ou rien ;
- samples protégés ;
- deux imports concurrents du même média interdits.

---

# 7. Risques, dette technique et stratégie d'évolution

## 7.1 Risques actuels

1. **Scripts redondants :** plusieurs implémentations des mêmes connecteurs et
   contrôles peuvent diverger.
2. **Chemins codés en dur :** utiles pour une opération ponctuelle, dangereux
   pour une application générique.
3. **Secrets lus depuis les configurations :** pratique en diagnostic, mais à
   remplacer par une gestion dédiée.
4. **Concurrence :** qBittorrent, Sonarr, Radarr, Jellyfin et scripts peuvent
   modifier simultanément les mêmes chemins.
5. **Noms ambigus :** release, titre Jellyfin et titre TVDB/TMDB peuvent différer.
6. **Langues mal taguées :** `und`, nom de release et piste réelle peuvent se
   contredire.
7. **Évolution des scores ARR :** une décision peut devenir obsolète après une
   modification de Custom Format.
8. **Snapshots :** un fichier supprimé peut rester physiquement retenu par Btrfs.
9. **Fichiers ouverts :** l'espace peut ne pas être rendu immédiatement.
10. **Torrents hybrides :** compatibilité variable entre clients/libtorrent.
11. **Permissions/ACL Synology :** le mode POSIX affiché ne montre pas toujours
    toute la politique ACL.
12. **Métadonnées automatiques :** un rescan peut renommer ou déplacer un média
    organisé manuellement.

## 7.2 Stratégie de migration recommandée

1. Extraire une bibliothèque de lecture seule pour fichiers, inodes et mappings.
2. Unifier les connecteurs qBittorrent et ARR.
3. Importer les rapports existants comme historique, sans les traiter comme état
   courant.
4. Créer un modèle persistant de sessions, plans et journaux.
5. Reproduire d'abord les audits actuels dans la nouvelle application.
6. Ajouter le `dry-run` et comparer ses résultats au script historique.
7. Ajouter les consolidations exactes, cas le plus sûr.
8. Ajouter les remplacements ARR après validation des scores et cutoffs.
9. Ajouter l'automatisation seulement après tests de crash, concurrence et
   reprise.
10. Conserver les anciens scripts comme outils de secours jusqu'à parité prouvée.

## 7.3 Observabilité nécessaire

- durée de scan ;
- débit de comparaison ;
- nombre d'erreurs par racine ;
- appels et erreurs par API ;
- temps de recheck ;
- plans expirés ;
- actions compensées ;
- gains attendus/réels ;
- torrents laissés arrêtés ;
- imports ARR non vérifiés ;
- changements de configuration ARR depuis le dernier plan.

---

# 8. Sources, fichiers de référence et glossaire

## 8.1 Sources principales

### Code

- `doublon_checker.py` — moteur générique actuel ;
- `test_doublon_checker.py` — 32 tests et comportement attendu ;
- `generate_final_audit.py` — corrélation fichiers, qBittorrent et bases ARR ;
- scripts `audit_*`, `execute_*`, `finalize_*`, `fix_*`, `search_*` — scénarios
  opérationnels et dette à consolider.

### Rapports prioritaires

- `analyse/20261103T214200Z/rapport-final.md` — dernier bilan général ;
- `analyse/20261103T214200Z/cross-seed-qbittorrent-5.3.md` — diagnostic
  cross-seed/qBittorrent ;
- `analyse/20261103T214200Z/final-readable-audit-20261103T214112Z/rapport.md` —
  dernier inventaire prudent ;
- `analyse/20261103T214200Z/final-readable-audit-20261103T214112Z/jellyfin-sans-source-completed.csv`
  — 243 fichiers Jellyfin à examiner ;
- `analyse/20261103T202100Z/rapport-final.md` — imports et corrections ARR ;
- `analyse/20261103T002611Z/rapport-final.md` — lot approuvé principal ;
- `analyse/20261102T195357Z/01-nettoyage-exact.md` — consolidations exactes ;
- `analyse/20261102T195357Z/04-audit-systeme.md` — volume, Docker et snapshots ;
- `rapport_nettoyage_2026-10-27.md` — lot manuel antérieur.

### Résultats structurés utiles

- `analyse/20261103T002611Z/approved-batch-result-20261102T231157Z.json` ;
- `analyse/20261103T202100Z/user-batch-20261103-result-20261103T200943Z.json` ;
- `analyse/20261103T202100Z/queue-import-fix-result-20261103T200658Z.json` ;
- `analyse/20261103T202100Z/love-hina-finalize-result-20261103T201415Z.json` ;
- `analyse/20261103T202100Z/scientology-french-replacement-result-20261103T201543Z.json` ;
- `analyse/20261103T214200Z/arr-queue-cleanup-result-20261103T212634Z.json` ;
- `analyse/20261103T214200Z/handmaid-s05-h265-finalize-result-20261103T213900Z.json`.

## 8.2 Glossaire

**ARR**  
Nom collectif utilisé ici pour Sonarr et Radarr.

**Blocs alloués**  
Espace physique attribué par le système de fichiers à un inode. Plus pertinent
que la somme logique pour mesurer l'espace récupérable.

**Candidate**  
Fichier ou release qui pourrait correspondre à un autre média. Une candidate
n'est pas une preuve.

**Custom Format**  
Règle Sonarr/Radarr attribuant un score à des caractéristiques de release. C'est
le mécanisme retenu pour les préférences de codec, langue, groupe et autres
attributs.

**Cutoff**  
Niveau du profil ARR à partir duquel les upgrades peuvent cesser, selon sa
configuration.

**Gain attribuable**  
Espace dont la libération peut être reliée aux inodes effectivement libérés par
le plan.

**Gain observé**  
Variation de l'espace libre du volume. Peut inclure ou masquer d'autres
événements système.

**Hardlink**  
Nom supplémentaire pointant vers le même inode sur le même système de fichiers.

**Inode**  
Objet du système de fichiers portant les données et métadonnées physiques d'un
fichier. Deux chemins sont hardlinkés s'ils partagent appareil et inode.

**`missingFiles`**  
État qBittorrent indiquant que des fichiers attendus ne sont pas accessibles à
leur chemin connu.

**Profil de qualité**  
Configuration Sonarr/Radarr définissant qualités autorisées, ordre, cutoff et
scores de Custom Formats pour un média.

**Recheck**  
Vérification par qBittorrent de toutes les pièces d'un torrent par rapport aux
données locales.

**Release scene/P2P**  
Nom structuré contenant généralement titre, année/épisode, langue, résolution,
source, codec et groupe.

**Sample**  
Court extrait livré avec certaines releases. Dans ce projet, il est protégé par
défaut et ne doit pas être traité comme une copie complète orpheline.

**`stalledUP`**  
Torrent complet prêt à uploader mais sans transfert actif, souvent faute de
peers. Ce n'est pas en soi une erreur.

**`stoppedUP`**  
Torrent complet arrêté. Peut être volontaire ou résulter d'une opération ; doit
être analysé avant suppression.

**Taille logique**  
Taille visible du fichier. Plusieurs hardlinks affichent chacun cette taille
mais partagent les mêmes blocs.

---

# 9. Directives finales à l'IA chargée de créer l'application

1. Commencer par modéliser les preuves, plans et états ; ne pas commencer par un
   bouton « supprimer ».
2. Considérer le scan comme une photographie horodatée pouvant expirer.
3. Ne jamais confondre identité de contenu, identité de média et préférence de
   qualité : ce sont trois problèmes distincts.
4. Utiliser l'inode pour les hardlinks, la comparaison intégrale pour les copies
   exactes, ARR pour la qualité et l'utilisateur pour les dérogations.
5. Faire de `dry-run` un chemin d'exécution réel qui emprunte toute la logique
   sauf les mutations, pas une simple option d'affichage.
6. Rendre toutes les mutations explicites, journalisées, reprenables et
   vérifiables.
7. Ne jamais parcourir `/volume3/chill/vrac`.
8. Ne jamais utiliser `deleteFiles=true` avec qBittorrent.
9. Ne jamais coder une préférence de codec ou groupe en parallèle des Custom
   Formats Sonarr/Radarr.
10. En cas de doute, conserver les données, produire un rapport lisible et
    demander une décision.

## Critères de réussite du futur produit

Le produit est acceptable lorsque :

- il reproduit les inventaires du script historique sur un jeu de test ;
- il explique pourquoi chaque fichier est partagé, unique ou candidat ;
- il peut simuler un lot complet sans aucune mutation ;
- il capture et affiche la décision qualité de Sonarr/Radarr ;
- il invalide un plan si les fichiers ou profils ont changé ;
- il consolide une copie exacte sans casser le seed ni Jellyfin ;
- il remplace une saison seulement après preuve de couverture complète ;
- il survit à une interruption entre journalisation et mutation ;
- il mesure le gain sans double comptage ;
- il produit un rapport compréhensible sans accès aux logs bruts ;
- il protège systématiquement les exclusions, samples, chemins ambigus et scans
  incomplets.

