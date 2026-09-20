#!/usr/bin/env python3
"""Check committed documentation links, TOML and the design schema offline."""
import json
from pathlib import Path
import re
import sqlite3
import tempfile
import tomllib
from urllib.parse import unquote

root = Path(__file__).resolve().parents[1]
files = [root / "README.md", *sorted((root / "docs").rglob("*.md")),
         *sorted((root / "reports/0.1.0-lab").glob("*.md"))]
links = 0
for file in files:
    source = file.read_text(encoding="utf-8")
    assert source.count("```") % 2 == 0, f"Unclosed fence: {file}"
    for target in re.findall(r"\[[^\]\n]*\]\(([^)\n]+)\)", source):
        if re.match(r"[a-z]+://", target) or target.startswith("#"):
            continue
        destination = unquote(target.split("#", 1)[0].strip("<>"))
        assert (file.parent / destination).is_file(), f"Broken link: {file}: {target}"
        links += 1
    assert not re.search(r"\bfsf\b", source, re.I), f"Stale name: {file}"

for name in ["rustymail.example.toml", "rustymail.lab.toml"]:
    config = tomllib.loads((root / "deploy" / name).read_text(encoding="utf-8"))
    assert config["mode"] == "lab"
    assert config["delivery"]["mode"] == "disabled"
    assert config["store"]["synchronous"] == "full"
    assert all(v.startswith("127.0.0.1:") for v in config["listeners"].values())

with tempfile.TemporaryDirectory(prefix="rustymail-docs-") as temporary:
    connection = sqlite3.connect(Path(temporary) / "schema.sqlite")
    try:
        connection.executescript((root / "docs/examples/schema.sql").read_text(encoding="utf-8"))
        assert connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        assert connection.execute("PRAGMA foreign_key_check").fetchall() == []
        assert connection.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
        assert connection.execute("PRAGMA synchronous").fetchone()[0] == 2
    finally:
        connection.close()
print(json.dumps({"markdown_files": len(files), "local_links": links, "toml": "passed", "schema": "passed"}))
