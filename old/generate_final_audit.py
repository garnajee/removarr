#!/usr/bin/env python3
"""Generate a read-only, human-oriented NAS audit without scanning vrac."""

from __future__ import annotations

import ast
import csv
import difflib
import json
import os
import sqlite3
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath


COMPLETED_ROOT = Path("/volume3/chill/completed")
MEDIA_ROOT = Path("/volume3/chill/medias")
REPORT_ROOT = Path("/volume3/chill/doublon-checker-reports")
EXCLUDED_ROOT = Path("/volume3/chill/vrac")
SCRIPT_PATH = Path("/volume3/chill/doublon_checker.py")
SONARR_DB = Path("/volume2/docker/chill/sonarr/config/sonarr.db")
RADARR_DB = Path("/volume2/docker/chill/radarr/config/radarr.db")


def human_size(value: int) -> str:
    size = float(value)
    units = ("B", "KiB", "MiB", "GiB", "TiB")
    for unit in units:
        if abs(size) < 1024 or unit == units[-1]:
            return f"{size:.2f} {unit}" if unit != "B" else f"{int(size)} B"
        size /= 1024
    return f"{size:.2f} TiB"


def atomic_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with temporary.open("w", encoding="utf-8", newline="") as handle:
        handle.write(content)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def atomic_csv(path: Path, fields: list[str], rows: list[dict[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with temporary.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def literal_configuration(path: Path) -> dict[str, object]:
    tree = ast.parse(path.read_text(encoding="utf-8"), str(path))
    values: dict[str, object] = {}
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            try:
                values[node.targets[0].id] = ast.literal_eval(node.value)
            except (TypeError, ValueError):
                pass
    return values


def connect_readonly(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    return connection


def host_path(path: str) -> str:
    normalized = str(PurePosixPath(path))
    if normalized == "/downloads":
        return "/volume3/chill"
    if normalized.startswith("/downloads/"):
        return "/volume3/chill/" + normalized[len("/downloads/"):]
    if normalized == "/data":
        return "/volume3/chill"
    if normalized.startswith("/data/"):
        return "/volume3/chill/" + normalized[len("/data/"):]
    return normalized


def title_score(tokens: tuple[str, ...], *titles: str) -> float:
    left = " ".join(tokens).casefold()
    if not left:
        return 0.0
    return max(
        (difflib.SequenceMatcher(None, left, title.casefold()).ratio()
         for title in titles if title),
        default=0.0,
    )


def load_arr(dc):
    sonarr = connect_readonly(SONARR_DB)
    radarr = connect_readonly(RADARR_DB)

    series: dict[int, dict[str, object]] = {}
    for row in sonarr.execute(
        "SELECT Id, Title, CleanTitle, Path, Monitored, Year FROM Series"
    ):
        series[int(row["Id"])] = dict(row)
    episode_files = {
        int(row["Id"]): dict(row)
        for row in sonarr.execute(
            "SELECT Id, SeriesId, RelativePath, Size, SceneName, MediaInfo, "
            "Languages, Quality FROM EpisodeFiles"
        )
    }
    episodes: dict[int, dict[str, object]] = {}
    series_episodes: dict[tuple[int, int, int], dict[str, object]] = {}
    sonarr_path_map: dict[str, dict[str, object]] = {}
    for row in sonarr.execute(
        "SELECT Id, SeriesId, SeasonNumber, EpisodeNumber, EpisodeFileId, "
        "Title, Monitored FROM Episodes"
    ):
        value = dict(row)
        episode_id = int(value["Id"])
        episodes[episode_id] = value
        key = (
            int(value["SeriesId"]), int(value["SeasonNumber"]),
            int(value["EpisodeNumber"]),
        )
        series_episodes[key] = value
        file_id = int(value["EpisodeFileId"] or 0)
        if file_id and file_id in episode_files:
            series_row = series[int(value["SeriesId"])]
            path = str(
                PurePosixPath(str(series_row["Path"]))
                / str(episode_files[file_id]["RelativePath"])
            )
            sonarr_path_map[host_path(path).casefold()] = {
                "arr_type": "sonarr",
                "arr_id": int(series_row["Id"]),
                "arr_title": str(series_row["Title"]),
                "arr_monitored": bool(series_row["Monitored"]),
                "episode_id": episode_id,
                "season": int(value["SeasonNumber"]),
                "episode": int(value["EpisodeNumber"]),
                "current_media_path": host_path(path),
            }

    sonarr_history: dict[str, list[dict[str, object]]] = defaultdict(list)
    for row in sonarr.execute(
        "SELECT Id, EpisodeId, SeriesId, SourceTitle, Date, EventType, "
        "DownloadId FROM History WHERE DownloadId IS NOT NULL AND DownloadId != ''"
    ):
        sonarr_history[str(row["DownloadId"]).casefold()].append(dict(row))

    movies: dict[int, dict[str, object]] = {}
    radarr_path_map: dict[str, dict[str, object]] = {}
    for row in radarr.execute(
        "SELECT m.Id, m.Path, m.Monitored, m.MovieFileId, mm.Title, "
        "mm.OriginalTitle, mm.CleanTitle, mm.CleanOriginalTitle, mm.Year "
        "FROM Movies m JOIN MovieMetadata mm ON mm.Id=m.MovieMetadataId"
    ):
        value = dict(row)
        movie_id = int(value["Id"])
        movies[movie_id] = value
    movie_files = {
        int(row["Id"]): dict(row)
        for row in radarr.execute(
            "SELECT Id, MovieId, RelativePath, Size, SceneName, MediaInfo, "
            "Languages, Quality, Edition FROM MovieFiles"
        )
    }
    for movie_id, value in movies.items():
        file_id = int(value["MovieFileId"] or 0)
        if file_id and file_id in movie_files:
            path = str(
                PurePosixPath(str(value["Path"]))
                / str(movie_files[file_id]["RelativePath"])
            )
            radarr_path_map[host_path(path).casefold()] = {
                "arr_type": "radarr",
                "arr_id": movie_id,
                "arr_title": str(value["Title"]),
                "arr_monitored": bool(value["Monitored"]),
                "episode_id": "",
                "season": "",
                "episode": "",
                "current_media_path": host_path(path),
            }

    radarr_history: dict[str, list[dict[str, object]]] = defaultdict(list)
    for row in radarr.execute(
        "SELECT Id, SourceTitle, Date, EventType, DownloadId, MovieId "
        "FROM History WHERE DownloadId IS NOT NULL AND DownloadId != ''"
    ):
        radarr_history[str(row["DownloadId"]).casefold()].append(dict(row))

    return {
        "series": series,
        "episodes": episodes,
        "series_episodes": series_episodes,
        "episode_files": episode_files,
        "sonarr_history": sonarr_history,
        "sonarr_path_map": sonarr_path_map,
        "movies": movies,
        "movie_files": movie_files,
        "radarr_history": radarr_history,
        "radarr_path_map": radarr_path_map,
    }


def arr_from_history(parsed, hashes: list[str], data) -> dict[str, object] | None:
    sonarr_rows = [
        row for value in hashes
        for row in data["sonarr_history"].get(value.casefold(), [])
    ]
    if sonarr_rows:
        candidates = []
        for row in sonarr_rows:
            episode = data["episodes"].get(int(row["EpisodeId"] or 0))
            if not episode:
                continue
            if parsed.episode and (
                int(episode["SeasonNumber"]), int(episode["EpisodeNumber"])
            ) != parsed.episode:
                continue
            candidates.append((row, episode))
        if candidates:
            row, episode = sorted(candidates, key=lambda item: str(item[0]["Date"]))[-1]
            series = data["series"][int(episode["SeriesId"])]
            file_id = int(episode["EpisodeFileId"] or 0)
            media_path = ""
            if file_id in data["episode_files"]:
                media_path = host_path(str(
                    PurePosixPath(str(series["Path"]))
                    / str(data["episode_files"][file_id]["RelativePath"])
                ))
            return {
                "arr_type": "sonarr", "arr_id": int(series["Id"]),
                "arr_title": str(series["Title"]),
                "arr_monitored": bool(series["Monitored"]),
                "episode_id": int(episode["Id"]),
                "season": int(episode["SeasonNumber"]),
                "episode": int(episode["EpisodeNumber"]),
                "current_media_path": media_path,
                "match_method": "historique_qbittorrent_sonarr",
                "match_score": 1.0,
            }

    radarr_rows = [
        row for value in hashes
        for row in data["radarr_history"].get(value.casefold(), [])
        if int(row["MovieId"] or 0) in data["movies"]
    ]
    if radarr_rows:
        row = sorted(radarr_rows, key=lambda item: str(item["Date"]))[-1]
        movie = data["movies"][int(row["MovieId"])]
        file_id = int(movie["MovieFileId"] or 0)
        media_path = ""
        if file_id in data["movie_files"]:
            media_path = host_path(str(
                PurePosixPath(str(movie["Path"]))
                / str(data["movie_files"][file_id]["RelativePath"])
            ))
        return {
            "arr_type": "radarr", "arr_id": int(movie["Id"]),
            "arr_title": str(movie["Title"]),
            "arr_monitored": bool(movie["Monitored"]),
            "episode_id": "", "season": "", "episode": "",
            "current_media_path": media_path,
            "match_method": "historique_qbittorrent_radarr",
            "match_score": 1.0,
        }
    return None


def arr_from_name(parsed, data) -> dict[str, object] | None:
    if parsed.episode:
        season, episode_number = parsed.episode
        scored = []
        for series_id, series in data["series"].items():
            episode = data["series_episodes"].get(
                (series_id, season, episode_number)
            )
            if episode is None:
                continue
            score = title_score(
                parsed.title_tokens,
                str(series["Title"]), str(series["CleanTitle"]),
            )
            scored.append((score, series, episode))
        if scored:
            score, series, episode = max(scored, key=lambda item: item[0])
            if score >= 0.72:
                file_id = int(episode["EpisodeFileId"] or 0)
                media_path = ""
                if file_id in data["episode_files"]:
                    media_path = host_path(str(
                        PurePosixPath(str(series["Path"]))
                        / str(data["episode_files"][file_id]["RelativePath"])
                    ))
                return {
                    "arr_type": "sonarr", "arr_id": int(series["Id"]),
                    "arr_title": str(series["Title"]),
                    "arr_monitored": bool(series["Monitored"]),
                    "episode_id": int(episode["Id"]), "season": season,
                    "episode": episode_number, "current_media_path": media_path,
                    "match_method": "nom_episode_sonarr",
                    "match_score": round(score, 4),
                }
    elif parsed.year:
        scored = []
        for movie in data["movies"].values():
            if int(movie["Year"] or 0) not in {parsed.year - 1, parsed.year, parsed.year + 1}:
                continue
            score = title_score(
                parsed.title_tokens, str(movie["Title"]),
                str(movie["OriginalTitle"]), str(movie["CleanTitle"]),
                str(movie["CleanOriginalTitle"]),
            )
            scored.append((score, movie))
        if scored:
            score, movie = max(scored, key=lambda item: item[0])
            if score >= 0.72:
                file_id = int(movie["MovieFileId"] or 0)
                media_path = ""
                if file_id in data["movie_files"]:
                    media_path = host_path(str(
                        PurePosixPath(str(movie["Path"]))
                        / str(data["movie_files"][file_id]["RelativePath"])
                    ))
                return {
                    "arr_type": "radarr", "arr_id": int(movie["Id"]),
                    "arr_title": str(movie["Title"]),
                    "arr_monitored": bool(movie["Monitored"]),
                    "episode_id": "", "season": "", "episode": "",
                    "current_media_path": media_path,
                    "match_method": "nom_annee_radarr",
                    "match_score": round(score, 4),
                }
    return None


def qbit_file_index(dc, known_paths: set[str]):
    values = literal_configuration(SCRIPT_PATH)
    config = dc.Config(
        completed_path=Path(str(values["COMPLETED_PATH"])),
        media_path=Path(str(values["MEDIA_PATH"])),
        report_path=Path(str(values["REPORT_PATH"])),
        qbittorrent_url=str(values["QBITTORRENT_URL"]),
        qbittorrent_username=str(values["QBITTORRENT_USERNAME"]),
        qbittorrent_password=str(values["QBITTORRENT_PASSWORD"]),
        qbittorrent_container_path=str(values["QBITTORRENT_CONTAINER_PATH"]),
        qbittorrent_host_path=Path(str(values["QBITTORRENT_HOST_PATH"])),
        video_extensions=frozenset({".mkv"}), similarity_threshold=0.60,
        recheck_timeout_seconds=600, quick_sample_size=1024 * 1024,
        quick_sample_count=5, full_compare_chunk_size=16 * 1024 * 1024,
        search_similar_names_for_hardlink=False,
    )
    config = dc.validate_configuration(config)
    client = dc.QbitClient(
        config.qbittorrent_url, config.qbittorrent_username,
        config.qbittorrent_password,
    )
    client.login()
    index: dict[str, list[dict[str, object]]] = defaultdict(list)
    torrents = client.torrents_info()
    for torrent in torrents:
        torrent_hash = str(torrent.get("hash", ""))
        files = client.torrent_files(torrent_hash)
        for item in files:
            try:
                path = dc.choose_qbit_file_path(
                    torrent, item, config, known_paths, len(files)
                )
            except dc.QbitError:
                continue
            key = dc.lexical_key(path)
            if key in known_paths:
                index[key].append({
                    "hash": torrent_hash,
                    "name": str(torrent.get("name", "")),
                    "state": str(torrent.get("state", "")),
                    "category": str(torrent.get("category", "")),
                    "progress": float(torrent.get("progress", 0.0)),
                    "file_progress": float(item.get("progress", 0.0)),
                })
    return client.version, torrents, index


def join(values, field: str) -> str:
    return " | ".join(dict.fromkeys(str(item.get(field, "")) for item in values))


def main() -> int:
    # This script has no call that lists or walks EXCLUDED_ROOT.
    if EXCLUDED_ROOT == COMPLETED_ROOT or EXCLUDED_ROOT == MEDIA_ROOT:
        raise RuntimeError("Configuration d'exclusion invalide")
    sys.path.insert(0, "/volume3/chill")
    import doublon_checker as dc

    completed = dc.scan_tree(COMPLETED_ROOT, frozenset({".mkv"}))
    media = dc.scan_tree(MEDIA_ROOT, frozenset({".mkv"}))
    if completed.errors or media.errors:
        raise RuntimeError(
            f"Scan incomplet: completed={len(completed.errors)}, "
            f"medias={len(media.errors)}"
        )
    completed_ids = {entry.identity for entry in completed.files}
    media_ids = {entry.identity for entry in media.files}
    completed_only = [
        entry for entry in completed.videos if entry.identity not in media_ids
    ]
    media_only = [
        entry for entry in media.videos if entry.identity not in completed_ids
    ]
    known = {dc.lexical_key(entry.path) for entry in completed_only}
    qbit_version, torrents, qbit = qbit_file_index(dc, known)
    arr = load_arr(dc)
    engine = dc.SimilarityEngine(media.videos)

    completed_rows: list[dict[str, object]] = []
    for entry in completed_only:
        parsed = dc.parse_scene_name(entry.path)
        qbit_rows = qbit.get(dc.lexical_key(entry.path), [])
        hashes = list(dict.fromkeys(str(row["hash"]) for row in qbit_rows))
        match = arr_from_history(parsed, hashes, arr) or arr_from_name(parsed, arr)
        similar = engine.find(entry, 0.35, limit=3)
        best = similar[0] if similar else None
        current_media = str(match.get("current_media_path", "")) if match else ""
        if match and current_media and Path(current_media).is_file():
            classification = "autre_release_deja_dans_jellyfin"
            recommendation = "suppression_completed_a_valider"
            potential = entry.allocated
        elif match:
            classification = "identifie_arr_absent_de_jellyfin"
            recommendation = "import_hardlink_a_valider"
            potential = 0
        elif best and best.score >= 0.72:
            classification = "correspondance_jellyfin_a_valider"
            recommendation = "examiner_versions_avant_suppression"
            potential = entry.allocated
        else:
            classification = "non_gere_ou_ambigu"
            recommendation = "conserver_et_examiner"
            potential = 0
        completed_rows.append({
            "classification": classification,
            "recommendation": recommendation,
            "potential_gain_bytes": potential,
            "potential_gain": human_size(potential),
            "completed_path": str(entry.path),
            "size_bytes": entry.size,
            "size": human_size(entry.size),
            "allocated_bytes": entry.allocated,
            "codec": parsed.codec or "",
            "resolution": parsed.resolution or "",
            "languages": "|".join(parsed.languages),
            "title_parsed": " ".join(parsed.title_tokens),
            "year": parsed.year or "",
            "season": parsed.episode[0] if parsed.episode else "",
            "episode": parsed.episode[1] if parsed.episode else "",
            "qbit_hashes": " | ".join(hashes),
            "qbit_names": join(qbit_rows, "name"),
            "qbit_states": join(qbit_rows, "state"),
            "arr_type": match.get("arr_type", "") if match else "",
            "arr_id": match.get("arr_id", "") if match else "",
            "arr_title": match.get("arr_title", "") if match else "",
            "arr_monitored": match.get("arr_monitored", "") if match else "",
            "arr_match_method": match.get("match_method", "") if match else "",
            "arr_match_score": match.get("match_score", "") if match else "",
            "arr_current_media_path": current_media,
            "best_jellyfin_path": str(best.path) if best else "",
            "best_jellyfin_score": round(best.score, 4) if best else "",
            "best_jellyfin_size": best.size if best else "",
        })
    completed_rows.sort(
        key=lambda row: (-int(row["potential_gain_bytes"]), -int(row["allocated_bytes"]))
    )

    media_rows: list[dict[str, object]] = []
    for entry in media_only:
        parsed = dc.parse_scene_name(entry.path)
        match = (
            arr["sonarr_path_map"].get(str(entry.path).casefold())
            or arr["radarr_path_map"].get(str(entry.path).casefold())
            or arr_from_name(parsed, arr)
        )
        media_rows.append({
            "media_path": str(entry.path),
            "size_bytes": entry.size,
            "size": human_size(entry.size),
            "allocated_bytes": entry.allocated,
            "codec": parsed.codec or "",
            "resolution": parsed.resolution or "",
            "languages": "|".join(parsed.languages),
            "title_parsed": " ".join(parsed.title_tokens),
            "year": parsed.year or "",
            "season": parsed.episode[0] if parsed.episode else "",
            "episode": parsed.episode[1] if parsed.episode else "",
            "arr_type": match.get("arr_type", "") if match else "",
            "arr_id": match.get("arr_id", "") if match else "",
            "arr_title": match.get("arr_title", "") if match else "",
            "arr_monitored": match.get("arr_monitored", "") if match else "",
            "episode_id": match.get("episode_id", "") if match else "",
            "arr_match_method": match.get("match_method", "path_arr") if match else "",
            "arr_match_score": match.get("match_score", 1.0) if match else "",
            "search_supported": bool(match and match.get("arr_id")),
        })
    media_rows.sort(key=lambda row: (-int(row["allocated_bytes"]), str(row["media_path"])))

    stopped = [item for item in torrents if str(item.get("state", "")) == "stoppedUP"]
    missing = [item for item in torrents if str(item.get("state", "")) == "missingFiles"]
    qbit_rows = [
        {
            "state": str(item.get("state", "")),
            "name": str(item.get("name", "")),
            "hash": str(item.get("hash", "")),
            "size_bytes": int(item.get("size", 0)),
            "size": human_size(int(item.get("size", 0))),
            "progress": float(item.get("progress", 0.0)),
            "category": str(item.get("category", "")),
            "content_path": str(item.get("content_path", "")),
        }
        for item in sorted(stopped + missing, key=lambda row: (
            str(row.get("state", "")), str(row.get("name", "")).casefold()
        ))
    ]

    stamp = dc.utc_stamp()
    output = REPORT_ROOT / f"final-readable-audit-{stamp}"
    output.mkdir(parents=False, exist_ok=False)
    completed_csv = output / "completed-a-classer.csv"
    media_csv = output / "jellyfin-sans-source-completed.csv"
    qbit_csv = output / "qbittorrent-arretes-ou-manquants.csv"
    if not completed_rows or not media_rows:
        raise RuntimeError("L'audit ne peut pas produire les CSV principaux vides")
    atomic_csv(completed_csv, list(completed_rows[0]), completed_rows)
    atomic_csv(media_csv, list(media_rows[0]), media_rows)
    qbit_fields = (list(qbit_rows[0]) if qbit_rows else [
        "state", "name", "hash", "size_bytes", "size", "progress",
        "category", "content_path",
    ])
    atomic_csv(qbit_csv, qbit_fields, qbit_rows)

    counts = Counter(str(row["classification"]) for row in completed_rows)
    sizes = Counter()
    potentials = Counter()
    for row in completed_rows:
        sizes[str(row["classification"])] += int(row["allocated_bytes"])
        potentials[str(row["classification"])] += int(row["potential_gain_bytes"])
    markdown = [
        "# Audit prudent du reliquat",
        "",
        f"Créé le {datetime.now(timezone.utc).isoformat(timespec='seconds')}.",
        "",
        "## Garanties",
        "",
        "- Audit en lecture seule des données.",
        f"- `{EXCLUDED_ROOT}` est exclu et n'a pas été parcouru.",
        "- Aucun téléchargement Sonarr/Radarr n'a été lancé.",
        "",
        "## completed",
        "",
        f"- {len(completed_only)} MKV sans inode dans Jellyfin, soit "
        f"{human_size(sum(item.allocated for item in completed_only))}.",
    ]
    for category in sorted(counts):
        markdown.append(
            f"- `{category}` : {counts[category]} fichier(s), "
            f"{human_size(sizes[category])}, gain soumis à validation "
            f"{human_size(potentials[category])}."
        )
    markdown.extend([
        "",
        "Les lignes du CSV sont classées par gain potentiel décroissant. Aucune "
        "recommandation n'a été appliquée.",
        "",
        "## Jellyfin",
        "",
        f"- {len(media_only)} MKV sans inode dans `completed`, soit "
        f"{human_size(sum(item.allocated for item in media_only))}.",
        f"- {sum(bool(row['search_supported']) for row in media_rows)} fichier(s) "
        "sont reliés à une entrée Sonarr/Radarr et peuvent faire l'objet d'une "
        "recherche interactive de release.",
        "",
        "## qBittorrent",
        "",
        f"- Version : {qbit_version}.",
        f"- Torrents `stoppedUP` : {len(stopped)}.",
        f"- Torrents `missingFiles` : {len(missing)}.",
        "- Leur présence est seulement inventoriée ; aucune tâche n'a été retirée.",
        "",
        "## Fichiers détaillés",
        "",
        "- `completed-a-classer.csv`",
        "- `jellyfin-sans-source-completed.csv`",
        "- `qbittorrent-arretes-ou-manquants.csv`",
        "",
    ])
    summary_path = output / "rapport.md"
    atomic_text(summary_path, "\n".join(markdown))
    manifest = {
        "created_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "excluded_root": str(EXCLUDED_ROOT),
        "completed_unlinked_mkv": len(completed_only),
        "completed_unlinked_allocated": sum(item.allocated for item in completed_only),
        "media_unlinked_mkv": len(media_only),
        "media_unlinked_allocated": sum(item.allocated for item in media_only),
        "classifications": dict(counts),
        "classification_allocated": dict(sizes),
        "classification_potential_gain": dict(potentials),
        "media_search_supported": sum(bool(row["search_supported"]) for row in media_rows),
        "stopped_up": len(stopped),
        "missing_files": len(missing),
        "output": str(output),
    }
    atomic_text(output / "manifest.json", json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(manifest, ensure_ascii=False, indent=2))
    print(f"OUTPUT_DIR={output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
