#!/usr/bin/env python3
"""Audit and interactively remove downloads not hardlinked into a media tree.

The program deliberately uses only Python's standard library.  All settings are
kept in the ``if __name__ == "__main__"`` block at the bottom of this file.
"""

from __future__ import annotations

import csv
import concurrent.futures
import dataclasses
import datetime as dt
import difflib
import errno
import getpass
import http.cookiejar
import json
import math
import os
import posixpath
import re
import shutil
import stat
import subprocess
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Iterator, Sequence


Identity = tuple[int, int]


@dataclass(frozen=True)
class Config:
    completed_path: Path
    media_path: Path
    report_path: Path
    qbittorrent_url: str
    qbittorrent_username: str
    qbittorrent_password: str
    qbittorrent_container_path: str
    qbittorrent_host_path: Path
    video_extensions: frozenset[str]
    similarity_threshold: float
    recheck_timeout_seconds: int
    quick_sample_size: int
    quick_sample_count: int
    full_compare_chunk_size: int
    search_similar_names_for_hardlink: bool
    run_mode: str = "interactive"
    auto_min_age_hours: float = 48.0
    auto_delete_similarity_threshold: float = 0.90
    auto_remove_empty_torrents: bool = True
    ffprobe_command: tuple[str, ...] = ()
    ffprobe_host_path: Path | None = None
    ffprobe_container_path: str = ""
    auto_require_duration_match: bool = False
    auto_duration_tolerance_seconds: float = 5.0
    auto_duration_tolerance_ratio: float = 0.002
    ffprobe_workers: int = 4


@dataclass(frozen=True)
class ScanError:
    path: str
    message: str


@dataclass(frozen=True)
class EntryRecord:
    path: Path
    relative: str
    kind: str
    size: int
    allocated: int
    dev: int
    ino: int
    nlink: int
    mtime_ns: int
    mode: int
    is_video: bool = False

    @property
    def identity(self) -> Identity:
        return (self.dev, self.ino)

    @property
    def signature(self) -> tuple[Any, ...]:
        return (
            self.kind,
            self.dev,
            self.ino,
            self.size,
            self.nlink,
            self.mtime_ns,
            self.mode,
        )


@dataclass
class TreeScan:
    root: Path
    entries: dict[str, EntryRecord]
    errors: list[ScanError]

    @property
    def files(self) -> list[EntryRecord]:
        return [entry for entry in self.entries.values() if entry.kind == "file"]

    @property
    def videos(self) -> list[EntryRecord]:
        return [entry for entry in self.entries.values() if entry.is_video]

    @property
    def directories(self) -> list[EntryRecord]:
        return [entry for entry in self.entries.values() if entry.kind == "dir"]


@dataclass(frozen=True)
class ParsedName:
    normalized: str
    title_tokens: tuple[str, ...]
    year: int | None
    episode: tuple[int, int] | None
    resolution: str | None
    codec: str | None
    source: str | None
    languages: tuple[str, ...]
    editions: tuple[str, ...]


@dataclass(frozen=True)
class Similarity:
    path: Path
    score: float
    size: int
    parsed: ParsedName


@dataclass
class DeletionCandidate:
    candidate_id: int
    kind: str
    path: Path
    videos: list[EntryRecord]
    entries: list[EntryRecord]
    snapshot: dict[str, tuple[Any, ...]]
    similarities: dict[str, list[Similarity]] = field(default_factory=dict)
    estimated_reclaimable: int = 0

    @property
    def logical_size(self) -> int:
        return sum(entry.size for entry in self.entries if entry.kind == "file")

    @property
    def has_unmatched_video(self) -> bool:
        return any(not self.similarities.get(str(video.path)) for video in self.videos)


@dataclass(frozen=True)
class HardlinkRepair:
    repair_id: int
    completed: EntryRecord
    media: EntryRecord
    similarity_score: float
    detection_method: str


@dataclass(frozen=True)
class AutoCandidateDecision:
    candidate_id: int
    path: Path
    eligible: bool
    action: str
    reasons: tuple[str, ...]
    replacements: tuple[tuple[str, str, float], ...] = ()


@dataclass(frozen=True)
class QbitFile:
    path: Path
    name: str
    size: int
    priority: int


@dataclass
class QbitTorrent:
    torrent_hash: str
    name: str
    state: str
    files: list[QbitFile]


@dataclass
class QbitIndex:
    torrents: dict[str, QbitTorrent]
    path_to_hashes: dict[str, set[str]]
    mapping_errors: list[str] = field(default_factory=list)


@dataclass
class DeletionResult:
    logical_deleted: int = 0
    attributable_gain: int = 0
    hardlink_gain: int = 0
    observed_immediate_gain: int = 0
    observed_final_gain: int = 0
    files_deleted: int = 0
    directories_deleted: int = 0
    torrents_removed: int = 0
    hardlinks_created: int = 0
    failed_repairs: int = 0
    failed_candidates: int = 0
    torrents_left_stopped: int = 0
    stopped_torrent_hashes: list[str] = field(default_factory=list)


SCENE_MARKERS = {
    "1080p", "1080i", "2160p", "720p", "480p", "4k", "uhd",
    "bluray", "blu", "ray", "bdrip", "brrip", "webrip", "web", "webdl", "dl",
    "hdtv", "dvdrip", "remux", "proper", "repack", "internal", "extended",
    "x264", "x265", "h264", "h265", "hevc", "av1", "xvid", "hdr",
    "hdr10", "hdr10plus", "dv", "dolby", "vision", "sdr", "10bit",
    "aac", "ac3", "eac3", "ddp", "dts", "dtshd", "truehd", "atmos",
    "multi", "multisub", "dual", "audio", "vostfr", "subfrench", "french",
    "fr", "eng", "english", "vf", "vff", "vfi", "vo", "subs", "dubbed",
    "mkv", "readnfo", "limited", "unrated", "complete",
}
LANGUAGE_MARKERS = {
    "french", "fr", "vf", "vff", "vfi", "vostfr", "english", "eng",
    "multi", "dual", "subfrench",
}
CODEC_MARKERS = {"x264", "x265", "h264", "h265", "hevc", "av1", "xvid"}
RESOLUTION_MARKERS = {"2160p", "1080p", "1080i", "720p", "480p", "4k", "uhd"}
RESOLUTION_RANK = {
    "480p": 1,
    "720p": 2,
    "1080i": 3,
    "1080p": 4,
    "2160p": 5,
    "4k": 5,
    "uhd": 5,
}
SOURCE_MARKERS = {
    "bluray", "bdrip", "brrip", "webrip", "webdl", "hdtv", "dvdrip", "remux",
}
EDITION_MARKERS = {
    "extended", "imax", "uncut", "unrated", "theatrical", "remastered",
    "criterion", "fanedit", "openmatte", "directorscut",
}
STRONG_TECHNICAL_MARKERS = (
    RESOLUTION_MARKERS | CODEC_MARKERS | SOURCE_MARKERS | LANGUAGE_MARKERS |
    {"web", "hdtv", "bluray", "bdrip", "brrip", "dvdrip", "remux"}
)
EPISODE_RE = re.compile(r"(?<![a-z0-9])s(\d{1,2})[ ._-]*e(\d{1,3})(?!\d)", re.I)
YEAR_RE = re.compile(r"(?<!\d)((?:19|20)\d{2})(?!\d)")
TOKEN_RE = re.compile(r"[a-z0-9]+")


def utc_stamp() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def iso_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def human_size(value: int) -> str:
    negative = value < 0
    amount = float(abs(value))
    units = ("B", "KiB", "MiB", "GiB", "TiB", "PiB")
    unit = units[0]
    for unit in units:
        if amount < 1024.0 or unit == units[-1]:
            break
        amount /= 1024.0
    prefix = "-" if negative else ""
    if unit == "B":
        return f"{prefix}{int(amount)} {unit}"
    return f"{prefix}{amount:.2f} {unit}"


def allocated_bytes(st: os.stat_result) -> int:
    blocks = getattr(st, "st_blocks", None)
    if blocks is not None:
        return int(blocks) * 512
    return math.ceil(st.st_size / 4096) * 4096 if st.st_size else 0


def entry_kind(mode: int) -> str:
    if stat.S_ISREG(mode):
        return "file"
    if stat.S_ISDIR(mode):
        return "dir"
    if stat.S_ISLNK(mode):
        return "symlink"
    return "other"


def make_entry(path: Path, root: Path, st: os.stat_result,
               extensions: frozenset[str]) -> EntryRecord:
    kind = entry_kind(st.st_mode)
    return EntryRecord(
        path=path,
        relative=os.path.relpath(path, root),
        kind=kind,
        size=st.st_size if kind == "file" else 0,
        allocated=allocated_bytes(st) if kind == "file" else 0,
        dev=st.st_dev,
        ino=st.st_ino,
        nlink=st.st_nlink,
        mtime_ns=st.st_mtime_ns,
        mode=st.st_mode,
        is_video=kind == "file" and path.suffix.casefold() in extensions,
    )


def scan_tree(root: Path, extensions: frozenset[str]) -> TreeScan:
    root = root.resolve(strict=True)
    if not root.is_dir():
        raise NotADirectoryError(root)
    entries: dict[str, EntryRecord] = {}
    errors: list[ScanError] = []
    stack = [root]
    while stack:
        directory = stack.pop()
        try:
            with os.scandir(directory) as iterator:
                children = list(iterator)
        except OSError as exc:
            errors.append(ScanError(str(directory), str(exc)))
            continue
        for child in children:
            path = Path(child.path)
            try:
                st = child.stat(follow_symlinks=False)
                record = make_entry(path, root, st, extensions)
                entries[record.relative] = record
                if record.kind == "dir":
                    stack.append(path)
            except OSError as exc:
                errors.append(ScanError(str(path), str(exc)))
    return TreeScan(root=root, entries=entries, errors=errors)


def tree_statistics(scan: TreeScan) -> dict[str, int]:
    files = scan.files
    unique: dict[Identity, EntryRecord] = {}
    for item in files:
        unique.setdefault(item.identity, item)
    return {
        "files": len(files),
        "videos": len(scan.videos),
        "logical_bytes": sum(item.size for item in files),
        "allocated_unique_bytes": sum(item.allocated for item in unique.values()),
        "unique_inodes": len(unique),
        "errors": len(scan.errors),
    }


def compute_scan_statistics(completed: TreeScan, media: TreeScan) -> dict[str, Any]:
    completed_by_id = {item.identity: item for item in completed.files}
    media_by_id = {item.identity: item for item in media.files}
    shared = set(completed_by_id) & set(media_by_id)
    combined = dict(media_by_id)
    combined.update(completed_by_id)

    completed_counts = Counter(item.identity for item in completed.files)
    exclusive_reclaimable = 0
    for identity, item in completed_by_id.items():
        if identity not in media_by_id and completed_counts[identity] >= item.nlink:
            exclusive_reclaimable += item.allocated

    orphan_video_ids = {
        item.identity for item in completed.videos if item.identity not in media_by_id
    }
    video_reclaimable = 0
    for identity in orphan_video_ids:
        item = completed_by_id[identity]
        if completed_counts[identity] >= item.nlink:
            video_reclaimable += item.allocated

    return {
        "completed": tree_statistics(completed),
        "media": tree_statistics(media),
        "shared_inodes": len(shared),
        "shared_allocated_bytes": sum(completed_by_id[key].allocated for key in shared),
        "combined_unique_allocated_bytes": sum(item.allocated for item in combined.values()),
        "completed_exclusive_reclaimable_bytes": exclusive_reclaimable,
        "orphan_videos_reclaimable_bytes": video_reclaimable,
    }


def ascii_text(value: str) -> str:
    normalized = unicodedata.normalize("NFKD", value.casefold())
    return normalized.encode("ascii", "ignore").decode("ascii")


def parse_scene_name(path: Path) -> ParsedName:
    raw = ascii_text(path.stem)
    episode_match = EPISODE_RE.search(raw)
    episode = None
    if episode_match:
        episode = (int(episode_match.group(1)), int(episode_match.group(2)))
    year_match = YEAR_RE.search(raw)
    year = int(year_match.group(1)) if year_match else None
    token_matches = list(TOKEN_RE.finditer(raw))
    tokens = [match.group(0) for match in token_matches]
    resolution = next((token for token in tokens if token in RESOLUTION_MARKERS), None)
    codec = next((token for token in tokens if token in CODEC_MARKERS), None)
    source = next((token for token in tokens if token in SOURCE_MARKERS), None)
    if source is None and "web" in tokens and "dl" in tokens:
        source = "web-dl"
    languages = tuple(sorted({token for token in tokens if token in LANGUAGE_MARKERS}))
    editions_found = {token for token in tokens if token in EDITION_MARKERS}
    joined_tokens = " ".join(tokens)
    if "director s cut" in joined_tokens or "directors cut" in joined_tokens:
        editions_found.add("directorscut")
    if "open matte" in joined_tokens:
        editions_found.add("openmatte")

    # Scene/P2P filenames place the content identity first and technical tags
    # after the year or SxxExx marker.  Stopping there prevents release groups
    # such as FW, SUPPLY or AXWELL from diluting the title score.
    boundary_candidates = [
        match.start() for match in (episode_match, year_match) if match is not None
    ]
    if not boundary_candidates:
        boundary_candidates.extend(
            match.start() for match in token_matches
            if match.group(0) in STRONG_TECHNICAL_MARKERS
        )
    title_raw = raw[:min(boundary_candidates)] if boundary_candidates else raw
    title_tokens = TOKEN_RE.findall(title_raw)
    if not title_tokens:
        title_tokens = [
            token for token in tokens
            if token not in SCENE_MARKERS
            and not (token.isdigit() and (len(token) == 4 or len(token) <= 3))
            and not re.fullmatch(r"s\d{1,2}e\d{1,3}", token)
        ]
    return ParsedName(
        normalized=" ".join(tokens),
        title_tokens=tuple(title_tokens),
        year=year,
        episode=episode,
        resolution=resolution,
        codec=codec,
        source=source,
        languages=languages,
        editions=tuple(sorted(editions_found)),
    )


def similarity_score(left: EntryRecord, left_name: ParsedName,
                     right: EntryRecord, right_name: ParsedName) -> float:
    if left_name.episode and right_name.episode and left_name.episode != right_name.episode:
        return 0.0
    left_tokens = set(left_name.title_tokens)
    right_tokens = set(right_name.title_tokens)
    if not left_tokens or not right_tokens:
        return 0.0
    common = left_tokens & right_tokens
    if not common:
        return 0.0
    union = left_tokens | right_tokens
    jaccard = len(common) / len(union)
    sequence = difflib.SequenceMatcher(
        None, " ".join(left_name.title_tokens), " ".join(right_name.title_tokens)
    ).ratio()
    larger = max(left.size, right.size)
    size_score = min(left.size, right.size) / larger if larger else 1.0
    score = 0.55 * sequence + 0.25 * jaccard + 0.10 * size_score
    if left_name.year and right_name.year:
        score += 0.10 if left_name.year == right_name.year else -0.20
    if left_name.episode and right_name.episode == left_name.episode:
        score += 0.15
    return max(0.0, min(1.0, score))


class SimilarityEngine:
    def __init__(self, media_videos: Sequence[EntryRecord]) -> None:
        self.records = list(media_videos)
        self.parsed = [parse_scene_name(record.path) for record in self.records]
        self.by_token: dict[str, set[int]] = defaultdict(set)
        self.by_episode: dict[tuple[int, int], set[int]] = defaultdict(set)
        self.by_year: dict[int, set[int]] = defaultdict(set)
        for index, parsed in enumerate(self.parsed):
            for token in set(parsed.title_tokens):
                self.by_token[token].add(index)
            if parsed.episode:
                self.by_episode[parsed.episode].add(index)
            if parsed.year:
                self.by_year[parsed.year].add(index)

    def find(self, video: EntryRecord, threshold: float,
             limit: int = 3) -> list[Similarity]:
        parsed = parse_scene_name(video.path)
        token_pool: set[int] = set()
        for token in set(parsed.title_tokens):
            token_pool.update(self.by_token.get(token, ()))
        if parsed.episode:
            episode_pool = self.by_episode.get(parsed.episode, set())
            pool = token_pool & episode_pool if token_pool else set(episode_pool)
        elif parsed.year and token_pool:
            same_year = self.by_year.get(parsed.year, set())
            pool = token_pool & same_year or token_pool
        else:
            pool = token_pool
        scored: list[Similarity] = []
        for index in pool:
            media_record = self.records[index]
            score = similarity_score(video, parsed, media_record, self.parsed[index])
            if score >= threshold:
                scored.append(Similarity(media_record.path, score, media_record.size,
                                         self.parsed[index]))
        scored.sort(key=lambda item: (-item.score, abs(item.size - video.size), str(item.path)))
        return scored[:limit]


def language_profile(languages: Sequence[str]) -> tuple[frozenset[str], bool] | None:
    """Return conservative language classes and whether the release is multi/dual."""
    values = {value.casefold() for value in languages}
    if not values:
        return None
    wide = bool(values & {"multi", "dual"})
    classes: set[str] = set()
    if values & {"french", "fr", "vf", "vff", "vfi"}:
        classes.add("fr")
    if values & {"english", "eng"}:
        classes.add("en")
    if values & {"vostfr", "subfrench"}:
        classes.add("fr_sub")
    if wide:
        classes.add("multi")
    return frozenset(classes), wide


def languages_are_compatible(source: ParsedName, replacement: ParsedName) -> bool:
    source_profile = language_profile(source.languages)
    replacement_profile = language_profile(replacement.languages)
    if source_profile is None or replacement_profile is None:
        return False
    source_classes, source_wide = source_profile
    replacement_classes, replacement_wide = replacement_profile
    if source_wide:
        return replacement_wide
    if replacement_wide:
        return True
    return bool(source_classes) and source_classes.issubset(replacement_classes)


def auto_match_rejection(video: EntryRecord, match: Similarity,
                         threshold: float) -> str | None:
    source = parse_scene_name(video.path)
    replacement = match.parsed
    if match.score < threshold:
        return "score_insuffisant"
    if source.title_tokens != replacement.title_tokens:
        return "titre_different_ou_ambigu"
    if source.episode:
        if replacement.episode != source.episode:
            return "episode_different_ou_absent"
    else:
        if source.year is None or replacement.year is None:
            return "annee_absente"
        if replacement.year != source.year:
            return "annee_differente"
    if source.resolution is None or replacement.resolution is None:
        return "resolution_absente"
    if RESOLUTION_RANK.get(replacement.resolution, 0) < RESOLUTION_RANK.get(
            source.resolution, 0):
        return "resolution_jellyfin_inferieure"
    if not languages_are_compatible(source, replacement):
        return "langue_absente_ou_incompatible"
    if source.editions != replacement.editions:
        return "edition_differente_ou_ambigue"
    return None


def candidate_age_reason(candidate: DeletionCandidate, minimum_age_hours: float,
                         now_ns: int) -> str | None:
    cutoff_ns = now_ns - int(minimum_age_hours * 3600 * 1_000_000_000)
    mtimes = [entry.mtime_ns for entry in candidate.entries]
    try:
        mtimes.append(os.lstat(candidate.path).st_mtime_ns)
    except OSError:
        return "cible_inaccessible_pendant_controle_age"
    if not mtimes or max(mtimes) > cutoff_ns:
        return f"modification_plus_recente_que_{minimum_age_hours:g}h"
    return None


def assess_auto_candidate(candidate: DeletionCandidate, threshold: float,
                          minimum_age_hours: float, now_ns: int,
                          blocked_video_paths: set[str] | None = None,
                          blocked_reason: str = "reparation_hardlink_non_resolue",
                          action: str = "delete") -> AutoCandidateDecision:
    blocked = blocked_video_paths or set()
    reasons: list[str] = []
    replacements: list[tuple[str, str, float]] = []
    age_reason = candidate_age_reason(candidate, minimum_age_hours, now_ns)
    if age_reason:
        reasons.append(age_reason)
    for video in candidate.videos:
        video_key = lexical_key(video.path)
        if video_key in blocked:
            reasons.append(f"{blocked_reason}:{video.path}")
            continue
        matches = candidate.similarities.get(str(video.path), [])
        accepted: Similarity | None = None
        rejected: list[str] = []
        for match in matches:
            rejection = auto_match_rejection(video, match, threshold)
            if rejection is None:
                accepted = match
                break
            rejected.append(rejection)
        if accepted is None:
            detail = ",".join(sorted(set(rejected))) if rejected else "aucun_remplacant"
            reasons.append(f"video_non_admissible:{video.path}:{detail}")
        else:
            replacements.append((str(video.path), str(accepted.path), accepted.score))
    eligible = not reasons and bool(candidate.videos)
    return AutoCandidateDecision(
        candidate_id=candidate.candidate_id,
        path=candidate.path,
        eligible=eligible,
        action=action if eligible else "keep",
        reasons=tuple(reasons) if reasons else ("criteres_prudents_satisfaits",),
        replacements=tuple(replacements),
    )


def assess_auto_candidates(candidates: Sequence[DeletionCandidate], threshold: float,
                           minimum_age_hours: float, now_ns: int,
                           blocked_video_paths: set[str] | None = None,
                           blocked_reason: str = "reparation_hardlink_non_resolue") -> list[AutoCandidateDecision]:
    return [
        assess_auto_candidate(
            candidate, threshold, minimum_age_hours, now_ns,
            blocked_video_paths, blocked_reason,
        )
        for candidate in candidates
    ]


def probe_media_duration(path: Path, config: Config) -> float:
    if not config.ffprobe_command:
        raise RuntimeError("commande ffprobe non configurée")
    probe_path = str(path)
    if config.ffprobe_container_path:
        host_root = config.ffprobe_host_path
        if host_root is None:
            raise RuntimeError("racine hôte ffprobe non configurée")
        try:
            relative = path.resolve(strict=True).relative_to(host_root.resolve(strict=True))
        except ValueError as exc:
            raise RuntimeError(f"chemin hors mapping ffprobe : {path}") from exc
        probe_path = str(PurePosixPath(config.ffprobe_container_path).joinpath(
            *relative.parts
        ))
    command = [
        *config.ffprobe_command,
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
        probe_path,
    ]
    try:
        result = subprocess.run(
            command, check=False, capture_output=True, text=True, timeout=120
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise RuntimeError(f"ffprobe impossible pour {path}: {exc}") from exc
    if result.returncode != 0:
        detail = result.stderr.strip().replace("\n", " ")[:500]
        raise RuntimeError(f"ffprobe a échoué pour {path}: {detail}")
    try:
        duration = float(result.stdout.strip())
    except ValueError as exc:
        raise RuntimeError(f"durée ffprobe invalide pour {path}") from exc
    if not math.isfinite(duration) or duration <= 0:
        raise RuntimeError(f"durée ffprobe invalide pour {path}: {duration}")
    return duration


def verify_auto_decision_durations(
        decisions: Sequence[AutoCandidateDecision], config: Config,
        ) -> tuple[list[AutoCandidateDecision], list[dict[str, Any]]]:
    if not config.auto_require_duration_match:
        return list(decisions), []

    paths = sorted({
        path
        for decision in decisions if decision.eligible
        for replacement in decision.replacements
        for path in replacement[:2]
    })
    durations: dict[str, float] = {}
    errors: dict[str, str] = {}

    def run_probe(path_value: str) -> tuple[str, float | None, str | None]:
        try:
            return path_value, probe_media_duration(Path(path_value), config), None
        except RuntimeError as exc:
            return path_value, None, str(exc)

    with concurrent.futures.ThreadPoolExecutor(
            max_workers=max(1, config.ffprobe_workers)) as executor:
        for path_value, duration, error in executor.map(run_probe, paths):
            if error is None and duration is not None:
                durations[path_value] = duration
            else:
                errors[path_value] = error or "erreur ffprobe inconnue"

    verified: list[AutoCandidateDecision] = []
    checks: list[dict[str, Any]] = []
    for decision in decisions:
        if not decision.eligible:
            verified.append(decision)
            continue
        failure_reasons: list[str] = []
        for completed_path, media_path, score in decision.replacements:
            completed_duration = durations.get(completed_path)
            media_duration = durations.get(media_path)
            error = errors.get(completed_path) or errors.get(media_path)
            difference = None
            tolerance = None
            matches = False
            if error is None and completed_duration is not None and media_duration is not None:
                difference = abs(completed_duration - media_duration)
                tolerance = max(
                    config.auto_duration_tolerance_seconds,
                    max(completed_duration, media_duration)
                    * config.auto_duration_tolerance_ratio,
                )
                matches = difference <= tolerance
            if error is not None:
                failure_reasons.append(
                    f"duree_non_verifiee:{completed_path}:{error}"
                )
            elif not matches:
                failure_reasons.append(
                    f"duree_differente:{completed_path}:ecart={difference:.3f}s"
                )
            checks.append({
                "candidate_id": decision.candidate_id,
                "completed_path": completed_path,
                "media_path": media_path,
                "score": round(score, 4),
                "completed_duration_seconds": completed_duration,
                "media_duration_seconds": media_duration,
                "difference_seconds": difference,
                "tolerance_seconds": tolerance,
                "matches": matches,
                "error": error,
            })
        if failure_reasons:
            verified.append(dataclasses.replace(
                decision,
                eligible=False,
                action="keep",
                reasons=tuple(failure_reasons),
            ))
        else:
            verified.append(decision)
    return verified, checks


def write_duration_report(report_dir: Path, stamp: str, phase: str,
                          checks: Sequence[dict[str, Any]]) -> Path:
    suffix = "" if phase == "initial" else f"-{phase}"
    path = report_dir / f"duration-check-{stamp}{suffix}.json"
    write_json(path, {
        "created_at": iso_now(),
        "phase": phase,
        "checks": list(checks),
    })
    return path


def files_match_samples(left: Path, right: Path, sample_size: int = 1024 * 1024,
                        sample_count: int = 5) -> bool:
    """Quickly reject different files using evenly distributed byte ranges.

    A positive result is only a repair proposal.  It is never considered proof
    of equality: create_hardlink_repair performs a complete comparison again.
    """
    try:
        left_stat = os.stat(left, follow_symlinks=False)
        right_stat = os.stat(right, follow_symlinks=False)
        if not stat.S_ISREG(left_stat.st_mode) or not stat.S_ISREG(right_stat.st_mode):
            return False
        if left_stat.st_size != right_stat.st_size:
            return False
        size = left_stat.st_size
        if size == 0:
            return True
        width = min(max(1, sample_size), size)
        count = max(1, sample_count)
        span = size - width
        if count == 1 or span == 0:
            offsets = [0]
        else:
            offsets = sorted({round(index * span / (count - 1)) for index in range(count)})
        with left.open("rb") as left_handle, right.open("rb") as right_handle:
            for offset in offsets:
                left_handle.seek(offset)
                right_handle.seek(offset)
                if left_handle.read(width) != right_handle.read(width):
                    return False
        return True
    except OSError:
        return False


def files_are_identical(left: Path, right: Path,
                        chunk_size: int = 16 * 1024 * 1024,
                        show_progress: bool = False) -> bool:
    """Compare every byte of two files, with large buffers and sequential hints."""
    try:
        left_stat = os.stat(left, follow_symlinks=False)
        right_stat = os.stat(right, follow_symlinks=False)
        if not stat.S_ISREG(left_stat.st_mode) or not stat.S_ISREG(right_stat.st_mode):
            return False
        if left_stat.st_size != right_stat.st_size:
            return False
        total_size = left_stat.st_size
        buffer_size = max(64 * 1024, chunk_size)
        left_buffer = bytearray(buffer_size)
        right_buffer = bytearray(buffer_size)
        left_view = memoryview(left_buffer)
        right_view = memoryview(right_buffer)
        processed = 0
        last_percent = -1
        with left.open("rb") as left_handle, right.open("rb") as right_handle:
            if hasattr(os, "posix_fadvise") and hasattr(os, "POSIX_FADV_SEQUENTIAL"):
                try:
                    os.posix_fadvise(left_handle.fileno(), 0, 0, os.POSIX_FADV_SEQUENTIAL)
                    os.posix_fadvise(right_handle.fileno(), 0, 0, os.POSIX_FADV_SEQUENTIAL)
                except OSError:
                    pass
            while True:
                left_count = left_handle.readinto(left_buffer)
                right_count = right_handle.readinto(right_buffer)
                if left_count != right_count:
                    return False
                if left_count == 0:
                    if show_progress:
                        if last_percent == 100:
                            print(flush=True)
                        else:
                            print(
                                f"\r  Vérification exacte : 100% "
                                f"({human_size(total_size)} / {human_size(total_size)})",
                                flush=True,
                            )
                    return True
                if left_view[:left_count] != right_view[:right_count]:
                    if show_progress:
                        print()
                    return False
                processed += left_count
                if show_progress and total_size:
                    percent = min(100, int(processed * 100 / total_size))
                    if percent != last_percent and (percent == 100 or percent % 2 == 0):
                        print(
                            f"\r  Vérification exacte : {percent:3d}% "
                            f"({human_size(processed)} / {human_size(total_size)})",
                            end="", flush=True,
                        )
                        last_percent = percent
    except OSError:
        if show_progress:
            print()
        return False


def discover_hardlink_repairs(completed: TreeScan, media: TreeScan,
                              threshold: float,
                              sample_size: int = 1024 * 1024,
                              sample_count: int = 5,
                              include_similar_names: bool = True) -> list[HardlinkRepair]:
    """Find potential copies; exact proof is deliberately deferred.

    Exact filename+size pairs require no content read.  Sampling is used only
    for similarly named releases whose filenames differ.
    """
    media_ids = {entry.identity for entry in media.files}
    media_by_name_size: dict[tuple[str, int], list[EntryRecord]] = defaultdict(list)
    for media_video in media.videos:
        media_by_name_size[(media_video.path.name.casefold(), media_video.size)].append(
            media_video
        )
    media_device = media.root.stat().st_dev
    repairs: list[HardlinkRepair] = []
    unmatched_videos: list[EntryRecord] = []
    for video in sorted(completed.videos, key=lambda item: str(item.path)):
        if video.identity in media_ids or video.dev != media_device:
            continue
        exact_metadata_matches = sorted(
            media_by_name_size.get((video.path.name.casefold(), video.size), []),
            key=lambda item: str(item.path),
        )
        if exact_metadata_matches:
            for media_record in exact_metadata_matches:
                if media_record.dev != video.dev:
                    continue
                repairs.append(HardlinkRepair(
                    repair_id=len(repairs) + 1,
                    completed=video,
                    media=media_record,
                    similarity_score=1.0,
                    detection_method="exact_name_and_size",
                ))
            continue
        unmatched_videos.append(video)

    if not include_similar_names:
        return repairs

    engine = SimilarityEngine(media.videos)
    media_by_path = {entry.path: entry for entry in media.videos}
    for video in unmatched_videos:
        matches = engine.find(video, threshold, limit=10)
        exact_size = [match for match in matches if match.size == video.size]
        for match in exact_size:
            media_record = media_by_path.get(match.path)
            if media_record is None or media_record.dev != video.dev:
                continue
            if files_match_samples(
                    video.path, media_record.path, sample_size, sample_count):
                repairs.append(HardlinkRepair(
                    repair_id=len(repairs) + 1,
                    completed=video,
                    media=media_record,
                    similarity_score=match.score,
                    detection_method="similar_name_and_samples",
                ))
                break
    return repairs


def path_is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def snapshot_subtree(root: Path, extensions: frozenset[str]) -> tuple[
        dict[str, tuple[Any, ...]], list[ScanError]]:
    snapshots: dict[str, tuple[Any, ...]] = {}
    errors: list[ScanError] = []
    try:
        root_stat = os.lstat(root)
        root_record = make_entry(root, root, root_stat, extensions)
        if root_record.kind != "dir":
            return {}, [ScanError(str(root), "La cible n'est plus un dossier")]
        snapshots["."] = root_record.signature
    except OSError as exc:
        return {}, [ScanError(str(root), str(exc))]
    stack = [root]
    while stack:
        directory = stack.pop()
        try:
            with os.scandir(directory) as iterator:
                children = list(iterator)
        except OSError as exc:
            errors.append(ScanError(str(directory), str(exc)))
            continue
        for child in children:
            path = Path(child.path)
            try:
                st = child.stat(follow_symlinks=False)
                record = make_entry(path, root, st, extensions)
                snapshots[record.relative] = record.signature
                if record.kind == "dir":
                    stack.append(path)
            except OSError as exc:
                errors.append(ScanError(str(path), str(exc)))
    return snapshots, errors


def error_below(path: Path, errors: Sequence[ScanError]) -> bool:
    base = os.path.abspath(path)
    for error in errors:
        try:
            if os.path.commonpath((base, os.path.abspath(error.path))) == base:
                return True
        except ValueError:
            continue
    return False


def estimate_reclaimable(entries: Iterable[EntryRecord],
                         media_identities: set[Identity]) -> int:
    files = [entry for entry in entries if entry.kind == "file"]
    selected_counts = Counter(entry.identity for entry in files)
    representatives = {entry.identity: entry for entry in files}
    total = 0
    for identity, count in selected_counts.items():
        item = representatives[identity]
        if identity not in media_identities and count >= item.nlink:
            total += item.allocated
    return total


def build_deletion_candidates(completed: TreeScan, media: TreeScan,
                              threshold: float,
                              extensions: frozenset[str]) -> list[DeletionCandidate]:
    media_ids = {entry.identity for entry in media.files}
    orphan = [video for video in completed.videos if video.identity not in media_ids]
    orphan_paths = {video.path for video in orphan}
    all_videos = completed.videos

    videos_below: dict[Path, list[EntryRecord]] = defaultdict(list)
    for video in all_videos:
        parent = video.path.parent
        while parent != completed.root and path_is_within(parent, completed.root):
            videos_below[parent].append(video)
            parent = parent.parent

    qualifying: set[Path] = set()
    for directory, videos in videos_below.items():
        if videos and all(video.path in orphan_paths for video in videos):
            if not error_below(directory, completed.errors):
                qualifying.add(directory)

    qualifying_with_descendants: set[Path] = set()
    for directory in qualifying:
        parent = directory.parent
        while parent != completed.root and path_is_within(parent, completed.root):
            if parent in qualifying:
                qualifying_with_descendants.add(parent)
            parent = parent.parent
    leafmost = qualifying - qualifying_with_descendants

    engine = SimilarityEngine(media.videos)
    similarities = {
        str(video.path): engine.find(video, threshold) for video in orphan
    }
    candidates: list[DeletionCandidate] = []
    covered: set[Path] = set()
    entries_by_folder: dict[Path, list[EntryRecord]] = defaultdict(list)
    videos_by_folder: dict[Path, list[EntryRecord]] = defaultdict(list)
    for entry in completed.entries.values():
        cursor = entry.path if entry.path in leafmost else entry.path.parent
        while cursor != completed.root and path_is_within(cursor, completed.root):
            if cursor in leafmost:
                entries_by_folder[cursor].append(entry)
                break
            cursor = cursor.parent
    for video in orphan:
        cursor = video.path.parent
        while cursor != completed.root and path_is_within(cursor, completed.root):
            if cursor in leafmost:
                videos_by_folder[cursor].append(video)
                break
            cursor = cursor.parent

    for directory in sorted(leafmost, key=lambda path: str(path)):
        snapshot, errors = snapshot_subtree(directory, extensions)
        if errors:
            continue
        entries = entries_by_folder[directory]
        videos = videos_by_folder[directory]
        if not videos:
            continue
        covered.update(video.path for video in videos)
        candidate = DeletionCandidate(
            candidate_id=0,
            kind="folder",
            path=directory,
            videos=videos,
            entries=entries,
            snapshot=snapshot,
            similarities={str(video.path): similarities[str(video.path)] for video in videos},
        )
        candidate.estimated_reclaimable = estimate_reclaimable(entries, media_ids)
        candidates.append(candidate)

    for video in sorted(orphan, key=lambda item: str(item.path)):
        if video.path in covered:
            continue
        candidate = DeletionCandidate(
            candidate_id=0,
            kind="file",
            path=video.path,
            videos=[video],
            entries=[video],
            snapshot={".": video.signature},
            similarities={str(video.path): similarities[str(video.path)]},
        )
        candidate.estimated_reclaimable = estimate_reclaimable([video], media_ids)
        candidates.append(candidate)

    candidates.sort(key=lambda item: (str(item.path).casefold(), item.kind))
    for index, candidate in enumerate(candidates, 1):
        candidate.candidate_id = index
    return candidates


def atomic_write_text(path: Path, content: str) -> None:
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with temporary.open("w", encoding="utf-8", newline="") as handle:
        handle.write(content)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def write_json(path: Path, data: Any) -> None:
    atomic_write_text(path, json.dumps(data, ensure_ascii=False, indent=2) + "\n")


def append_jsonl(path: Path, event: dict[str, Any]) -> None:
    payload = dict(event)
    payload.setdefault("timestamp", iso_now())
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def parsed_to_dict(parsed: ParsedName) -> dict[str, Any]:
    return dataclasses.asdict(parsed)


def candidate_to_dict(candidate: DeletionCandidate) -> dict[str, Any]:
    return {
        "id": candidate.candidate_id,
        "kind": candidate.kind,
        "path": str(candidate.path),
        "logical_bytes": candidate.logical_size,
        "estimated_reclaimable_bytes": candidate.estimated_reclaimable,
        "videos": [
            {
                "path": str(video.path),
                "size": video.size,
                "allocated": video.allocated,
                "nlink": video.nlink,
                "similarities": [
                    {
                        "path": str(match.path),
                        "score": round(match.score, 4),
                        "size": match.size,
                        "parsed": parsed_to_dict(match.parsed),
                    }
                    for match in candidate.similarities.get(str(video.path), [])
                ],
            }
            for video in candidate.videos
        ],
    }


def repair_to_dict(repair: HardlinkRepair) -> dict[str, Any]:
    return {
        "id": repair.repair_id,
        "completed_path": str(repair.completed.path),
        "media_path": str(repair.media.path),
        "size": repair.completed.size,
        "allocated_reclaimable_candidate": repair.media.allocated,
        "completed_nlink": repair.completed.nlink,
        "media_nlink": repair.media.nlink,
        "direction": "completed_to_medias",
        "similarity_score": round(repair.similarity_score, 4),
        "detection_method": repair.detection_method,
        "quick_samples_match": repair.detection_method == "similar_name_and_samples",
        "full_content_verified": False,
    }


def auto_decision_to_dict(decision: AutoCandidateDecision) -> dict[str, Any]:
    return {
        "candidate_id": decision.candidate_id,
        "path": str(decision.path),
        "auto_eligible": decision.eligible,
        "auto_action": decision.action,
        "auto_reason": list(decision.reasons),
        "replacements": [
            {
                "completed_path": completed_path,
                "media_path": media_path,
                "score": round(score, 4),
            }
            for completed_path, media_path, score in decision.replacements
        ],
    }


def write_unlinked_inventory_reports(report_dir: Path, stamp: str,
                                     completed: TreeScan,
                                     media: TreeScan,
                                     threshold: float) -> tuple[Path, Path]:
    """Write bidirectional inventories of videos without a cross-tree hardlink."""
    completed_ids = {entry.identity for entry in completed.files}
    media_ids = {entry.identity for entry in media.files}
    completed_counts = Counter(entry.identity for entry in completed.files)
    media_counts = Counter(entry.identity for entry in media.files)
    completed_engine = SimilarityEngine(completed.videos)
    media_engine = SimilarityEngine(media.videos)

    def write_one(path: Path, videos: Sequence[EntryRecord],
                  opposite_ids: set[Identity], local_counts: Counter[Identity],
                  engine: SimilarityEngine) -> None:
        temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
        with temporary.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle)
            writer.writerow([
                "path", "logical_bytes", "allocated_bytes", "nlink",
                "links_seen_in_tree", "possible_external_links", "title",
                "year", "season", "episode", "resolution", "languages",
                "editions", "best_match_path", "best_match_score",
                "best_match_logical_bytes", "same_size",
            ])
            for video in sorted(videos, key=lambda item: str(item.path).casefold()):
                if video.identity in opposite_ids:
                    continue
                parsed = parse_scene_name(video.path)
                matches = engine.find(video, threshold, limit=1)
                best = matches[0] if matches else None
                season = parsed.episode[0] if parsed.episode else ""
                episode = parsed.episode[1] if parsed.episode else ""
                writer.writerow([
                    str(video.path), video.size, video.allocated, video.nlink,
                    local_counts[video.identity],
                    video.nlink > local_counts[video.identity],
                    " ".join(parsed.title_tokens), parsed.year or "",
                    season, episode, parsed.resolution or "",
                    "|".join(parsed.languages), "|".join(parsed.editions),
                    str(best.path) if best else "",
                    f"{best.score:.4f}" if best else "",
                    best.size if best else "",
                    best.size == video.size if best else "",
                ])
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)

    completed_path = report_dir / f"unlinked-completed-{stamp}.csv"
    media_path = report_dir / f"unlinked-medias-{stamp}.csv"
    write_one(
        completed_path, completed.videos, media_ids, completed_counts, media_engine
    )
    write_one(media_path, media.videos, completed_ids, media_counts, completed_engine)
    return completed_path, media_path


def write_initial_reports(report_dir: Path, stamp: str, config: Config,
                          statistics: dict[str, Any], completed: TreeScan,
                          media: TreeScan,
                          candidates: Sequence[DeletionCandidate],
                          repairs: Sequence[HardlinkRepair],
                          auto_decisions: Sequence[AutoCandidateDecision] = (),
                          ) -> tuple[Path, Path, Path, Path]:
    scan_path = report_dir / f"scan-{stamp}.json"
    csv_path = report_dir / f"candidates-{stamp}.csv"
    actions_path = report_dir / f"actions-{stamp}.jsonl"
    summary_path = report_dir / f"summary-{stamp}.txt"
    scan_data = {
        "created_at": iso_now(),
        "completed_path": str(config.completed_path),
        "media_path": str(config.media_path),
        "run_mode": config.run_mode,
        "automation": {
            "minimum_age_hours": config.auto_min_age_hours,
            "deletion_similarity_threshold": config.auto_delete_similarity_threshold,
            "remove_empty_torrents": config.auto_remove_empty_torrents,
        },
        "hardlink_detection": {
            "exact_name_and_size_without_content_read": True,
            "search_similar_names": config.search_similar_names_for_hardlink,
            "quick_sample_size": config.quick_sample_size,
            "quick_sample_count": config.quick_sample_count,
            "full_comparison_only_after_selection": True,
        },
        "statistics": statistics,
        "scan_errors": {
            "completed": [dataclasses.asdict(error) for error in completed.errors],
            "media": [dataclasses.asdict(error) for error in media.errors],
        },
        "hardlink_repairs": [repair_to_dict(repair) for repair in repairs],
        "candidates": [candidate_to_dict(candidate) for candidate in candidates],
    }
    write_json(scan_path, scan_data)

    temporary = csv_path.with_name(f".{csv_path.name}.tmp-{os.getpid()}")
    repair_targets = {
        str(repair.completed.path): str(repair.media.path) for repair in repairs
    }
    decisions_by_id = {decision.candidate_id: decision for decision in auto_decisions}
    with temporary.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow([
            "id", "type", "path", "video_count", "logical_bytes",
            "estimated_reclaimable_bytes", "has_unmatched_video", "similar_paths",
            "hardlink_target", "auto_eligible", "auto_action", "auto_reason",
        ])
        for candidate in candidates:
            decision = decisions_by_id.get(candidate.candidate_id)
            matches = [
                f"{match.path} ({match.score:.3f})"
                for video in candidate.videos
                for match in candidate.similarities.get(str(video.path), [])
            ]
            writer.writerow([
                candidate.candidate_id,
                candidate.kind,
                str(candidate.path),
                len(candidate.videos),
                candidate.logical_size,
                candidate.estimated_reclaimable,
                candidate.has_unmatched_video,
                " | ".join(matches),
                " | ".join(
                    repair_targets[str(video.path)] for video in candidate.videos
                    if str(video.path) in repair_targets
                ),
                decision.eligible if decision is not None else "",
                decision.action if decision is not None else "interactive",
                " | ".join(decision.reasons) if decision is not None else "mode_interactif",
            ])
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, csv_path)
    actions_path.touch(exist_ok=False)
    return scan_path, csv_path, actions_path, summary_path


def write_post_repair_report(report_dir: Path, stamp: str,
                             statistics: dict[str, Any], completed: TreeScan,
                             media: TreeScan,
                             candidates: Sequence[DeletionCandidate]) -> Path:
    path = report_dir / f"post-repair-{stamp}.json"
    write_json(path, {
        "created_at": iso_now(),
        "statistics": statistics,
        "scan_errors": {
            "completed": [dataclasses.asdict(error) for error in completed.errors],
            "media": [dataclasses.asdict(error) for error in media.errors],
        },
        "remaining_candidates": [candidate_to_dict(candidate) for candidate in candidates],
    })
    return path


def write_auto_plan(report_dir: Path, stamp: str, phase: str, config: Config,
                    repairs: Sequence[HardlinkRepair],
                    decisions: Sequence[AutoCandidateDecision],
                    unused_torrents: Sequence[QbitTorrent],
                    blocked_repair_paths: Iterable[str] = ()) -> Path:
    suffix = "" if phase == "initial" else f"-{phase}"
    path = report_dir / f"auto-plan-{stamp}{suffix}.json"
    write_json(path, {
        "created_at": iso_now(),
        "mode": config.run_mode,
        "phase": phase,
        "dry_run": config.run_mode == "dry-run",
        "criteria": {
            "minimum_age_hours": config.auto_min_age_hours,
            "similarity_threshold": config.auto_delete_similarity_threshold,
            "same_or_better_resolution": True,
            "compatible_language_required": True,
            "remove_empty_torrents": config.auto_remove_empty_torrents,
        },
        "repairs": [
            {
                **repair_to_dict(repair),
                "auto_eligible": True,
                "auto_action": "repair",
                "auto_reason": ["comparaison_integrale_requise_avant_action"],
            }
            for repair in repairs
        ],
        "deletions": [auto_decision_to_dict(decision) for decision in decisions],
        "blocked_repair_paths": sorted(set(blocked_repair_paths)),
        "torrents_predicted_unused": [
            {"hash": torrent.torrent_hash, "name": torrent.name}
            for torrent in unused_torrents
        ],
    })
    return path


class QbitError(RuntimeError):
    pass


class QbitClient:
    def __init__(self, base_url: str, username: str, password: str,
                 timeout: float = 20.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.username = username
        self.password = password
        self.timeout = timeout
        self.cookies = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.cookies)
        )
        self.version = ""

    def _request(self, endpoint: str, data: dict[str, Any] | None = None,
                 expect_json: bool = False) -> Any:
        body = None
        if data is not None:
            normalized = {
                key: ("true" if value is True else "false" if value is False else str(value))
                for key, value in data.items()
            }
            body = urllib.parse.urlencode(normalized).encode("utf-8")
        url = f"{self.base_url}/api/v2/{endpoint.lstrip('/')}"
        request = urllib.request.Request(url, data=body)
        request.add_header("Referer", self.base_url + "/")
        request.add_header("Origin", self.base_url)
        try:
            with self.opener.open(request, timeout=self.timeout) as response:
                content = response.read().decode("utf-8", "replace")
        except (OSError, urllib.error.URLError, urllib.error.HTTPError) as exc:
            raise QbitError(f"Échec API qBittorrent ({endpoint}) : {exc}") from exc
        if expect_json:
            try:
                return json.loads(content or "null")
            except json.JSONDecodeError as exc:
                raise QbitError(f"Réponse JSON invalide pour {endpoint}") from exc
        return content

    def login(self) -> None:
        response = self._request("auth/login", {
            "username": self.username,
            "password": self.password,
        })
        login_body = response.strip()
        # qBittorrent <= 5.1 répond "Ok.". Depuis la version 5.2, une
        # authentification réussie répond 204 No Content et le corps est vide.
        # Dans les deux cas, la validation fiable consiste à utiliser le cookie
        # reçu puis à interroger un endpoint protégé.
        if login_body not in {"", "Ok."}:
            raise QbitError(f"Authentification refusée : {login_body}")

        try:
            version = self._request("app/version").strip()
        except QbitError as exc:
            raise QbitError(
                "L'authentification qBittorrent n'a pas pu être confirmée "
                "avec /api/v2/app/version"
            ) from exc

        if not re.match(r"^v?\d+(?:\.\d+)+", version):
            raise QbitError(
                "Authentification qBittorrent non confirmée : "
                f"version inattendue ({version or 'réponse vide'})"
            )
        self.version = version

    @property
    def major_version(self) -> int:
        match = re.match(r"v?(\d+)", self.version)
        return int(match.group(1)) if match else 0

    def torrents_info(self) -> list[dict[str, Any]]:
        value = self._request("torrents/info", expect_json=True)
        if not isinstance(value, list):
            raise QbitError("La liste des torrents n'est pas valide")
        return value

    def torrent_files(self, torrent_hash: str) -> list[dict[str, Any]]:
        value = self._request(
            "torrents/files?" + urllib.parse.urlencode({"hash": torrent_hash}),
            expect_json=True,
        )
        if not isinstance(value, list):
            raise QbitError(f"Contenu invalide pour le torrent {torrent_hash}")
        return value

    def stop(self, hashes: Iterable[str]) -> None:
        joined = "|".join(sorted(set(hashes)))
        if not joined:
            return
        endpoint = "torrents/stop" if self.major_version >= 5 else "torrents/pause"
        self._request(endpoint, {"hashes": joined})

    def start(self, hashes: Iterable[str]) -> None:
        joined = "|".join(sorted(set(hashes)))
        if not joined:
            return
        endpoint = "torrents/start" if self.major_version >= 5 else "torrents/resume"
        self._request(endpoint, {"hashes": joined})

    def recheck(self, hashes: Iterable[str]) -> None:
        joined = "|".join(sorted(set(hashes)))
        if joined:
            self._request("torrents/recheck", {"hashes": joined})

    def delete(self, hashes: Iterable[str]) -> None:
        joined = "|".join(sorted(set(hashes)))
        if joined:
            self._request("torrents/delete", {"hashes": joined, "deleteFiles": False})


def map_container_path(container_value: str, container_root: str,
                       host_root: Path) -> Path:
    normalized_root = posixpath.normpath(container_root)
    normalized_value = posixpath.normpath(container_value)
    try:
        relative = PurePosixPath(normalized_value).relative_to(PurePosixPath(normalized_root))
    except ValueError as exc:
        raise QbitError(
            f"Chemin qBittorrent hors mapping : {container_value!r} n'est pas sous {container_root!r}"
        ) from exc
    return host_root.joinpath(*relative.parts)


def lexical_key(path: Path) -> str:
    return os.path.normcase(os.path.abspath(path))


def choose_qbit_file_path(info: dict[str, Any], file_data: dict[str, Any],
                          config: Config, known_paths: set[str],
                          file_count: int) -> Path:
    name = str(file_data.get("name", ""))
    save_path = str(info.get("save_path", ""))
    content_path = str(info.get("content_path", ""))
    possibilities: list[Path] = []
    if save_path and name:
        possibilities.append(map_container_path(
            posixpath.join(save_path, name),
            config.qbittorrent_container_path,
            config.qbittorrent_host_path,
        ))
    if content_path:
        mapped_content = map_container_path(
            content_path,
            config.qbittorrent_container_path,
            config.qbittorrent_host_path,
        )
        if file_count == 1:
            possibilities.append(mapped_content)
        if name:
            parts = PurePosixPath(name).parts
            if parts and parts[0] == mapped_content.name:
                possibilities.append(mapped_content.joinpath(*parts[1:]))
            else:
                possibilities.append(mapped_content.joinpath(*parts))
    for possibility in possibilities:
        if lexical_key(possibility) in known_paths:
            return possibility
    if not possibilities:
        raise QbitError(f"Impossible de reconstruire le chemin du fichier qBittorrent {name!r}")
    return possibilities[0]


def build_qbit_index(client: QbitClient, config: Config,
                     completed: TreeScan) -> QbitIndex:
    known_paths = {lexical_key(entry.path) for entry in completed.files}
    completed_root = config.completed_path.resolve()
    torrents: dict[str, QbitTorrent] = {}
    path_to_hashes: dict[str, set[str]] = defaultdict(set)
    mapping_errors: list[str] = []
    for info in client.torrents_info():
        torrent_hash = str(info.get("hash", ""))
        if not torrent_hash:
            continue
        try:
            save_host = map_container_path(
                str(info.get("save_path", "")),
                config.qbittorrent_container_path,
                config.qbittorrent_host_path,
            )
            content_host = map_container_path(
                str(info.get("content_path", info.get("save_path", ""))),
                config.qbittorrent_container_path,
                config.qbittorrent_host_path,
            )
        except QbitError:
            continue
        if not (path_is_within(save_host, completed_root)
                or path_is_within(content_host, completed_root)
                or path_is_within(completed_root, save_host)):
            continue
        raw_files = client.torrent_files(torrent_hash)
        files: list[QbitFile] = []
        for raw_file in raw_files:
            try:
                host_path = choose_qbit_file_path(
                    info, raw_file, config, known_paths, len(raw_files)
                )
            except QbitError as exc:
                mapping_errors.append(f"{torrent_hash}: {exc}")
                continue
            if not path_is_within(host_path, completed_root):
                continue
            qbit_file = QbitFile(
                path=host_path,
                name=str(raw_file.get("name", "")),
                size=int(raw_file.get("size", 0)),
                priority=int(raw_file.get("priority", 1)),
            )
            files.append(qbit_file)
            path_to_hashes[lexical_key(host_path)].add(torrent_hash)
        if raw_files and not files and path_is_within(content_host, completed_root):
            mapping_errors.append(
                f"{torrent_hash}: aucun fichier du torrent situé dans completed n'a pu être mappé"
            )
        torrents[torrent_hash] = QbitTorrent(
            torrent_hash=torrent_hash,
            name=str(info.get("name", torrent_hash)),
            state=str(info.get("state", "unknown")),
            files=files,
        )
    return QbitIndex(
        torrents=torrents,
        path_to_hashes=dict(path_to_hashes),
        mapping_errors=mapping_errors,
    )


def print_statistics(statistics: dict[str, Any], free_bytes: int) -> None:
    print("\n=== Statistiques du disque ===")
    for label, key in (("completed", "completed"), ("medias", "media")):
        values = statistics[key]
        print(
            f"{label:10} : {values['files']:,} fichiers, {values['videos']:,} vidéos, "
            f"logique {human_size(values['logical_bytes'])}, "
            f"alloué unique {human_size(values['allocated_unique_bytes'])}"
        )
    print(f"Inodes partagés completed/medias : {statistics['shared_inodes']:,}")
    print(f"Espace partagé                  : {human_size(statistics['shared_allocated_bytes'])}")
    print(
        "Récupérable (vidéos orphelines): "
        f"{human_size(statistics['orphan_videos_reclaimable_bytes'])}"
    )
    print(
        "Récupérable (completed exclusif): "
        f"{human_size(statistics['completed_exclusive_reclaimable_bytes'])}"
    )
    print(f"Espace libre actuel             : {human_size(free_bytes)}")


def candidate_line(candidate: DeletionCandidate, selected: bool) -> str:
    marker = "*" if selected else " "
    kind = "DOSSIER" if candidate.kind == "folder" else "FICHIER"
    risk = "SANS REMPLAÇANT" if candidate.has_unmatched_video else "similaire trouvé"
    return (
        f"{marker} {candidate.candidate_id:4d} [{kind:7}] "
        f"{human_size(candidate.logical_size):>10} / gain {human_size(candidate.estimated_reclaimable):>10} "
        f"| {len(candidate.videos)} MKV | {risk} | {candidate.path}"
    )


def show_candidate_details(candidate: DeletionCandidate) -> None:
    print("\n" + candidate_line(candidate, False))
    print(f"Contenu supprimé ({len(candidate.entries)} entrées indexées) :")
    for entry in sorted(candidate.entries, key=lambda item: str(item.path)):
        print(f"  - [{entry.kind}] {entry.path}")
    print("Correspondances possibles :")
    for video in candidate.videos:
        print(f"  {video.path} ({human_size(video.size)}, nlink={video.nlink})")
        matches = candidate.similarities.get(str(video.path), [])
        if not matches:
            print("      AUCUN REMPLAÇANT SIMILAIRE")
        for match in matches:
            metadata = []
            if match.parsed.year:
                metadata.append(str(match.parsed.year))
            if match.parsed.episode:
                metadata.append(f"S{match.parsed.episode[0]:02d}E{match.parsed.episode[1]:02d}")
            if match.parsed.resolution:
                metadata.append(match.parsed.resolution)
            if match.parsed.codec:
                metadata.append(match.parsed.codec)
            if match.parsed.source:
                metadata.append(match.parsed.source)
            suffix = ", ".join(metadata)
            print(
                f"      {match.score:.3f} | {human_size(match.size):>10} | "
                f"{match.path}{' | ' + suffix if suffix else ''}"
            )


def parse_id_expression(expression: str, valid_ids: set[int]) -> set[int]:
    result: set[int] = set()
    for piece in expression.replace(" ", "").split(","):
        if not piece:
            continue
        if "-" in piece:
            start_text, end_text = piece.split("-", 1)
            start, end = int(start_text), int(end_text)
            if start > end:
                start, end = end, start
            result.update(value for value in range(start, end + 1) if value in valid_ids)
        else:
            value = int(piece)
            if value in valid_ids:
                result.add(value)
    return result


def interactive_selection(candidates: Sequence[DeletionCandidate],
                          page_size: int = 25) -> list[DeletionCandidate]:
    if not candidates:
        print("\nAucun MKV orphelin détecté.")
        return []
    selected: set[int] = set()
    page = 0
    filter_name = "all"
    by_id = {candidate.candidate_id: candidate for candidate in candidates}
    while True:
        if filter_name == "folder":
            visible = [item for item in candidates if item.kind == "folder"]
        elif filter_name == "file":
            visible = [item for item in candidates if item.kind == "file"]
        elif filter_name == "matched":
            visible = [item for item in candidates if not item.has_unmatched_video]
        elif filter_name == "unmatched":
            visible = [item for item in candidates if item.has_unmatched_video]
        else:
            visible = list(candidates)
        page_count = max(1, math.ceil(len(visible) / page_size))
        page = min(page, page_count - 1)
        start = page * page_size
        print(f"\n=== Candidats — filtre {filter_name}, page {page + 1}/{page_count} ===")
        for candidate in visible[start:start + page_size]:
            print(candidate_line(candidate, candidate.candidate_id in selected))
        print(
            f"Sélection : {len(selected)} élément(s). Commandes : "
            "n/p, s IDS, u IDS, d ID, f all|folder|file|matched|unmatched, x, q"
        )
        command = input("> ").strip()
        if command == "n":
            page = min(page + 1, page_count - 1)
        elif command == "p":
            page = max(page - 1, 0)
        elif command.startswith("s "):
            try:
                selected.update(parse_id_expression(command[2:], set(by_id)))
            except ValueError:
                print("Sélection invalide.")
        elif command.startswith("u "):
            try:
                selected.difference_update(parse_id_expression(command[2:], set(by_id)))
            except ValueError:
                print("Sélection invalide.")
        elif command.startswith("d "):
            try:
                item = by_id[int(command[2:].strip())]
            except (ValueError, KeyError):
                print("Identifiant invalide.")
            else:
                show_candidate_details(item)
        elif command.startswith("f "):
            requested = command[2:].strip().casefold()
            if requested in {"all", "folder", "file", "matched", "unmatched"}:
                filter_name = requested
                page = 0
            else:
                print("Filtre invalide.")
        elif command == "x":
            chosen = [by_id[value] for value in sorted(selected)]
            if not chosen:
                print("La sélection est vide.")
                continue
            if any(item.has_unmatched_video for item in chosen):
                phrase = input("Tapez SUPPRIMER SANS REMPLACANT pour confirmer le risque : ")
                if phrase != "SUPPRIMER SANS REMPLACANT":
                    print("Confirmation refusée.")
                    continue
            phrase = input(f"Tapez SUPPRIMER {len(chosen)} ELEMENTS pour lancer la suppression : ")
            if phrase == f"SUPPRIMER {len(chosen)} ELEMENTS":
                return chosen
            print("Confirmation refusée.")
        elif command == "q":
            return []
        else:
            print("Commande inconnue.")


def interactive_repair_selection(repairs: Sequence[HardlinkRepair],
                                 page_size: int = 25) -> list[HardlinkRepair]:
    if not repairs:
        return []
    selected: set[int] = set()
    by_id = {repair.repair_id: repair for repair in repairs}
    page = 0
    page_count = max(1, math.ceil(len(repairs) / page_size))
    while True:
        page = min(page, page_count - 1)
        start = page * page_size
        print(f"\n=== Réparations de hardlinks — page {page + 1}/{page_count} ===")
        print("Le fichier completed reste la référence ; la copie medias sera remplacée par son hardlink.")
        print("Même nom + taille ne lit aucun contenu ; la preuve octet par octet vient après sélection.")
        for repair in repairs[start:start + page_size]:
            marker = "*" if repair.repair_id in selected else " "
            method = (
                "même nom + même taille (métadonnées seulement)"
                if repair.detection_method == "exact_name_and_size"
                else "nom similaire + échantillons concordants"
            )
            print(
                f"{marker} {repair.repair_id:4d} [RELIER] {human_size(repair.completed.size):>10} "
                f"| score {repair.similarity_score:.3f} | {method}\n"
                f"       completed : {repair.completed.path}\n"
                f"       medias    : {repair.media.path}"
            )
        print(
            f"Sélection : {len(selected)} élément(s). Commandes : "
            "n/p, s IDS, u IDS, d ID, x, q (ignorer les réparations)"
        )
        command = input("> ").strip()
        if command == "n":
            page = min(page + 1, page_count - 1)
        elif command == "p":
            page = max(page - 1, 0)
        elif command.startswith("s "):
            try:
                selected.update(parse_id_expression(command[2:], set(by_id)))
            except ValueError:
                print("Sélection invalide.")
        elif command.startswith("u "):
            try:
                selected.difference_update(parse_id_expression(command[2:], set(by_id)))
            except ValueError:
                print("Sélection invalide.")
        elif command.startswith("d "):
            try:
                repair = by_id[int(command[2:].strip())]
            except (ValueError, KeyError):
                print("Identifiant invalide.")
            else:
                print(json.dumps(repair_to_dict(repair), ensure_ascii=False, indent=2))
        elif command == "x":
            chosen = [by_id[value] for value in sorted(selected)]
            if not chosen:
                print("La sélection est vide.")
                continue
            phrase = input(f"Tapez CREER {len(chosen)} HARDLINKS pour confirmer : ")
            if phrase == f"CREER {len(chosen)} HARDLINKS":
                return chosen
            print("Confirmation refusée.")
        elif command == "q":
            return []
        else:
            print("Commande inconnue.")


def record_still_matches(record: EntryRecord) -> tuple[bool, str]:
    try:
        current = os.lstat(record.path)
    except OSError as exc:
        return False, str(exc)
    kind = entry_kind(current.st_mode)
    signature = (
        kind,
        current.st_dev,
        current.st_ino,
        current.st_size if kind == "file" else 0,
        current.st_nlink,
        current.st_mtime_ns,
        current.st_mode,
    )
    if signature != record.signature:
        return False, "Le fichier a changé depuis le scan"
    return True, ""


def repair_record_still_matches(record: EntryRecord) -> tuple[bool, str]:
    """Like record_still_matches, but allow nlink changes caused by this repair batch."""
    try:
        current = os.lstat(record.path)
    except OSError as exc:
        return False, str(exc)
    kind = entry_kind(current.st_mode)
    expected = (
        record.kind, record.dev, record.ino, record.size,
        record.mtime_ns, record.mode,
    )
    actual = (
        kind, current.st_dev, current.st_ino,
        current.st_size if kind == "file" else 0,
        current.st_mtime_ns, current.st_mode,
    )
    if actual != expected:
        return False, "Le fichier a changé depuis le scan"
    return True, ""


def revalidate_candidate(candidate: DeletionCandidate,
                         extensions: frozenset[str]) -> tuple[bool, str]:
    if candidate.kind == "folder":
        current, errors = snapshot_subtree(candidate.path, extensions)
        if errors:
            return False, "; ".join(error.message for error in errors)
        if current != candidate.snapshot:
            return False, "Le contenu ou les métadonnées du dossier ont changé depuis le scan"
        return True, ""
    try:
        st = os.lstat(candidate.path)
        current = make_entry(candidate.path, candidate.path, st, extensions).signature
    except OSError as exc:
        return False, str(exc)
    if current != candidate.snapshot["."]:
        return False, "Le fichier a changé depuis le scan"
    return True, ""


def ensure_delete_target(path: Path, completed_root: Path,
                         allow_symlink: bool = False) -> None:
    root = completed_root.resolve(strict=True)
    if allow_symlink and path.is_symlink():
        resolved_parent = path.parent.resolve(strict=True)
        if not path_is_within(resolved_parent, root):
            raise RuntimeError(f"Cible hors completed : {path}")
        return
    resolved = path.resolve(strict=True)
    if resolved == root or not path_is_within(resolved, root):
        raise RuntimeError(f"Cible refusée hors completed : {path}")


def ensure_media_target(path: Path, media_root: Path) -> None:
    root = media_root.resolve(strict=True)
    resolved = path.resolve(strict=True)
    if not path_is_within(resolved, root) or not resolved.is_file():
        raise RuntimeError(f"Cible medias refusée : {path}")


def verify_hardlink_creation_allowed(source: Path, temporary: Path) -> None:
    """Test hardlink permissions without retaining or replacing any media file."""
    try:
        os.link(source, temporary, follow_symlinks=False)
        source_stat = os.stat(source, follow_symlinks=False)
        link_stat = os.stat(temporary, follow_symlinks=False)
        if ((source_stat.st_dev, source_stat.st_ino)
                != (link_stat.st_dev, link_stat.st_ino)):
            raise RuntimeError("Le test de hardlink n'a pas créé le même inode")
        temporary.unlink()
    except OSError as exc:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        if exc.errno == errno.EPERM:
            try:
                source_stat = os.stat(source, follow_symlinks=False)
                owner_detail = (
                    f"UID du script={os.geteuid()}, "
                    f"UID propriétaire du fichier={source_stat.st_uid}"
                )
            except OSError:
                owner_detail = f"UID du script={os.geteuid()}"
            raise PermissionError(
                "création de hardlink interdite par le système (EPERM ; "
                f"{owner_detail}). Causes probables : protection "
                "fs.protected_hardlinks, ACL Synology, ou fichier "
                "immutable/WriteOnce. Relancez avec un compte autorisé "
                "(éventuellement sudo) après vérification des droits"
            ) from exc
        raise


def create_hardlink_repair(repair: HardlinkRepair, completed_root: Path,
                           media_root: Path, actions_path: Path,
                           full_compare_chunk_size: int = 16 * 1024 * 1024,
                           show_progress: bool = False) -> bool:
    """Atomically replace the media copy with a link to the completed inode."""
    append_jsonl(actions_path, {
        "event": "hardlink_intent",
        "completed_path": str(repair.completed.path),
        "media_path": str(repair.media.path),
        "completed_source_identity": list(repair.completed.identity),
        "old_media_identity": list(repair.media.identity),
        "direction": "completed_to_medias",
        "size": repair.completed.size,
    })
    temporary = repair.media.path.with_name(
        f".{repair.media.path.name}.doublon-checker-link-{os.getpid()}-{time.time_ns()}"
    )
    replaced = False
    try:
        ensure_delete_target(repair.completed.path, completed_root)
        ensure_media_target(repair.media.path, media_root)
        completed_valid, completed_reason = repair_record_still_matches(repair.completed)
        media_valid, media_reason = repair_record_still_matches(repair.media)
        if not completed_valid:
            raise RuntimeError(f"completed : {completed_reason}")
        if not media_valid:
            raise RuntimeError(f"medias : {media_reason}")
        if repair.completed.dev != repair.media.dev:
            raise RuntimeError("Les deux fichiers ne sont pas sur le même système de fichiers")

        # Tester l'autorisation avant de lire intégralement plusieurs gigaoctets.
        # Le lien de test est supprimé immédiatement et le fichier medias reste intact.
        verify_hardlink_creation_allowed(repair.completed.path, temporary)
        if show_progress:
            print(
                "Comparaison exacte avant hardlink :\n"
                f"  completed : {repair.completed.path}\n"
                f"  medias    : {repair.media.path}"
            )
        if not files_are_identical(
                repair.completed.path, repair.media.path,
                full_compare_chunk_size, show_progress):
            raise RuntimeError("Le contenu des deux fichiers n'est plus identique")
        completed_valid, completed_reason = repair_record_still_matches(repair.completed)
        media_valid, media_reason = repair_record_still_matches(repair.media)
        if not completed_valid or not media_valid:
            raise RuntimeError(
                "Un fichier a changé pendant la comparaison : "
                f"{completed_reason or media_reason}"
            )

        os.link(repair.completed.path, temporary, follow_symlinks=False)
        os.replace(temporary, repair.media.path)
        replaced = True
        completed_now = os.stat(repair.completed.path, follow_symlinks=False)
        media_now = os.stat(repair.media.path, follow_symlinks=False)
        if (completed_now.st_dev, completed_now.st_ino) != (media_now.st_dev, media_now.st_ino):
            raise RuntimeError("La vérification de l'inode après remplacement a échoué")
        try:
            flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
            directory_fd = os.open(repair.media.path.parent, flags)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        except OSError:
            pass
        append_jsonl(actions_path, {
            "event": "hardlink_success",
            "completed_path": str(repair.completed.path),
            "media_path": str(repair.media.path),
            "new_identity": [completed_now.st_dev, completed_now.st_ino],
            "new_nlink": completed_now.st_nlink,
            "full_content_verified": True,
        })
        return True
    except Exception as exc:
        if not replaced:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
        append_jsonl(actions_path, {
            "event": "hardlink_failure",
            "completed_path": str(repair.completed.path),
            "media_path": str(repair.media.path),
            "replaced": replaced,
            "error": repr(exc),
        })
        print(f"ERREUR pendant la création du hardlink {repair.completed.path} : {exc}")
        return False


def delete_candidate(candidate: DeletionCandidate, completed_root: Path,
                     actions_path: Path) -> tuple[list[EntryRecord], int, int, bool]:
    removed_records: list[EntryRecord] = []
    file_count = 0
    directory_count = 0
    removed_paths: list[str] = []
    append_jsonl(actions_path, {
        "event": "delete_intent",
        "candidate_id": candidate.candidate_id,
        "kind": candidate.kind,
        "path": str(candidate.path),
        "entries": [str(entry.path) for entry in candidate.entries],
    })
    try:
        if candidate.kind == "file":
            ensure_delete_target(candidate.path, completed_root)
            candidate.path.unlink()
            removed_records.extend(candidate.entries)
            file_count = 1
            removed_paths.append(str(candidate.path))
        else:
            ensure_delete_target(candidate.path, completed_root)
            records_by_path = {lexical_key(entry.path): entry for entry in candidate.entries}
            non_dirs = [
                (relative, signature) for relative, signature in candidate.snapshot.items()
                if relative != "." and signature[0] != "dir"
            ]
            for relative, _signature in sorted(
                    non_dirs, key=lambda item: len(Path(item[0]).parts), reverse=True):
                target = candidate.path / relative
                ensure_delete_target(target, completed_root, allow_symlink=True)
                target.unlink()
                file_count += 1
                removed_paths.append(str(target))
                record = records_by_path.get(lexical_key(target))
                if record and record.kind == "file":
                    removed_records.append(record)
            dirs = [
                relative for relative, signature in candidate.snapshot.items()
                if relative != "." and signature[0] == "dir"
            ]
            for relative in sorted(dirs, key=lambda value: len(Path(value).parts), reverse=True):
                target = candidate.path / relative
                ensure_delete_target(target, completed_root)
                target.rmdir()
                directory_count += 1
            candidate.path.rmdir()
            directory_count += 1
        append_jsonl(actions_path, {
            "event": "delete_success",
            "candidate_id": candidate.candidate_id,
            "path": str(candidate.path),
            "files_deleted": file_count,
            "directories_deleted": directory_count,
            "deleted_paths": removed_paths,
        })
        return removed_records, file_count, directory_count, True
    except Exception as exc:
        append_jsonl(actions_path, {
            "event": "delete_failure",
            "candidate_id": candidate.candidate_id,
            "path": str(candidate.path),
            "error": repr(exc),
            "files_deleted_before_failure": file_count,
            "directories_deleted_before_failure": directory_count,
            "deleted_paths": removed_paths,
        })
        print(f"ERREUR pendant la suppression de {candidate.path} : {exc}")
        return removed_records, file_count, directory_count, False


def affected_hashes_for_candidates(candidates: Sequence[DeletionCandidate],
                                    qbit_index: QbitIndex) -> set[str]:
    hashes: set[str] = set()
    for candidate in candidates:
        for entry in candidate.entries:
            if entry.kind == "file":
                hashes.update(qbit_index.path_to_hashes.get(lexical_key(entry.path), ()))
    return hashes


def file_paths_for_candidates(candidates: Sequence[DeletionCandidate]) -> set[str]:
    return {
        lexical_key(entry.path)
        for candidate in candidates
        for entry in candidate.entries
        if entry.kind == "file"
    }


def find_unused_torrents(qbit_index: QbitIndex,
                         planned_removed_paths: set[str] | None = None,
                         only_hashes: set[str] | None = None) -> list[QbitTorrent]:
    planned = planned_removed_paths or set()
    unused: list[QbitTorrent] = []
    hashes = only_hashes if only_hashes is not None else set(qbit_index.torrents)
    for torrent_hash in sorted(hashes):
        torrent = qbit_index.torrents.get(torrent_hash)
        if torrent is None:
            continue
        wanted = [item for item in torrent.files if item.priority > 0]
        if wanted and all(
                lexical_key(item.path) in planned or not os.path.lexists(item.path)
                for item in wanted):
            unused.append(torrent)
    return unused


def torrent_was_active(state: str) -> bool:
    lowered = state.casefold()
    return not (lowered.startswith("paused") or lowered.startswith("stopped"))


def wait_for_recheck(client: QbitClient, hashes: set[str], timeout_seconds: int) -> bool:
    if not hashes:
        return True
    deadline = time.monotonic() + timeout_seconds
    started_at = time.monotonic()
    checking_seen = False
    while time.monotonic() < deadline:
        current = {
            str(item.get("hash", "")): str(item.get("state", ""))
            for item in client.torrents_info()
        }
        checking = {
            torrent_hash for torrent_hash in hashes
            if current.get(torrent_hash, "").casefold().startswith("checking")
        }
        if checking:
            checking_seen = True
            print(f"Recheck qBittorrent en cours : {len(checking)} torrent(s)...")
            time.sleep(2)
            continue
        if checking_seen or (
            time.monotonic() - started_at >= 2
            and all(torrent_hash in current for torrent_hash in hashes)
        ):
            return True
        time.sleep(1)
    return False


def select_unused_torrents(unused: Sequence[QbitTorrent]) -> set[str]:
    if not unused:
        print("Aucun torrent entièrement dépourvu de données n'a été trouvé.")
        return set()
    print("\n=== Torrents devenus inutilisés ===")
    for index, torrent in enumerate(unused, 1):
        print(f"{index:4d} | {torrent.torrent_hash} | {torrent.name}")
    raw = input("Hashes à retirer (numéros/plages, entrée vide pour aucun) : ").strip()
    if not raw:
        return set()
    try:
        indexes = parse_id_expression(raw, set(range(1, len(unused) + 1)))
    except ValueError:
        print("Sélection invalide : aucun torrent retiré.")
        return set()
    selected = {unused[index - 1].torrent_hash for index in indexes}
    phrase = input(f"Tapez RETIRER {len(selected)} TORRENTS pour confirmer : ")
    if phrase != f"RETIRER {len(selected)} TORRENTS":
        print("Confirmation refusée.")
        return set()
    return selected


def current_free(path: Path) -> int:
    return shutil.disk_usage(path).free


def record_stopped_torrents(result: DeletionResult, hashes: Iterable[str]) -> None:
    combined = set(result.stopped_torrent_hashes)
    combined.update(hashes)
    result.stopped_torrent_hashes = sorted(combined)
    result.torrents_left_stopped = len(combined)


def clear_stopped_torrents(result: DeletionResult, hashes: Iterable[str]) -> None:
    remaining = set(result.stopped_torrent_hashes) - set(hashes)
    result.stopped_torrent_hashes = sorted(remaining)
    result.torrents_left_stopped = len(remaining)


def cumulative_gain(report_dir: Path, current_actions: Path) -> int:
    total = 0
    for path in report_dir.glob("actions-*.jsonl"):
        if path == current_actions:
            continue
        try:
            with path.open("r", encoding="utf-8") as handle:
                for line in handle:
                    event = json.loads(line)
                    if event.get("event") == "session_summary":
                        total += int(event.get("attributable_gain", 0))
        except (OSError, ValueError, json.JSONDecodeError):
            continue
    return total


def build_summary_text(result: DeletionResult, free_before: int,
                       cumulative: int) -> str:
    return "\n".join([
        "=== Bilan du nettoyage ===",
        f"Espace libre avant               : {human_size(free_before)}",
        f"Espace supprimé logiquement      : {human_size(result.logical_deleted)}",
        f"Gain attribuable estimé          : {human_size(result.attributable_gain)}",
        f"  dont réparation des hardlinks : {human_size(result.hardlink_gain)}",
        f"  dont suppressions              : {human_size(result.attributable_gain - result.hardlink_gain)}",
        f"Gain libre observé immédiatement : {human_size(result.observed_immediate_gain)}",
        f"Gain observé après qBittorrent   : {human_size(result.observed_final_gain)}",
        f"Fichiers supprimés               : {result.files_deleted}",
        f"Dossiers supprimés               : {result.directories_deleted}",
        f"Torrents retirés                 : {result.torrents_removed}",
        f"Torrents laissés arrêtés         : {result.torrents_left_stopped}",
        f"Hardlinks réparés                : {result.hardlinks_created}",
        f"Réparations en échec             : {result.failed_repairs}",
        f"Candidats en échec               : {result.failed_candidates}",
        f"Gain attribuable cumulé          : {human_size(cumulative + result.attributable_gain)}",
        "",
        "Note : la variation observée peut être influencée par les snapshots,",
        "les fichiers ouverts et les autres écritures réalisées sur le volume.",
        "",
    ])


def save_session_summary(result: DeletionResult, report_dir: Path,
                         actions_path: Path, summary_path: Path,
                         free_before: int, free_after: int,
                         reason: str | None = None) -> str:
    previous = cumulative_gain(report_dir, actions_path)
    summary = build_summary_text(result, free_before, previous)
    if reason:
        summary += f"Statut : {reason}\n"
    atomic_write_text(summary_path, summary)
    event: dict[str, Any] = {
        "event": "session_summary",
        **dataclasses.asdict(result),
        "free_before": free_before,
        "free_after": free_after,
    }
    if reason:
        event["reason"] = reason
    append_jsonl(actions_path, event)
    return summary


def execute_hardlink_repairs(config: Config, repairs: Sequence[HardlinkRepair],
                             client: QbitClient, qbit_index: QbitIndex,
                             actions_path: Path, session_free_before: int) -> DeletionResult:
    result = DeletionResult()
    affected: set[str] = set()
    for repair in repairs:
        affected.update(qbit_index.path_to_hashes.get(lexical_key(repair.completed.path), ()))
    previously_active = {
        torrent_hash for torrent_hash in affected
        if torrent_hash in qbit_index.torrents
        and torrent_was_active(qbit_index.torrents[torrent_hash].state)
    }
    print(f"Mise à l'arrêt de {len(affected)} torrent(s) avant réparation...")
    client.stop(affected)

    validation_errors: list[str] = []
    for repair in repairs:
        completed_valid, completed_reason = record_still_matches(repair.completed)
        media_valid, media_reason = record_still_matches(repair.media)
        if not completed_valid:
            validation_errors.append(f"{repair.completed.path}: {completed_reason}")
        if not media_valid:
            validation_errors.append(f"{repair.media.path}: {media_reason}")
    if validation_errors:
        if previously_active:
            client.start(previously_active)
        for error in validation_errors:
            print(f"Réparation refusée — {error}")
        append_jsonl(actions_path, {
            "event": "hardlink_batch_aborted",
            "errors": validation_errors,
        })
        result.failed_repairs = len(repairs)
        return result

    successful: list[HardlinkRepair] = []
    try:
        for repair in repairs:
            if create_hardlink_repair(
                    repair, config.completed_path, config.media_path, actions_path,
                    config.full_compare_chunk_size, True):
                successful.append(repair)
                result.hardlinks_created += 1
            else:
                result.failed_repairs += 1
    except KeyboardInterrupt:
        modified_hashes = {
            torrent_hash
            for repair in successful
            for torrent_hash in qbit_index.path_to_hashes.get(
                lexical_key(repair.completed.path), ()
            )
        }
        to_resume = previously_active - modified_hashes
        if to_resume:
            try:
                client.start(to_resume)
            except QbitError as exc:
                append_jsonl(actions_path, {
                    "event": "hardlink_interrupt_resume_failure",
                    "hashes": sorted(to_resume),
                    "error": str(exc),
                })
        append_jsonl(actions_path, {
            "event": "hardlink_batch_interrupted",
            "successful_repairs": [repair.repair_id for repair in successful],
            "modified_hashes_left_stopped": sorted(modified_hashes),
            "unmodified_hashes_resumed": sorted(to_resume),
        })
        raise

    repaired_records = [repair.media for repair in successful]
    result.hardlink_gain = estimate_reclaimable(repaired_records, set())
    result.attributable_gain = result.hardlink_gain
    result.observed_immediate_gain = current_free(config.completed_path) - session_free_before

    repaired_hashes: set[str] = set()
    for repair in successful:
        repaired_hashes.update(
            qbit_index.path_to_hashes.get(lexical_key(repair.completed.path), ())
        )
    unmodified_active = previously_active - repaired_hashes
    if unmodified_active:
        try:
            client.start(unmodified_active)
        except QbitError as exc:
            append_jsonl(actions_path, {
                "event": "hardlink_resume_failure",
                "hashes": sorted(unmodified_active),
                "error": str(exc),
            })
    if repaired_hashes:
        try:
            client.recheck(repaired_hashes)
            if wait_for_recheck(client, repaired_hashes, config.recheck_timeout_seconds):
                client.start(previously_active & repaired_hashes)
            else:
                print("Délai de recheck dépassé : les torrents réparés restent arrêtés.")
                record_stopped_torrents(result, repaired_hashes)
                append_jsonl(actions_path, {
                    "event": "hardlink_recheck_timeout",
                    "hashes": sorted(repaired_hashes),
                })
        except QbitError as exc:
            print(f"Erreur qBittorrent après réparation : {exc}")
            print("Les torrents réparés restent arrêtés.")
            record_stopped_torrents(result, repaired_hashes)
            append_jsonl(actions_path, {
                "event": "hardlink_qbittorrent_failure",
                "hashes": sorted(repaired_hashes),
                "error": str(exc),
            })
    result.observed_final_gain = current_free(config.completed_path) - session_free_before
    return result


def execute_deletions(config: Config, completed: TreeScan, media: TreeScan,
                      selected: Sequence[DeletionCandidate], client: QbitClient,
                      qbit_index: QbitIndex, actions_path: Path,
                      summary_path: Path, session_free_before: int | None = None,
                      initial_result: DeletionResult | None = None,
                      automatic_torrent_cleanup: bool | None = None,
                      ) -> DeletionResult:
    result = initial_result or DeletionResult()
    media_ids = {entry.identity for entry in media.files}
    affected = affected_hashes_for_candidates(selected, qbit_index)
    previously_active = {
        torrent_hash for torrent_hash in affected
        if torrent_hash in qbit_index.torrents
        and torrent_was_active(qbit_index.torrents[torrent_hash].state)
    }
    print(f"Mise à l'arrêt de {len(affected)} torrent(s) associé(s)...")
    client.stop(affected)

    validation_errors = []
    for candidate in selected:
        valid, reason = revalidate_candidate(candidate, config.video_extensions)
        if not valid:
            validation_errors.append(f"{candidate.path}: {reason}")
    if validation_errors:
        if previously_active:
            client.start(previously_active)
        for error in validation_errors:
            print(f"Validation refusée — {error}")
        append_jsonl(actions_path, {
            "event": "session_aborted",
            "reason": "revalidation_failed",
            "errors": validation_errors,
        })
        free_now = current_free(config.completed_path)
        baseline = session_free_before if session_free_before is not None else free_now
        result.observed_final_gain = free_now - baseline
        save_session_summary(
            result, config.report_path, actions_path, summary_path,
            baseline, free_now, "suppression annulée après revalidation",
        )
        return result

    free_before = session_free_before if session_free_before is not None else current_free(config.completed_path)
    append_jsonl(actions_path, {
        "event": "deletion_batch_intent",
        "candidate_ids": [item.candidate_id for item in selected],
        "affected_torrents": sorted(affected),
        "free_before": free_before,
    })

    removed_records: list[EntryRecord] = []
    removed_paths: set[str] = set()
    try:
        for candidate in selected:
            records, files, directories, success = delete_candidate(
                candidate, config.completed_path, actions_path
            )
            removed_records.extend(records)
            removed_paths.update(lexical_key(record.path) for record in records)
            result.files_deleted += files
            result.directories_deleted += directories
            if not success:
                result.failed_candidates += 1
    except KeyboardInterrupt:
        modified_hashes: set[str] = set()
        for path in removed_paths:
            modified_hashes.update(qbit_index.path_to_hashes.get(path, ()))
        to_resume = previously_active - modified_hashes
        if to_resume:
            try:
                client.start(to_resume)
            except QbitError as exc:
                append_jsonl(actions_path, {
                    "event": "deletion_interrupt_resume_failure",
                    "hashes": sorted(to_resume),
                    "error": str(exc),
                })
        append_jsonl(actions_path, {
            "event": "deletion_batch_interrupted",
            "modified_hashes_left_stopped": sorted(modified_hashes),
            "unmodified_hashes_resumed": sorted(to_resume),
        })
        raise

    result.logical_deleted += sum(record.size for record in removed_records)
    result.attributable_gain += estimate_reclaimable(removed_records, media_ids)
    free_immediate = current_free(config.completed_path)
    result.observed_immediate_gain = free_immediate - free_before

    actually_affected: set[str] = set()
    for path in removed_paths:
        actually_affected.update(qbit_index.path_to_hashes.get(path, ()))
    unmodified_active = previously_active - actually_affected
    if unmodified_active:
        try:
            client.start(unmodified_active)
        except QbitError as exc:
            append_jsonl(actions_path, {
                "event": "deletion_unmodified_resume_failure",
                "hashes": sorted(unmodified_active),
                "error": str(exc),
            })
    if actually_affected:
        try:
            print(f"Lancement du recheck de {len(actually_affected)} torrent(s)...")
            client.recheck(actually_affected)
            if not wait_for_recheck(client, actually_affected, config.recheck_timeout_seconds):
                print("Délai de recheck dépassé : les torrents restent arrêtés et ne seront pas retirés.")
                record_stopped_torrents(result, actually_affected)
                append_jsonl(actions_path, {
                    "event": "recheck_timeout",
                    "hashes": sorted(actually_affected),
                })
            else:
                unused = find_unused_torrents(
                    qbit_index, only_hashes=actually_affected
                )
                append_jsonl(actions_path, {
                    "event": "unused_torrents_found",
                    "hashes": [torrent.torrent_hash for torrent in unused],
                    "automatic_cleanup": automatic_torrent_cleanup,
                })
                if automatic_torrent_cleanup is True:
                    to_remove = {torrent.torrent_hash for torrent in unused}
                elif automatic_torrent_cleanup is False:
                    to_remove = set()
                else:
                    to_remove = select_unused_torrents(unused)
                if to_remove:
                    client.delete(to_remove)
                    result.torrents_removed += len(to_remove)
                    clear_stopped_torrents(result, to_remove)
                    append_jsonl(actions_path, {
                        "event": "torrents_removed",
                        "hashes": sorted(to_remove),
                        "deleteFiles": False,
                    })
                still_stopped = actually_affected - to_remove
                if still_stopped:
                    record_stopped_torrents(result, still_stopped)
                    append_jsonl(actions_path, {
                        "event": "torrents_left_stopped",
                        "hashes": sorted(still_stopped),
                        "reason": "fichiers_partiels_ou_retrait_desactive",
                    })
        except QbitError as exc:
            print(f"Erreur qBittorrent après suppression locale : {exc}")
            print("Les torrents concernés restent arrêtés ; le bilan local va être enregistré.")
            record_stopped_torrents(result, actually_affected)
            append_jsonl(actions_path, {
                "event": "qbittorrent_post_delete_failure",
                "error": str(exc),
                "hashes": sorted(actually_affected),
            })

    free_final = current_free(config.completed_path)
    result.observed_final_gain = free_final - free_before
    summary = save_session_summary(
        result, config.report_path, actions_path, summary_path,
        free_before, free_final,
    )
    print("\n" + summary)
    return result


def validate_configuration(config: Config) -> Config:
    if config.run_mode not in {"interactive", "dry-run", "auto"}:
        raise RuntimeError("RUN_MODE doit valoir interactive, dry-run ou auto")
    if config.auto_min_age_hours < 0:
        raise RuntimeError("AUTO_MIN_AGE_HOURS doit être positif ou nul")
    if not 0.0 <= config.auto_delete_similarity_threshold <= 1.0:
        raise RuntimeError("AUTO_DELETE_SIMILARITY_THRESHOLD doit être compris entre 0 et 1")
    if config.auto_duration_tolerance_seconds < 0:
        raise RuntimeError("AUTO_DURATION_TOLERANCE_SECONDS doit être positif ou nul")
    if config.auto_duration_tolerance_ratio < 0:
        raise RuntimeError("AUTO_DURATION_TOLERANCE_RATIO doit être positif ou nul")
    if config.ffprobe_workers <= 0:
        raise RuntimeError("FFPROBE_WORKERS doit être strictement positif")
    if config.auto_require_duration_match and not config.ffprobe_command:
        raise RuntimeError(
            "FFPROBE_COMMAND est requis quand AUTO_REQUIRE_DURATION_MATCH est actif"
        )
    if config.run_mode != "interactive" and not config.qbittorrent_password:
        raise RuntimeError(
            "QBITTORRENT_PASSWORD ne peut pas être vide en mode dry-run ou auto"
        )
    completed = config.completed_path.resolve(strict=True)
    media = config.media_path.resolve(strict=True)
    if not completed.is_dir() or not media.is_dir():
        raise RuntimeError("completed et medias doivent être des dossiers")
    report = config.report_path.resolve(strict=False)
    if path_is_within(report, completed) or path_is_within(report, media):
        raise RuntimeError("Le dossier de rapports doit être hors de completed et medias")
    if completed == media or path_is_within(completed, media) or path_is_within(media, completed):
        raise RuntimeError("completed et medias ne doivent pas se contenir mutuellement")
    host_mapping = config.qbittorrent_host_path.resolve(strict=True)
    if not path_is_within(completed, host_mapping):
        raise RuntimeError("completed doit se trouver sous QBITTORRENT_HOST_PATH")
    if config.quick_sample_size <= 0 or config.quick_sample_count <= 0:
        raise RuntimeError("Les paramètres d'échantillonnage doivent être strictement positifs")
    if config.full_compare_chunk_size < 64 * 1024:
        raise RuntimeError("FULL_COMPARE_CHUNK_SIZE doit être au moins égal à 64 KiB")
    ffprobe_host_mapping = config.ffprobe_host_path
    if config.auto_require_duration_match:
        if ffprobe_host_mapping is None or not config.ffprobe_container_path:
            raise RuntimeError(
                "FFPROBE_HOST_PATH et FFPROBE_CONTAINER_PATH sont requis"
            )
        ffprobe_host_mapping = ffprobe_host_mapping.resolve(strict=True)
        if not path_is_within(completed, ffprobe_host_mapping) or not path_is_within(
                media, ffprobe_host_mapping):
            raise RuntimeError(
                "completed et medias doivent être sous FFPROBE_HOST_PATH"
            )
    return dataclasses.replace(
        config,
        completed_path=completed,
        media_path=media,
        report_path=report,
        qbittorrent_host_path=host_mapping,
        ffprobe_host_path=ffprobe_host_mapping,
        video_extensions=frozenset(
            extension.casefold() if extension.startswith(".") else "." + extension.casefold()
            for extension in config.video_extensions
        ),
    )


def main(config: Config) -> int:
    try:
        config = validate_configuration(config)
        config.report_path.mkdir(parents=True, exist_ok=True)
    except (OSError, RuntimeError) as exc:
        print(f"Configuration invalide : {exc}", file=sys.stderr)
        return 2

    print(f"Mode d'exécution : {config.run_mode}")
    if config.run_mode == "dry-run":
        print("PRÉVISUALISATION : aucune modification locale ou qBittorrent ne sera effectuée.")
    print(f"Analyse de {config.completed_path}...")
    completed = scan_tree(config.completed_path, config.video_extensions)
    print(f"Analyse de {config.media_path}...")
    media = scan_tree(config.media_path, config.video_extensions)
    statistics = compute_scan_statistics(completed, media)
    free_at_scan = current_free(config.completed_path)
    print_statistics(statistics, free_at_scan)
    if completed.root.stat().st_dev != media.root.stat().st_dev:
        print("ATTENTION : les deux arbres sont sur des systèmes de fichiers différents ; "
              "ils ne peuvent pas partager de hardlinks.")
    if completed.errors or media.errors:
        print(f"ATTENTION : {len(completed.errors) + len(media.errors)} erreur(s) de scan. "
              "Les zones concernées seront protégées.")

    print("Recherche des vidéos similaires...")
    candidates = build_deletion_candidates(
        completed, media, config.similarity_threshold, config.video_extensions
    )
    repairs: list[HardlinkRepair] = []
    if not media.errors and completed.root.stat().st_dev == media.root.stat().st_dev:
        print(
            "Recherche des doublons potentiels : nom + taille, sans lecture du contenu..."
        )
        if config.search_similar_names_for_hardlink:
            print(
                "Recherche complémentaire activée pour les noms différents : "
                f"{config.quick_sample_count} échantillons de "
                f"{human_size(config.quick_sample_size)}."
            )
        repairs = discover_hardlink_repairs(
            completed, media, config.similarity_threshold,
            config.quick_sample_size, config.quick_sample_count,
            config.search_similar_names_for_hardlink,
        )
        print(f"Réparations de hardlinks proposées : {len(repairs)}")
    repair_source_keys = {
        lexical_key(repair.completed.path) for repair in repairs
    }
    initial_auto_decisions: list[AutoCandidateDecision] = []
    initial_duration_checks: list[dict[str, Any]] = []
    if config.run_mode != "interactive":
        initial_auto_decisions = assess_auto_candidates(
            candidates,
            config.auto_delete_similarity_threshold,
            config.auto_min_age_hours,
            time.time_ns(),
            repair_source_keys,
            "reparation_hardlink_planifiee",
        )
        if config.auto_require_duration_match:
            eligible_before_duration = sum(
                decision.eligible for decision in initial_auto_decisions
            )
            print(
                "Vérification ffprobe des durées pour "
                f"{eligible_before_duration} candidat(s)..."
            )
            initial_auto_decisions, initial_duration_checks = (
                verify_auto_decision_durations(initial_auto_decisions, config)
            )
    stamp = utc_stamp()
    try:
        unlinked_completed_path, unlinked_media_path = write_unlinked_inventory_reports(
            config.report_path, stamp, completed, media, config.similarity_threshold
        )
        scan_path, csv_path, actions_path, summary_path = write_initial_reports(
            config.report_path, stamp, config, statistics, completed, media,
            candidates, repairs, initial_auto_decisions,
        )
    except OSError as exc:
        print(f"Impossible d'écrire les rapports ; aucune suppression autorisée : {exc}",
              file=sys.stderr)
        return 3
    print(f"Rapport du scan : {scan_path}")
    print(f"Liste CSV       : {csv_path}")
    print(f"Sans hardlink completed : {unlinked_completed_path}")
    print(f"Sans hardlink medias    : {unlinked_media_path}")
    if config.auto_require_duration_match:
        duration_path = write_duration_report(
            config.report_path, stamp, "initial", initial_duration_checks
        )
        print(f"Vérification des durées : {duration_path}")

    if not candidates and not repairs and config.run_mode == "interactive":
        result = DeletionResult()
        summary = save_session_summary(
            result, config.report_path, actions_path, summary_path,
            free_at_scan, current_free(config.completed_path), "aucun candidat",
        )
        print(summary)
        return 0

    if config.run_mode != "interactive" and (completed.errors or media.errors):
        reason = (
            "mode automatique bloqué : scan incomplet "
            f"({len(completed.errors) + len(media.errors)} erreur(s))"
        )
        append_jsonl(actions_path, {
            "event": "automation_blocked",
            "reason": "scan_incomplete",
            "completed_errors": len(completed.errors),
            "media_errors": len(media.errors),
        })
        summary = save_session_summary(
            DeletionResult(), config.report_path, actions_path, summary_path,
            free_at_scan, current_free(config.completed_path), reason,
        )
        print(summary)
        return 7

    if media.errors:
        result = DeletionResult()
        reason = (
            "suppression bloquée : le scan de medias est incomplet "
            f"({len(media.errors)} erreur(s))"
        )
        summary = save_session_summary(
            result, config.report_path, actions_path, summary_path,
            free_at_scan, current_free(config.completed_path), reason,
        )
        print(summary)
        return 7

    password = config.qbittorrent_password
    if not password:
        try:
            password = getpass.getpass("Mot de passe qBittorrent : ")
        except (EOFError, KeyboardInterrupt):
            print("\nAuthentification annulée ; les rapports restent disponibles.")
            save_session_summary(
                DeletionResult(), config.report_path, actions_path, summary_path,
                free_at_scan, current_free(config.completed_path),
                "authentification qBittorrent annulée",
            )
            return 4
    client = QbitClient(
        config.qbittorrent_url,
        config.qbittorrent_username,
        password,
    )
    try:
        client.login()
        print(f"Connexion qBittorrent réussie ({client.version}). Indexation des torrents...")
        qbit_index = build_qbit_index(client, config, completed)
    except QbitError as exc:
        append_jsonl(actions_path, {"event": "qbittorrent_unavailable", "error": str(exc)})
        print(f"API qBittorrent indisponible : {exc}", file=sys.stderr)
        print("Suppression bloquée. Les rapports du scan restent disponibles.")
        save_session_summary(
            DeletionResult(), config.report_path, actions_path, summary_path,
            free_at_scan, current_free(config.completed_path),
            "suppression bloquée : API qBittorrent indisponible",
        )
        return 5
    if qbit_index.mapping_errors:
        for error in qbit_index.mapping_errors:
            print(f"Erreur de mapping qBittorrent : {error}", file=sys.stderr)
        append_jsonl(actions_path, {
            "event": "qbittorrent_mapping_errors",
            "errors": qbit_index.mapping_errors,
        })
        save_session_summary(
            DeletionResult(), config.report_path, actions_path, summary_path,
            free_at_scan, current_free(config.completed_path),
            "suppression bloquée : mapping qBittorrent incomplet",
        )
        return 8

    session_free_before = current_free(config.completed_path)

    if config.run_mode == "dry-run":
        decisions_by_id = {
            decision.candidate_id: decision for decision in initial_auto_decisions
        }
        planned_deletions = [
            candidate for candidate in candidates
            if decisions_by_id[candidate.candidate_id].eligible
        ]
        predicted_unused = find_unused_torrents(
            qbit_index,
            file_paths_for_candidates(planned_deletions),
            affected_hashes_for_candidates(planned_deletions, qbit_index),
        )
        plan_path = write_auto_plan(
            config.report_path, stamp, "initial", config, repairs,
            initial_auto_decisions, predicted_unused, repair_source_keys,
        )
        append_jsonl(actions_path, {
            "event": "auto_plan_written",
            "path": str(plan_path),
            "phase": "initial",
            "dry_run": True,
            "planned_repairs": len(repairs),
            "planned_deletions": len(planned_deletions),
            "predicted_unused_torrents": len(predicted_unused),
            "repair_decisions": [
                {
                    "path": str(repair.completed.path),
                    "auto_eligible": True,
                    "auto_action": "repair",
                    "auto_reason": ["comparaison_integrale_requise_avant_action"],
                }
                for repair in repairs
            ],
            "deletion_decisions": [
                auto_decision_to_dict(decision)
                for decision in initial_auto_decisions
            ],
        })
        print(f"Plan automatique : {plan_path}")
        print(
            f"Dry-run terminé : {len(repairs)} réparation(s), "
            f"{len(planned_deletions)} suppression(s) et "
            f"{len(predicted_unused)} retrait(s) de torrent prévus."
        )
        summary = save_session_summary(
            DeletionResult(), config.report_path, actions_path, summary_path,
            session_free_before, current_free(config.completed_path),
            "dry-run terminé sans aucune mutation",
        )
        print(summary)
        return 0

    accumulated = DeletionResult()
    if config.run_mode == "auto":
        initial_by_id = {
            decision.candidate_id: decision for decision in initial_auto_decisions
        }
        initially_planned = [
            candidate for candidate in candidates
            if initial_by_id[candidate.candidate_id].eligible
        ]
        initial_unused = find_unused_torrents(
            qbit_index,
            file_paths_for_candidates(initially_planned),
            affected_hashes_for_candidates(initially_planned, qbit_index),
        )
        initial_plan_path = write_auto_plan(
            config.report_path, stamp, "initial", config, repairs,
            initial_auto_decisions, initial_unused, repair_source_keys,
        )
        append_jsonl(actions_path, {
            "event": "auto_plan_written",
            "path": str(initial_plan_path),
            "phase": "initial",
            "dry_run": False,
            "planned_repairs": len(repairs),
            "preliminary_deletions": len(initially_planned),
            "repair_decisions": [
                {
                    "path": str(repair.completed.path),
                    "auto_eligible": True,
                    "auto_action": "repair",
                    "auto_reason": ["comparaison_integrale_requise_avant_action"],
                }
                for repair in repairs
            ],
            "deletion_decisions": [
                auto_decision_to_dict(decision)
                for decision in initial_auto_decisions
            ],
        })
        print(f"Plan automatique initial : {initial_plan_path}")
        try:
            if repairs:
                accumulated = execute_hardlink_repairs(
                    config, repairs, client, qbit_index,
                    actions_path, session_free_before,
                )

            print("Nouvelle analyse après la phase de réparation...")
            completed = scan_tree(config.completed_path, config.video_extensions)
            media = scan_tree(config.media_path, config.video_extensions)
            statistics = compute_scan_statistics(completed, media)
            print_statistics(statistics, current_free(config.completed_path))
            if completed.errors or media.errors:
                append_jsonl(actions_path, {
                    "event": "automation_blocked",
                    "reason": "post_repair_scan_incomplete",
                    "completed_errors": len(completed.errors),
                    "media_errors": len(media.errors),
                })
                summary = save_session_summary(
                    accumulated, config.report_path, actions_path, summary_path,
                    session_free_before, current_free(config.completed_path),
                    "suppression automatique bloquée : scan après réparation incomplet",
                )
                print(summary)
                return 7

            candidates = build_deletion_candidates(
                completed, media, config.similarity_threshold, config.video_extensions
            )
            post_repair_path = write_post_repair_report(
                config.report_path, stamp, statistics, completed, media, candidates
            )
            print(f"Rapport après réparation : {post_repair_path}")

            media_identities = {entry.identity for entry in media.files}
            failed_repair_paths: set[str] = set()
            for repair in repairs:
                try:
                    current = os.stat(repair.completed.path, follow_symlinks=False)
                    identity = (current.st_dev, current.st_ino)
                except OSError:
                    failed_repair_paths.add(lexical_key(repair.completed.path))
                    continue
                if identity not in media_identities:
                    failed_repair_paths.add(lexical_key(repair.completed.path))

            final_decisions = assess_auto_candidates(
                candidates,
                config.auto_delete_similarity_threshold,
                config.auto_min_age_hours,
                time.time_ns(),
                failed_repair_paths,
                "reparation_hardlink_en_echec",
            )
            final_duration_checks: list[dict[str, Any]] = []
            if config.auto_require_duration_match:
                eligible_before_duration = sum(
                    decision.eligible for decision in final_decisions
                )
                print(
                    "Vérification ffprobe après réparation pour "
                    f"{eligible_before_duration} candidat(s)..."
                )
                final_decisions, final_duration_checks = (
                    verify_auto_decision_durations(final_decisions, config)
                )
                final_duration_path = write_duration_report(
                    config.report_path, stamp, "post-repair", final_duration_checks
                )
                print(f"Vérification des durées : {final_duration_path}")
            final_by_id = {
                decision.candidate_id: decision for decision in final_decisions
            }
            selected = [
                candidate for candidate in candidates
                if final_by_id[candidate.candidate_id].eligible
            ]
            predicted_unused = find_unused_torrents(
                qbit_index,
                file_paths_for_candidates(selected),
                affected_hashes_for_candidates(selected, qbit_index),
            )
            final_plan_path = write_auto_plan(
                config.report_path, stamp, "post-repair", config, (),
                final_decisions, predicted_unused, failed_repair_paths,
            )
            append_jsonl(actions_path, {
                "event": "auto_plan_written",
                "path": str(final_plan_path),
                "phase": "post-repair",
                "dry_run": False,
                "planned_deletions": len(selected),
                "predicted_unused_torrents": len(predicted_unused),
                "failed_repair_paths": sorted(failed_repair_paths),
                "deletion_decisions": [
                    auto_decision_to_dict(decision)
                    for decision in final_decisions
                ],
            })
            print(f"Plan automatique avant suppression : {final_plan_path}")
            print(
                f"Sélection prudente : {len(selected)} candidat(s) supprimable(s), "
                f"{len(candidates) - len(selected)} conservé(s)."
            )
            if not selected:
                accumulated.observed_final_gain = (
                    current_free(config.completed_path) - session_free_before
                )
                summary = save_session_summary(
                    accumulated, config.report_path, actions_path, summary_path,
                    session_free_before, current_free(config.completed_path),
                    "mode automatique terminé : aucune suppression admissible",
                )
                print(summary)
                return 0
            execute_deletions(
                config, completed, media, selected, client, qbit_index,
                actions_path, summary_path, session_free_before, accumulated,
                automatic_torrent_cleanup=config.auto_remove_empty_torrents,
            )
            return 0
        except KeyboardInterrupt:
            print("\nMode automatique interrompu. Les torrents non modifiés ont été repris.")
            accumulated.observed_final_gain = (
                current_free(config.completed_path) - session_free_before
            )
            save_session_summary(
                accumulated, config.report_path, actions_path, summary_path,
                session_free_before, current_free(config.completed_path),
                "mode automatique interrompu",
            )
            return 130
        except QbitError as exc:
            append_jsonl(actions_path, {"event": "qbittorrent_failure", "error": str(exc)})
            print(f"Action automatique bloquée par qBittorrent : {exc}", file=sys.stderr)
            accumulated.observed_final_gain = (
                current_free(config.completed_path) - session_free_before
            )
            save_session_summary(
                accumulated, config.report_path, actions_path, summary_path,
                session_free_before, current_free(config.completed_path),
                "mode automatique bloqué : erreur qBittorrent",
            )
            return 6

    try:
        selected_repairs = interactive_repair_selection(repairs)
        if selected_repairs:
            try:
                accumulated = execute_hardlink_repairs(
                    config, selected_repairs, client, qbit_index,
                    actions_path, session_free_before,
                )
            except QbitError as exc:
                append_jsonl(actions_path, {
                    "event": "hardlink_batch_blocked",
                    "error": str(exc),
                })
                save_session_summary(
                    accumulated, config.report_path, actions_path, summary_path,
                    session_free_before, current_free(config.completed_path),
                    "réparation bloquée avant modification : erreur qBittorrent",
                )
                print(f"Réparation bloquée par qBittorrent : {exc}", file=sys.stderr)
                return 6
            print("Nouvelle analyse après réparation des hardlinks...")
            completed = scan_tree(config.completed_path, config.video_extensions)
            media = scan_tree(config.media_path, config.video_extensions)
            statistics = compute_scan_statistics(completed, media)
            print_statistics(statistics, current_free(config.completed_path))
            if media.errors:
                accumulated.observed_final_gain = (
                    current_free(config.completed_path) - session_free_before
                )
                summary = save_session_summary(
                    accumulated, config.report_path, actions_path, summary_path,
                    session_free_before, current_free(config.completed_path),
                    "suppression bloquée après réparation : scan de medias incomplet",
                )
                print(summary)
                return 7
            candidates = build_deletion_candidates(
                completed, media, config.similarity_threshold, config.video_extensions
            )
            post_repair_path = write_post_repair_report(
                config.report_path, stamp, statistics, completed, media, candidates
            )
            print(f"Rapport après réparation : {post_repair_path}")
        selected = interactive_selection(candidates)
    except (EOFError, KeyboardInterrupt):
        print("\nSélection annulée.")
        accumulated.observed_final_gain = current_free(config.completed_path) - session_free_before
        save_session_summary(
            accumulated, config.report_path, actions_path, summary_path,
            session_free_before, current_free(config.completed_path),
            "sélection annulée",
        )
        return 0
    if not selected:
        accumulated.observed_final_gain = current_free(config.completed_path) - session_free_before
        summary = save_session_summary(
            accumulated, config.report_path, actions_path, summary_path,
            session_free_before, current_free(config.completed_path),
            "aucune suppression sélectionnée",
        )
        print(summary)
        return 0
    try:
        execute_deletions(
            config, completed, media, selected, client, qbit_index,
            actions_path, summary_path, session_free_before, accumulated,
        )
    except QbitError as exc:
        append_jsonl(actions_path, {"event": "qbittorrent_failure", "error": str(exc)})
        print(f"Action bloquée par une erreur qBittorrent : {exc}", file=sys.stderr)
        accumulated.observed_final_gain = current_free(config.completed_path) - session_free_before
        save_session_summary(
            accumulated, config.report_path, actions_path, summary_path,
            session_free_before, current_free(config.completed_path),
            "suppression bloquée avant modification : erreur qBittorrent",
        )
        return 6
    return 0


if __name__ == "__main__":
    COMPLETED_PATH = "/volume3/chill/completed"
    MEDIA_PATH = "/volume3/chill/medias"
    REPORT_PATH = "/volume3/chill/doublon-checker-reports"

    QBITTORRENT_URL = "http://127.0.0.1:8080"
    QBITTORRENT_USERNAME = "admin"
    QBITTORRENT_PASSWORD = ""
    QBITTORRENT_CONTAINER_PATH = "/data"
    QBITTORRENT_HOST_PATH = "/volume3/chill"

    VIDEO_EXTENSIONS = {".mkv"}
    SIMILARITY_THRESHOLD = 0.60
    RECHECK_TIMEOUT_SECONDS = 600
    QUICK_SAMPLE_SIZE = 1024 * 1024
    QUICK_SAMPLE_COUNT = 5
    FULL_COMPARE_CHUNK_SIZE = 16 * 1024 * 1024
    SEARCH_SIMILAR_NAMES_FOR_HARDLINK = False

    # "interactive" (par défaut), "dry-run" (prévisualisation) ou "auto".
    RUN_MODE = "interactive"
    AUTO_MIN_AGE_HOURS = 48
    AUTO_DELETE_SIMILARITY_THRESHOLD = 0.90
    AUTO_REMOVE_EMPTY_TORRENTS = True
    AUTO_REQUIRE_DURATION_MATCH = False
    AUTO_DURATION_TOLERANCE_SECONDS = 5.0
    AUTO_DURATION_TOLERANCE_RATIO = 0.002
    FFPROBE_WORKERS = 4
    FFPROBE_COMMAND = ()
    FFPROBE_HOST_PATH = "/volume3/chill"
    FFPROBE_CONTAINER_PATH = "/data"

    configuration = Config(
        completed_path=Path(COMPLETED_PATH),
        media_path=Path(MEDIA_PATH),
        report_path=Path(REPORT_PATH),
        qbittorrent_url=QBITTORRENT_URL,
        qbittorrent_username=QBITTORRENT_USERNAME,
        qbittorrent_password=QBITTORRENT_PASSWORD,
        qbittorrent_container_path=QBITTORRENT_CONTAINER_PATH,
        qbittorrent_host_path=Path(QBITTORRENT_HOST_PATH),
        video_extensions=frozenset(VIDEO_EXTENSIONS),
        similarity_threshold=SIMILARITY_THRESHOLD,
        recheck_timeout_seconds=RECHECK_TIMEOUT_SECONDS,
        quick_sample_size=QUICK_SAMPLE_SIZE,
        quick_sample_count=QUICK_SAMPLE_COUNT,
        full_compare_chunk_size=FULL_COMPARE_CHUNK_SIZE,
        search_similar_names_for_hardlink=SEARCH_SIMILAR_NAMES_FOR_HARDLINK,
        run_mode=RUN_MODE,
        auto_min_age_hours=AUTO_MIN_AGE_HOURS,
        auto_delete_similarity_threshold=AUTO_DELETE_SIMILARITY_THRESHOLD,
        auto_remove_empty_torrents=AUTO_REMOVE_EMPTY_TORRENTS,
        ffprobe_command=tuple(FFPROBE_COMMAND),
        ffprobe_host_path=Path(FFPROBE_HOST_PATH),
        ffprobe_container_path=FFPROBE_CONTAINER_PATH,
        auto_require_duration_match=AUTO_REQUIRE_DURATION_MATCH,
        auto_duration_tolerance_seconds=AUTO_DURATION_TOLERANCE_SECONDS,
        auto_duration_tolerance_ratio=AUTO_DURATION_TOLERANCE_RATIO,
        ffprobe_workers=FFPROBE_WORKERS,
    )
    raise SystemExit(main(configuration))
