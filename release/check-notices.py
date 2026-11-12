#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Gate downloadable CI payloads on the same actual notice policy as publication.
This checks evidence and bytes, not legal completeness. Diagnostic logs are kept
independently even if this gate refuses a binary/static-web artifact.
"""
from pathlib import Path
import runpy
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: check-notices.py ACTUAL_BUNDLED_NOTICE_DIRECTORY")
source = Path(__file__).resolve().parent
policy = runpy.run_path(str(source / "prepare-artifacts.py"))
policy["verify_notices"](Path(sys.argv[1]), policy["load"](source / "notice-roots.json"))
print("source-scoped runtime notice inventory and actual bundled bytes verified")
