#!/usr/bin/env python3
"""Seed channels in a stopped, freshly initialized stock Mumble test server."""
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as database:
    assert database.execute("SELECT name FROM channels WHERE server_id = 1 AND channel_id = 0").fetchone()
    database.executemany(
        "INSERT OR IGNORE INTO channels (server_id, channel_id, parent_id, name, inheritacl) VALUES (1, ?, ?, ?, 1)",
        [(1, 0, "Lobby"), (2, 1, "Development")],
    )
