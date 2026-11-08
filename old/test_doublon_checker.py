import json
import os
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import doublon_checker as dc


EXTENSIONS = frozenset({".mkv"})


class RecordingQbitClient:
    def __init__(self):
        self.stopped = []
        self.started = []
        self.rechecked = []
        self.deleted = []

    def stop(self, hashes):
        self.stopped.append(set(hashes))

    def start(self, hashes):
        self.started.append(set(hashes))

    def recheck(self, hashes):
        self.rechecked.append(set(hashes))

    def delete(self, hashes):
        self.deleted.append(set(hashes))


class FilesystemTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.completed = self.root / "completed"
        self.media = self.root / "medias"
        self.reports = self.root / "reports"
        self.completed.mkdir()
        self.media.mkdir()
        self.reports.mkdir()

    def tearDown(self):
        self.temporary.cleanup()

    def write_file(self, path, content=b"video"):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path

    def scans_real_hardlinks_and_deduplicates_allocated_size(self):
        source = self.write_file(self.completed / "Film.2024.1080p.mkv", b"x" * 8192)
        destination = self.media / "films" / source.name
        destination.parent.mkdir()
        os.link(source, destination)

        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        statistics = dc.compute_scan_statistics(completed_scan, media_scan)

        self.assertEqual(statistics["shared_inodes"], 1)
        self.assertEqual(statistics["orphan_videos_reclaimable_bytes"], 0)
        self.assertEqual(dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        ), [])

    def test_scans_real_hardlinks_and_deduplicates_allocated_size(self):
        self.scans_real_hardlinks_and_deduplicates_allocated_size()

    def test_nlink_outside_media_does_not_count_as_imported(self):
        video = self.write_file(self.completed / "Film.2023.mkv", b"1234")
        external = self.root / "elsewhere.mkv"
        os.link(video, external)
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)

        candidates = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )

        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].estimated_reclaimable, 0)
        self.assertEqual(candidates[0].videos[0].nlink, 2)

    def test_gain_is_counted_only_when_every_hardlink_is_selected(self):
        first = self.write_file(self.completed / "one.mkv", b"x" * 8192)
        second = self.completed / "two.mkv"
        os.link(first, second)
        scan = dc.scan_tree(self.completed, EXTENSIONS)
        records = {item.path.name: item for item in scan.files}

        self.assertEqual(dc.estimate_reclaimable([records["one.mkv"]], set()), 0)
        self.assertGreater(
            dc.estimate_reclaimable([records["one.mkv"], records["two.mkv"]], set()),
            0,
        )

    def test_similarity_uses_title_year_episode_and_size(self):
        orphan = self.write_file(
            self.completed / "Example.Show.S02E03.1080p.MULTI.x264.mkv", b"a" * 9000
        )
        likely = self.write_file(
            self.media / "series" / "Example.Show.S02E03.2160p.FRENCH.x265.mkv",
            b"b" * 10000,
        )
        self.write_file(
            self.media / "series" / "Other.Show.S02E04.1080p.mkv", b"c" * 9000
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        video = next(item for item in completed_scan.videos if item.path == orphan.resolve())

        matches = dc.SimilarityEngine(media_scan.videos).find(video, 0.60)

        self.assertTrue(matches)
        self.assertEqual(matches[0].path, likely.resolve())

    def test_scene_title_stops_before_identity_marker_and_ignores_release_group(self):
        supply = dc.parse_scene_name(Path(
            "When.They.See.Us.S01E01.MULTi.1080p.WEB.x264-AXWELL.mkv"
        ))
        other = dc.parse_scene_name(Path(
            "When.They.See.Us.S01E01.MULTi.1080p.WEB.x264-ARK01.mkv"
        ))

        self.assertEqual(supply.title_tokens, ("when", "they", "see", "us"))
        self.assertEqual(supply.title_tokens, other.title_tokens)
        self.assertEqual(supply.episode, (1, 1))

    def test_auto_candidate_refuses_different_editions(self):
        completed_file = self.write_file(
            self.completed / "The.Movie.2024.EXTENDED.MULTi.1080p.mkv", b"source"
        )
        media_file = self.write_file(
            self.media / "The.Movie.2024.MULTi.1080p.mkv", b"replacement"
        )
        old_ns = time.time_ns() - 72 * 3600 * 1_000_000_000
        os.utime(completed_file, ns=(old_ns, old_ns))
        completed_video = dc.scan_tree(self.completed, EXTENSIONS).videos[0]
        media_video = dc.scan_tree(self.media, EXTENSIONS).videos[0]
        candidate = dc.DeletionCandidate(
            candidate_id=1, kind="file", path=completed_video.path,
            videos=[completed_video], entries=[completed_video],
            snapshot={".": completed_video.signature},
            similarities={str(completed_video.path): [dc.Similarity(
                media_video.path, 0.99, media_video.size,
                dc.parse_scene_name(media_video.path),
            )]},
        )

        decision = dc.assess_auto_candidate(candidate, 0.90, 48, time.time_ns())

        self.assertFalse(decision.eligible)
        self.assertTrue(any(
            "edition_differente_ou_ambigue" in reason
            for reason in decision.reasons
        ))

    def test_auto_candidate_requires_exact_normalized_title(self):
        completed_file = self.write_file(
            self.completed / "Movie.Name.2024.MULTi.1080p.mkv", b"source"
        )
        media_file = self.write_file(
            self.media / "Movie.Name.Returns.2024.MULTi.1080p.mkv", b"replacement"
        )
        old_ns = time.time_ns() - 72 * 3600 * 1_000_000_000
        os.utime(completed_file, ns=(old_ns, old_ns))
        completed_video = dc.scan_tree(self.completed, EXTENSIONS).videos[0]
        media_video = dc.scan_tree(self.media, EXTENSIONS).videos[0]
        candidate = dc.DeletionCandidate(
            candidate_id=1, kind="file", path=completed_video.path,
            videos=[completed_video], entries=[completed_video],
            snapshot={".": completed_video.signature},
            similarities={str(completed_video.path): [dc.Similarity(
                media_video.path, 0.99, media_video.size,
                dc.parse_scene_name(media_video.path),
            )]},
        )

        decision = dc.assess_auto_candidate(candidate, 0.90, 48, time.time_ns())

        self.assertFalse(decision.eligible)
        self.assertTrue(any(
            "titre_different_ou_ambigu" in reason for reason in decision.reasons
        ))

    def test_writes_bidirectional_unlinked_inventories(self):
        completed_only = self.write_file(
            self.completed / "Movie.2024.MULTi.1080p.mkv", b"completed"
        )
        media_only = self.write_file(
            self.media / "Movie.2024.MULTi.1080p.mkv", b"media-copy"
        )
        linked_source = self.write_file(
            self.completed / "Linked.2023.MULTi.1080p.mkv", b"linked"
        )
        linked_media = self.media / linked_source.name
        os.link(linked_source, linked_media)
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)

        completed_csv, media_csv = dc.write_unlinked_inventory_reports(
            self.reports, "stamp", completed_scan, media_scan, 0.60
        )

        completed_text = completed_csv.read_text(encoding="utf-8")
        media_text = media_csv.read_text(encoding="utf-8")
        self.assertIn(str(completed_only.resolve()), completed_text)
        self.assertNotIn(str(linked_source.resolve()), completed_text)
        self.assertIn(str(media_only.resolve()), media_text)
        self.assertNotIn(str(linked_media.resolve()), media_text)

    def test_duration_verification_blocks_a_different_cut(self):
        completed_path = str(self.completed / "Movie.2024.mkv")
        media_path = str(self.media / "Movie.2024.mkv")
        decision = dc.AutoCandidateDecision(
            candidate_id=1,
            path=Path(completed_path),
            eligible=True,
            action="delete",
            reasons=("criteres_prudents_satisfaits",),
            replacements=((completed_path, media_path, 0.99),),
        )
        config = make_config(
            self.completed, self.media, self.reports, self.root,
            auto_require_duration_match=True,
            ffprobe_command=("ffprobe",),
            ffprobe_host_path=self.root,
            ffprobe_container_path="/data",
            auto_duration_tolerance_seconds=5.0,
            auto_duration_tolerance_ratio=0.002,
        )
        durations = {completed_path: 7200.0, media_path: 6900.0}

        with mock.patch.object(
                dc, "probe_media_duration",
                side_effect=lambda path, _config: durations[str(path)]):
            verified, checks = dc.verify_auto_decision_durations([decision], config)

        self.assertFalse(verified[0].eligible)
        self.assertIn("duree_differente", verified[0].reasons[0])
        self.assertFalse(checks[0]["matches"])

    def test_identical_copy_is_proposed_for_hardlink_repair(self):
        payload = b"same-video-content" * 1024
        completed_file = self.write_file(
            self.completed / "Movie.Name.2024.1080p.mkv", payload
        )
        media_file = self.write_file(
            self.media / "films" / "Movie.Name.2024.1080p.mkv", payload
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)

        with mock.patch.object(
                dc, "files_match_samples",
                side_effect=AssertionError("un nom exact ne doit lire aucun échantillon")):
            repairs = dc.discover_hardlink_repairs(completed_scan, media_scan, 0.60)

        self.assertEqual(len(repairs), 1)
        self.assertEqual(repairs[0].completed.path, completed_file.resolve())
        self.assertEqual(repairs[0].media.path, media_file.resolve())
        self.assertEqual(repairs[0].detection_method, "exact_name_and_size")

    def test_same_name_and_size_is_only_a_potential_duplicate(self):
        completed_file = self.write_file(self.completed / "Movie.Name.2024.mkv", b"AAAA")
        media_file = self.write_file(self.media / "films" / "Movie.Name.2024.mkv", b"BBBB")
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)

        repairs = dc.discover_hardlink_repairs(completed_scan, media_scan, 0.60)

        self.assertEqual(len(repairs), 1)
        self.assertEqual(repairs[0].detection_method, "exact_name_and_size")
        action_log = self.reports / "actions.jsonl"
        action_log.touch()
        self.assertFalse(dc.create_hardlink_repair(
            repairs[0], self.completed.resolve(), self.media.resolve(), action_log,
        ))
        self.assertNotEqual(os.stat(completed_file).st_ino, os.stat(media_file).st_ino)

    def test_quick_match_can_never_bypass_final_exact_comparison(self):
        completed_payload = bytearray(b"A" * 4096)
        media_payload = bytearray(completed_payload)
        media_payload[1000] = ord("B")  # Outside samples at 0, 2016 and 4032.
        completed_file = self.write_file(
            self.completed / "Movie.Name.2024.1080p.mkv", bytes(completed_payload)
        )
        media_file = self.write_file(
            self.media / "films" / "Movie.Name.2024.2160p.mkv", bytes(media_payload)
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        repairs = dc.discover_hardlink_repairs(
            completed_scan, media_scan, 0.60, sample_size=64, sample_count=3
        )
        self.assertEqual(len(repairs), 1)  # Only a fast proposal at this point.
        self.assertEqual(repairs[0].detection_method, "similar_name_and_samples")
        action_log = self.reports / "actions.jsonl"
        action_log.touch()

        success = dc.create_hardlink_repair(
            repairs[0], self.completed.resolve(), self.media.resolve(),
            action_log, full_compare_chunk_size=256,
        )

        self.assertFalse(success)
        self.assertNotEqual(os.stat(completed_file).st_ino, os.stat(media_file).st_ino)
        self.assertEqual(media_file.read_bytes(), bytes(media_payload))

    def test_hardlink_repair_links_completed_toward_media(self):
        payload = b"identical" * 4096
        completed_file = self.write_file(
            self.completed / "Movie.Name.2024.mkv", payload
        )
        media_file = self.write_file(
            self.media / "films" / "Movie.Name.2024.mkv", payload
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        repair = dc.discover_hardlink_repairs(completed_scan, media_scan, 0.60)[0]
        completed_before = os.stat(completed_file)
        media_before = os.stat(media_file)
        action_log = self.reports / "actions.jsonl"
        action_log.touch()

        success = dc.create_hardlink_repair(
            repair, self.completed.resolve(), self.media.resolve(), action_log
        )

        self.assertTrue(success)
        completed_after = os.stat(completed_file)
        media_after = os.stat(media_file)
        self.assertEqual(
            (completed_after.st_dev, completed_after.st_ino),
            (media_after.st_dev, media_after.st_ino),
        )
        self.assertEqual(completed_before.st_ino, completed_after.st_ino)
        self.assertEqual(completed_before.st_ino, media_after.st_ino)
        self.assertNotEqual(media_before.st_ino, media_after.st_ino)
        self.assertEqual(completed_file.read_bytes(), payload)
        events = [json.loads(line) for line in action_log.read_text().splitlines()]
        self.assertEqual(events[-1]["event"], "hardlink_success")

    def test_hardlink_permission_is_checked_before_full_comparison(self):
        payload = b"identical" * 4096
        self.write_file(self.completed / "Movie.Name.2024.mkv", payload)
        self.write_file(self.media / "films" / "Movie.Name.2024.mkv", payload)
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        repair = dc.discover_hardlink_repairs(completed_scan, media_scan, 0.60)[0]
        action_log = self.reports / "actions.jsonl"
        action_log.touch()

        permission_error = PermissionError(1, "Operation not permitted")
        with mock.patch.object(dc.os, "link", side_effect=permission_error), \
                mock.patch.object(
                    dc, "files_are_identical",
                    side_effect=AssertionError("le contenu ne doit pas encore être lu"),
                ):
            success = dc.create_hardlink_repair(
                repair, self.completed.resolve(), self.media.resolve(), action_log
            )

        self.assertFalse(success)
        events = [json.loads(line) for line in action_log.read_text().splitlines()]
        self.assertIn("fs.protected_hardlinks", events[-1]["error"])

    def test_smallest_orphan_folder_is_proposed_with_sidecars(self):
        season = self.completed / "A.Show" / "Season 01"
        first = self.write_file(season / "A.Show.S01E01.mkv", b"a")
        self.write_file(season / "A.Show.S01E01.srt", b"subtitle")
        self.write_file(
            self.completed / "A.Show" / "Season 02" / "A.Show.S02E01.mkv", b"b"
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)

        candidates = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )

        folders = [item for item in candidates if item.kind == "folder"]
        self.assertEqual({item.path for item in folders}, {
            (self.completed / "A.Show" / "Season 01").resolve(),
            (self.completed / "A.Show" / "Season 02").resolve(),
        })
        first_folder = next(item for item in folders if first.resolve() in [v.path for v in item.videos])
        self.assertTrue(any(item.path.suffix == ".srt" for item in first_folder.entries))

    def test_changed_folder_is_rejected(self):
        folder = self.completed / "Film"
        self.write_file(folder / "Film.2020.mkv", b"movie")
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        candidate = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )[0]
        self.write_file(folder / "new.nfo", b"new")

        valid, reason = dc.revalidate_candidate(candidate, EXTENSIONS)

        self.assertFalse(valid)
        self.assertIn("changé", reason)

    def test_deletion_never_follows_a_symlink(self):
        outside = self.write_file(self.root / "outside.txt", b"keep")
        folder = self.completed / "Film"
        self.write_file(folder / "Film.2020.mkv", b"movie")
        os.symlink(outside, folder / "link.txt")
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        candidate = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )[0]
        action_log = self.reports / "actions.jsonl"
        action_log.touch()

        _records, files, directories, success = dc.delete_candidate(
            candidate, self.completed, action_log
        )

        self.assertTrue(success)
        self.assertGreaterEqual(files, 2)
        self.assertGreaterEqual(directories, 1)
        self.assertEqual(outside.read_bytes(), b"keep")
        self.assertFalse(folder.exists())

    def test_report_directory_cannot_be_under_media(self):
        config = make_config(
            self.completed, self.media, self.media / "reports", self.root
        )
        with self.assertRaises(RuntimeError):
            dc.validate_configuration(config)

    def test_auto_candidate_requires_age_identity_quality_and_language(self):
        completed_file = self.write_file(
            self.completed / "Movie.Name.2024.MULTi.1080p.WEB.H264-GRP.mkv",
            b"completed-release",
        )
        media_file = self.write_file(
            self.media / "films" / "Movie.Name.2024.MULTi.2160p.WEB.H265-OTHER.mkv",
            b"different-media-release",
        )
        old_ns = time.time_ns() - 72 * 3600 * 1_000_000_000
        os.utime(completed_file, ns=(old_ns, old_ns))
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        video = completed_scan.videos[0]
        media_video = media_scan.videos[0]
        candidate = dc.DeletionCandidate(
            candidate_id=1,
            kind="file",
            path=video.path,
            videos=[video],
            entries=[video],
            snapshot={".": video.signature},
            similarities={str(video.path): [dc.Similarity(
                media_video.path, 0.95, media_video.size,
                dc.parse_scene_name(media_video.path),
            )]},
        )

        decision = dc.assess_auto_candidate(
            candidate, 0.90, 48, time.time_ns()
        )

        self.assertTrue(decision.eligible)
        self.assertEqual(decision.action, "delete")

        lower_path = media_file.with_name(
            "Movie.Name.2024.FRENCH.720p.WEB.H264-OTHER.mkv"
        )
        lower_parsed = dc.parse_scene_name(lower_path)
        candidate.similarities[str(video.path)] = [dc.Similarity(
            lower_path, 0.95, media_video.size, lower_parsed,
        )]
        refused = dc.assess_auto_candidate(
            candidate, 0.90, 48, time.time_ns()
        )
        self.assertFalse(refused.eligible)
        self.assertTrue(any(
            "resolution_jellyfin_inferieure" in reason
            or "langue_absente_ou_incompatible" in reason
            for reason in refused.reasons
        ))

    def test_auto_candidate_refuses_recent_and_failed_repair_paths(self):
        completed_file = self.write_file(
            self.completed / "Show.Name.S01E01.MULTi.1080p.mkv", b"episode"
        )
        media_file = self.write_file(
            self.media / "series" / "Show.Name.S01E01.MULTi.1080p.mkv", b"other"
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        video = completed_scan.videos[0]
        candidate = dc.DeletionCandidate(
            candidate_id=1, kind="file", path=video.path,
            videos=[video], entries=[video], snapshot={".": video.signature},
            similarities={str(video.path): [dc.Similarity(
                media_file.resolve(), 0.99, media_file.stat().st_size,
                dc.parse_scene_name(media_file),
            )]},
        )

        recent = dc.assess_auto_candidate(candidate, 0.90, 48, time.time_ns())
        blocked = dc.assess_auto_candidate(
            candidate, 0.90, 0, time.time_ns(),
            {dc.lexical_key(completed_file.resolve())},
        )

        self.assertFalse(recent.eligible)
        self.assertTrue(any("modification_plus_recente" in value for value in recent.reasons))
        self.assertFalse(blocked.eligible)
        self.assertTrue(any("reparation_hardlink_non_resolue" in value for value in blocked.reasons))

    def test_auto_folder_is_all_or_nothing(self):
        season = self.completed / "Show.Name.S01"
        first = self.write_file(season / "Show.Name.S01E01.MULTi.1080p.mkv", b"one")
        second = self.write_file(season / "Show.Name.S01E02.MULTi.1080p.mkv", b"two")
        old_ns = time.time_ns() - 72 * 3600 * 1_000_000_000
        os.utime(first, ns=(old_ns, old_ns))
        os.utime(second, ns=(old_ns, old_ns))
        os.utime(season, ns=(old_ns, old_ns))
        scan = dc.scan_tree(self.completed, EXTENSIONS)
        videos = sorted(scan.videos, key=lambda item: str(item.path))
        replacement = Path("/media/Show.Name.S01E01.MULTi.1080p.mkv")
        candidate = dc.DeletionCandidate(
            candidate_id=1, kind="folder", path=season.resolve(),
            videos=videos, entries=videos,
            snapshot={".": dc.make_entry(
                season.resolve(), season.resolve(), os.lstat(season), EXTENSIONS
            ).signature},
            similarities={str(videos[0].path): [dc.Similarity(
                replacement, 0.99, videos[0].size, dc.parse_scene_name(replacement)
            )]},
        )

        decision = dc.assess_auto_candidate(candidate, 0.90, 48, time.time_ns())

        self.assertFalse(decision.eligible)
        self.assertTrue(any(str(videos[1].path) in value for value in decision.reasons))

    def test_hardlink_interrupt_resumes_unmodified_torrent(self):
        payload = b"identical" * 128
        completed_file = self.write_file(self.completed / "Movie.2024.mkv", payload)
        self.write_file(self.media / "Movie.2024.mkv", payload)
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        repair = dc.discover_hardlink_repairs(completed_scan, media_scan, 0.60)[0]
        qbit_index = dc.QbitIndex(
            torrents={"hash1": dc.QbitTorrent("hash1", "Movie", "uploading", [])},
            path_to_hashes={dc.lexical_key(completed_file.resolve()): {"hash1"}},
        )
        client = RecordingQbitClient()
        actions = self.reports / "actions.jsonl"
        actions.touch()
        config = make_config(self.completed, self.media, self.reports, self.root)

        with mock.patch.object(
                dc, "create_hardlink_repair", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                dc.execute_hardlink_repairs(
                    config, [repair], client, qbit_index, actions,
                    dc.current_free(self.completed),
                )

        self.assertEqual(client.stopped, [{"hash1"}])
        self.assertEqual(client.started, [{"hash1"}])
        events = [json.loads(line) for line in actions.read_text().splitlines()]
        self.assertEqual(events[-1]["event"], "hardlink_batch_interrupted")

    def test_partial_torrent_is_left_stopped_and_reported(self):
        first = self.write_file(self.completed / "one.mkv", b"one")
        second = self.write_file(self.completed / "two.mkv", b"two")
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        candidates = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )
        selected = next(candidate for candidate in candidates if candidate.path == first.resolve())
        qbit_index = dc.QbitIndex(
            torrents={"hash1": dc.QbitTorrent("hash1", "Pair", "uploading", [
                dc.QbitFile(first.resolve(), "one.mkv", 3, 1),
                dc.QbitFile(second.resolve(), "two.mkv", 3, 1),
            ])},
            path_to_hashes={
                dc.lexical_key(first.resolve()): {"hash1"},
                dc.lexical_key(second.resolve()): {"hash1"},
            },
        )
        client = RecordingQbitClient()
        actions = self.reports / "actions.jsonl"
        actions.touch()
        summary = self.reports / "summary.txt"
        config = make_config(self.completed, self.media, self.reports, self.root)

        with mock.patch.object(dc, "wait_for_recheck", return_value=True):
            result = dc.execute_deletions(
                config, completed_scan, media_scan, [selected], client,
                qbit_index, actions, summary,
                automatic_torrent_cleanup=True,
            )

        self.assertEqual(client.stopped, [{"hash1"}])
        self.assertEqual(client.deleted, [])
        self.assertEqual(result.torrents_left_stopped, 1)
        self.assertEqual(result.stopped_torrent_hashes, ["hash1"])

    def test_id_ranges(self):
        self.assertEqual(dc.parse_id_expression("1, 3-5,9", {1, 2, 3, 4, 5}), {1, 3, 4, 5})


class FakeQbitHandler(BaseHTTPRequestHandler):
    calls = []
    version = "v5.0.4"
    login_no_content = False

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        FakeQbitHandler.calls.append(("GET", self.path, {}))
        if self.path == "/api/v2/app/version":
            self.reply(FakeQbitHandler.version)
        elif self.path == "/api/v2/torrents/info":
            self.reply(json.dumps([{
                "hash": "abc123",
                "name": "Movie",
                "state": "uploading",
                "save_path": "/data/completed",
                "content_path": "/data/completed/Movie.2024.mkv",
            }]), "application/json")
        elif self.path.startswith("/api/v2/torrents/files?"):
            self.reply(json.dumps([{
                "name": "Movie.2024.mkv", "size": 5, "priority": 1,
            }]), "application/json")
        else:
            self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        data = urllib.parse.parse_qs(self.rfile.read(length).decode())
        FakeQbitHandler.calls.append(("POST", self.path, data))
        if self.path == "/api/v2/auth/login":
            if FakeQbitHandler.login_no_content:
                self.send_response(204)
                self.send_header(
                    "Set-Cookie",
                    f"QBT_SID_{self.server.server_port}=test; HttpOnly; path=/",
                )
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Set-Cookie", "SID=test; HttpOnly; path=/")
            body = b"Ok."
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path in {
            "/api/v2/torrents/stop",
            "/api/v2/torrents/start",
            "/api/v2/torrents/pause",
            "/api/v2/torrents/resume",
            "/api/v2/torrents/recheck",
            "/api/v2/torrents/delete",
        }:
            self.reply("")
        else:
            self.send_error(404)

    def reply(self, value, content_type="text/plain"):
        body = value.encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class QbitTests(unittest.TestCase):
    def setUp(self):
        FakeQbitHandler.calls = []
        FakeQbitHandler.version = "v5.0.4"
        FakeQbitHandler.login_no_content = False
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeQbitHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.completed = self.root / "completed"
        self.media = self.root / "medias"
        self.reports = self.root / "reports"
        self.completed.mkdir()
        self.media.mkdir()
        self.reports.mkdir()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temporary.cleanup()

    def test_api_mapping_stop_recheck_and_safe_delete(self):
        video = self.completed / "Movie.2024.mkv"
        video.write_bytes(b"movie")
        url = f"http://127.0.0.1:{self.server.server_port}"
        client = dc.QbitClient(url, "admin", "secret")
        client.login()
        config = make_config(
            self.completed, self.media, self.reports, self.root,
            qbit_url=url,
        )
        scan = dc.scan_tree(self.completed, EXTENSIONS)

        index = dc.build_qbit_index(client, config, scan)
        client.stop({"abc123"})
        client.recheck({"abc123"})
        client.delete({"abc123"})

        self.assertEqual(index.torrents["abc123"].files[0].path, video.resolve())
        delete_calls = [call for call in FakeQbitHandler.calls if call[1].endswith("/delete")]
        self.assertEqual(delete_calls[0][2]["deleteFiles"], ["false"])
        self.assertTrue(any(call[1].endswith("/stop") for call in FakeQbitHandler.calls))

    def test_mapping_rejects_path_outside_container_root(self):
        with self.assertRaises(dc.QbitError):
            dc.map_container_path("/elsewhere/file", "/data", self.root)

    def test_qbittorrent_v4_uses_pause(self):
        FakeQbitHandler.version = "v4.6.7"
        url = f"http://127.0.0.1:{self.server.server_port}"
        client = dc.QbitClient(url, "admin", "secret")
        client.login()
        client.stop({"abc123"})

        self.assertTrue(any(call[1].endswith("/pause") for call in FakeQbitHandler.calls))

    def test_qbittorrent_v5_2_accepts_204_login(self):
        FakeQbitHandler.version = "v5.2.3"
        FakeQbitHandler.login_no_content = True
        url = f"http://127.0.0.1:{self.server.server_port}"
        client = dc.QbitClient(url, "admin", "secret")

        client.login()

        self.assertEqual(client.version, "v5.2.3")
        self.assertTrue(any(
            cookie.name.startswith("QBT_SID_") for cookie in client.cookies
        ))
        self.assertTrue(any(
            call[0] == "GET" and call[1] == "/api/v2/app/version"
            for call in FakeQbitHandler.calls
        ))

    def test_api_failure_is_wrapped(self):
        class FailingOpener:
            def open(self, *_args, **_kwargs):
                raise urllib.error.URLError("offline")

        client = dc.QbitClient("http://127.0.0.1:1", "admin", "secret")
        client.opener = FailingOpener()
        with self.assertRaises(dc.QbitError):
            client.login()

    def test_dry_run_does_not_mutate_files_or_qbittorrent(self):
        completed_file = self.completed / "Movie.2024.mkv"
        media_file = self.media / "Movie.2024.mkv"
        completed_file.write_bytes(b"movie")
        media_file.write_bytes(b"movie")
        completed_inode = completed_file.stat().st_ino
        media_inode = media_file.stat().st_ino
        url = f"http://127.0.0.1:{self.server.server_port}"
        config = make_config(
            self.completed, self.media, self.reports, self.root,
            qbit_url=url, run_mode="dry-run",
        )

        result = dc.main(config)

        self.assertEqual(result, 0)
        self.assertEqual(completed_file.stat().st_ino, completed_inode)
        self.assertEqual(media_file.stat().st_ino, media_inode)
        mutating = {"/api/v2/torrents/stop", "/api/v2/torrents/pause",
                    "/api/v2/torrents/recheck", "/api/v2/torrents/delete"}
        self.assertFalse(any(call[1] in mutating for call in FakeQbitHandler.calls))
        self.assertEqual(len(list(self.reports.glob("auto-plan-*.json"))), 1)
        csv_report = next(self.reports.glob("candidates-*.csv"))
        self.assertIn("auto_eligible", csv_report.read_text())

    def test_auto_mode_repairs_proven_duplicate_without_prompt(self):
        completed_file = self.completed / "Movie.2024.mkv"
        media_file = self.media / "Movie.2024.mkv"
        completed_file.write_bytes(b"movie")
        media_file.write_bytes(b"movie")
        completed_inode = completed_file.stat().st_ino
        old_media_inode = media_file.stat().st_ino
        url = f"http://127.0.0.1:{self.server.server_port}"
        config = make_config(
            self.completed, self.media, self.reports, self.root,
            qbit_url=url, run_mode="auto",
        )

        with mock.patch.object(dc, "wait_for_recheck", return_value=True):
            result = dc.main(config)

        self.assertEqual(result, 0)
        self.assertEqual(completed_file.stat().st_ino, completed_inode)
        self.assertEqual(media_file.stat().st_ino, completed_inode)
        self.assertNotEqual(old_media_inode, media_file.stat().st_ino)
        self.assertTrue(any(
            call[1] == "/api/v2/torrents/stop" for call in FakeQbitHandler.calls
        ))
        self.assertTrue(any(
            call[1] == "/api/v2/torrents/recheck" for call in FakeQbitHandler.calls
        ))
        self.assertEqual(len(list(self.reports.glob("auto-plan-*.json"))), 2)

    def test_automatic_cleanup_removes_only_empty_torrent_without_files(self):
        video = self.completed / "Movie.2024.mkv"
        video.write_bytes(b"movie")
        url = f"http://127.0.0.1:{self.server.server_port}"
        client = dc.QbitClient(url, "admin", "secret")
        client.login()
        config = make_config(
            self.completed, self.media, self.reports, self.root, qbit_url=url,
        )
        completed_scan = dc.scan_tree(self.completed, EXTENSIONS)
        media_scan = dc.scan_tree(self.media, EXTENSIONS)
        candidate = dc.build_deletion_candidates(
            completed_scan, media_scan, 0.60, EXTENSIONS
        )[0]
        qbit_index = dc.build_qbit_index(client, config, completed_scan)
        actions = self.reports / "actions.jsonl"
        actions.touch()
        summary = self.reports / "summary.txt"

        with mock.patch.object(dc, "wait_for_recheck", return_value=True):
            result = dc.execute_deletions(
                config, completed_scan, media_scan, [candidate], client,
                qbit_index, actions, summary,
                automatic_torrent_cleanup=True,
            )

        self.assertEqual(result.torrents_removed, 1)
        delete_call = next(
            call for call in FakeQbitHandler.calls
            if call[1] == "/api/v2/torrents/delete"
        )
        self.assertEqual(delete_call[2]["deleteFiles"], ["false"])


def make_config(completed, media, reports, host_root,
                qbit_url="http://127.0.0.1:8080", **overrides):
    values = dict(
        completed_path=completed.resolve(),
        media_path=media.resolve(),
        report_path=reports.resolve(),
        qbittorrent_url=qbit_url,
        qbittorrent_username="admin",
        qbittorrent_password="secret",
        qbittorrent_container_path="/data",
        qbittorrent_host_path=host_root.resolve(),
        video_extensions=EXTENSIONS,
        similarity_threshold=0.60,
        recheck_timeout_seconds=1,
        quick_sample_size=1024 * 1024,
        quick_sample_count=5,
        full_compare_chunk_size=1024 * 1024,
        search_similar_names_for_hardlink=False,
    )
    values.update(overrides)
    return dc.Config(**values)


if __name__ == "__main__":
    unittest.main()
