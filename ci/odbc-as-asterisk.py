#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Runs every func_odbc readsql (asterisk/etc/func_odbc.conf) as the database role `asterisk`
against a migrated database and fails on any error: a missing GRANT breaks calls at runtime only
(the dialplan just gets an empty result), so CI must catch it.

Usage: ci/odbc-as-asterisk.py  (psql connection from PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE;
the user must be allowed to SET ROLE asterisk, e.g. the owner role `pbx`)."""
import re
import subprocess
import sys
from pathlib import Path

conf = Path(__file__).resolve().parent.parent / "asterisk/etc/func_odbc.conf"
lines = [l for l in conf.read_text().splitlines() if not l.lstrip().startswith(";")]
text = "\n".join(lines) + "\n"
failed = 0
count = 0
for name, body in re.findall(r"^\[([A-Z_0-9]+)\]\n(.*?)(?=^\[|\Z)", text, flags=re.S | re.M):
    m = re.search(r"^readsql\s*=\s*(.*?)(?=^\w+\s*=|\Z)", body, flags=re.S | re.M)
    if not m:
        continue
    q = re.sub(r"\s*\n\s*", " ", m.group(1)).strip()
    # arguments: a harmless literal (also inside SQL_ESC); the query shape and grants matter here
    q = re.sub(r"\$\{SQL_ESC\(\$\{ARG\d\}\)\}", "1", q)
    q = re.sub(r"\$\{ARG\d\}", "1", q)
    count += 1
    r = subprocess.run(["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-c", "SET ROLE asterisk", "-c", q],
                       capture_output=True, text=True)
    if r.returncode != 0:
        failed += 1
        print(f"[{name}] {r.stderr.strip()}", file=sys.stderr)
print(f"{count} func_odbc queries as role asterisk, {failed} failed")
sys.exit(1 if failed or count == 0 else 0)
