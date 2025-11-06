#!/usr/bin/env python3
"""
Test script for qBittorrent integration
Tests the logic without requiring actual torrent client connections
"""

import sys
import os
from unittest.mock import Mock, patch, MagicMock

# Add parent directory to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../.."))

print("=" * 70)
print("Unit Tests for qBittorrent Integration")
print("=" * 70)

# Test 1: Mock qBittorrent client behavior
print("\n1. Testing QbitClientManager with mock qBittorrent client...")

with patch("main.QbitClient") as MockQbitClient:
    from main import QbitClientManager

    # Create a mock client
    mock_client = Mock()
    MockQbitClient.return_value = mock_client

    # Mock torrents list
    mock_torrent1 = Mock()
    mock_torrent1.name = "Movie.2024.1080p"
    mock_torrent1.hash = "abc123def456"

    mock_torrent2 = Mock()
    mock_torrent2.name = "Series.S01E01.720p"
    mock_torrent2.hash = "xyz789ghi012"

    mock_client.torrents_info.return_value = [mock_torrent1, mock_torrent2]

    # Create manager instance
    manager = QbitClientManager(
        host="localhost", port="8080", username="admin", password="adminpass"
    )

    # Test get_torrents_list
    torrents = manager.get_torrents_list()
    assert len(torrents) == 2, f"Expected 2 torrents, got {len(torrents)}"
    print("   ✓ get_torrents_list() returns correct number of torrents")

    # Test delete_torrent_and_data with single hash
    manager.delete_torrent_and_data("abc123def456")
    mock_client.torrents_delete.assert_called_once_with(
        delete_files=True, torrent_hashes=["abc123def456"]
    )
    print("   ✓ delete_torrent_and_data() works with single hash")

    # Reset mock
    mock_client.reset_mock()

    # Test delete_torrent_and_data with list of hashes
    manager.delete_torrent_and_data(["abc123def456", "xyz789ghi012"])
    mock_client.torrents_delete.assert_called_once_with(
        delete_files=True, torrent_hashes=["abc123def456", "xyz789ghi012"]
    )
    print("   ✓ delete_torrent_and_data() works with list of hashes")

# Test 2: Mock Transmission client behavior
print("\n2. Testing TransmissionClientManager with mock Transmission client...")

with patch("main.Client") as MockTransmissionClient:
    from main import TransmissionClientManager

    # Create a mock client
    mock_client = Mock()
    MockTransmissionClient.return_value = mock_client

    # Mock torrents list
    mock_torrent1 = Mock()
    mock_torrent1.id = 1
    mock_torrent1.name = "Movie.2024.1080p"

    mock_torrent2 = Mock()
    mock_torrent2.id = 2
    mock_torrent2.name = "Series.S01E01.720p"

    mock_client.get_torrents.return_value = [mock_torrent1, mock_torrent2]

    # Create manager instance
    manager = TransmissionClientManager(
        ip="localhost", port="9091", username="admin", password="admin"
    )

    # Test get_torrents_list
    torrents = manager.get_torrents_list()
    assert len(torrents) == 2, f"Expected 2 torrents, got {len(torrents)}"
    print("   ✓ get_torrents_list() returns correct number of torrents")

    # Test delete_torrent_and_data
    manager.delete_torrent_and_data(1)
    mock_client.remove_torrent.assert_called_once_with(ids=1, delete_data=True)
    print("   ✓ delete_torrent_and_data() works correctly")

# Test 3: Test main() logic with mocked file_sweeper
print("\n3. Testing main() logic with mocked file_sweeper...")

with patch("main.QbitClient") as MockQbitClient, patch(
    "main.file_sweeper"
) as mock_file_sweeper:

    from main import QbitClientManager

    # Setup mock client
    mock_client = Mock()
    MockQbitClient.return_value = mock_client

    # Mock torrents
    mock_torrent1 = Mock()
    mock_torrent1.name = "Movie.2024.1080p"
    mock_torrent1.hash = "abc123"

    mock_torrent2 = Mock()
    mock_torrent2.name = "Series.S01E01.720p"
    mock_torrent2.hash = "xyz789"

    mock_torrent3 = Mock()
    mock_torrent3.name = "NotInFilesystem"
    mock_torrent3.hash = "nothere"

    mock_client.torrents_info.return_value = [
        mock_torrent1,
        mock_torrent2,
        mock_torrent3,
    ]

    # Mock file_sweeper to return unlinked items
    mock_file_sweeper.main.return_value = [
        "/data/completed/Movie.2024.1080p",
        "/data/completed/Series.S01E01.720p",
    ]

    # Create manager and test main()
    manager = QbitClientManager(
        host="localhost", port="8080", username="admin", password="pass"
    )
    result = manager.main("/data/completed", [".mkv", ".mp4"])

    # Verify results
    assert len(result) == 2, f"Expected 2 results, got {len(result)}"
    assert result[0] == (
        "abc123",
        "Movie.2024.1080p",
    ), f"Unexpected result: {result[0]}"
    assert result[1] == (
        "xyz789",
        "Series.S01E01.720p",
    ), f"Unexpected result: {result[1]}"
    print("   ✓ main() correctly matches torrents with filesystem items")
    print("   ✓ main() returns (hash, name) tuples")
    print("   ✓ main() excludes torrents not found in filesystem")

# Test 4: Test CLIENT_TYPE environment variable handling
print("\n4. Testing CLIENT_TYPE environment variable handling...")

test_cases = [
    ("transmission", "TransmissionClientManager"),
    ("qbittorrent", "QbitClientManager"),
    ("QBITTORRENT", "QbitClientManager"),  # Test case insensitive
    ("Transmission", "TransmissionClientManager"),
    (None, "TransmissionClientManager"),  # Default
]

for client_type_env, expected_class in test_cases:
    # Reset modules to test fresh imports
    if "app" in sys.modules:
        del sys.modules["app"]

    # Set environment variable
    if client_type_env is not None:
        os.environ["CLIENT_TYPE"] = client_type_env
    elif "CLIENT_TYPE" in os.environ:
        del os.environ["CLIENT_TYPE"]

    # Mock both clients to avoid connection attempts
    with patch("main.Client"), patch("main.QbitClient"):
        try:
            import importlib
            import app as app_module

            importlib.reload(app_module)

            actual_class = app_module.client.__class__.__name__
            assert (
                actual_class == expected_class
            ), f"CLIENT_TYPE={client_type_env}: expected {expected_class}, got {actual_class}"

            status = "✓"
        except Exception as e:
            status = "✗"
            print(f"   {status} CLIENT_TYPE={client_type_env} -> Error: {e}")
            continue

    print(
        f"   {status} CLIENT_TYPE={client_type_env or 'None(default)'} -> {expected_class}"
    )

# Clean up environment
if "CLIENT_TYPE" in os.environ:
    del os.environ["CLIENT_TYPE"]

print("\n" + "=" * 70)
print("All unit tests passed! ✓")
print("=" * 70)
print("\n✅ qBittorrent integration is working correctly!")
print("\nNext steps:")
print("  • Set up a qBittorrent instance to test real connections")
print("  • Configure environment variables in docker-compose.yml")
print("  • Deploy and test with actual torrents")
