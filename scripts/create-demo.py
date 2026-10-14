#!/usr/bin/env python3
"""Create an isolated SQLite database for reviewing the Slint port locally."""
import sqlite3
from pathlib import Path

path = Path(__file__).resolve().parent.parent / 'target' / 'pueo-demo.sqlite'
path.parent.mkdir(parents=True, exist_ok=True)
if path.exists():
    raise SystemExit(f'Demo already exists: {path}')
with sqlite3.connect(path) as db:
    db.executescript('''
        CREATE TABLE customers (
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE,
            active BOOLEAN NOT NULL DEFAULT 1 CHECK(active IN (0, 1)),
            notes TEXT, created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX customers_name ON customers(name);
        CREATE VIEW active_customers AS SELECT id, name, email FROM customers WHERE active = 1;
    ''')
    db.executemany('INSERT INTO customers(name,email,active,notes) VALUES (?,?,?,?)', [
        (f'Customer {i:03}', f'customer{i}@example.com', i % 3 != 0,
         'A multiline note\nwith a second line' if i == 1 else None)
        for i in range(1, 651)
    ])
print(path)
