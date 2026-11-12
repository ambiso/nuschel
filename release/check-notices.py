#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Gate downloadable CI payloads on the same actual notice policy as publication.
This checks evidence and bytes, not legal completeness. Diagnostic logs are kept
independently even if this gate refuses a binary/static-web artifact.
"""
from pathlib import Path
import runpy
import sys
import argparse
import json

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--scope", choices=sorted(json.loads(Path(__file__).with_name("notice-scopes.json").read_bytes())), required=True)
parser.add_argument("directory", type=Path)
args = parser.parse_args()
source = Path(__file__).resolve().parent
policy = runpy.run_path(str(source / "prepare-artifacts.py"))
policy["verify_notices"](args.directory, policy["load"](source / "notice-roots.json"), args.scope)
print("source-scoped runtime notice inventory and actual bundled bytes verified")
