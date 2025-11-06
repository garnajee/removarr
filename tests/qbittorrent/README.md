# qBittorrent Integration Tests

This directory contains tests for the qBittorrent integration in Removarr.

## Test Files

### `test_qbit_integration.py`
Comprehensive unit tests for qBittorrent integration including:
- Import verification
- Class structure validation
- Method testing with mock clients
- Environment variable handling
- Code quality checks

**Run with:**
```bash
cd /workspaces/removarr
python3 tests/qbittorrent/test_qbit_integration.py
```

### `test_integration_demo.py`
Integration test demonstrating the complete workflow for both Transmission and qBittorrent:
- Connection simulation
- Torrent fetching
- Filesystem scanning
- Torrent deletion
- End-to-end workflow verification

**Run with:**
```bash
cd /workspaces/removarr
python3 tests/qbittorrent/test_integration_demo.py
```

## Running All Tests

To run all qBittorrent tests:
```bash
cd /workspaces/removarr
for test in tests/qbittorrent/test_*.py; do
    echo "Running $test..."
    python3 "$test"
    echo ""
done
```

## Requirements

All tests use Python's built-in `unittest.mock` module and don't require actual torrent client connections. They verify the logic and structure without needing live services.

## Test Coverage

✅ Import validation  
✅ Class structure verification  
✅ Method functionality testing  
✅ Environment variable handling  
✅ Client type switching  
✅ Complete workflow simulation  
✅ Code quality checks  
