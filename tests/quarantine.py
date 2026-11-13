"""Known-flaky test steps, listed with their issues in quarantine.txt.

A listed step still runs. Its result is printed as "QUARANTINED PASS" or
"QUARANTINED FAIL (#issue): step: error", and a failure does not stop the
test. A step that is not listed fails as usual.
"""

import contextlib
from pathlib import Path
import sys

LIST = Path(__file__).with_name("quarantine.txt")


def issue(name):
    """The issue a quarantined step is tracked in, or None."""
    for line in LIST.read_text().splitlines():
        fields = line.split()
        if fields and not line.startswith("#") and fields[0] == name:
            return fields[1]
    return None


@contextlib.contextmanager
def step(name):
    tracked = issue(name)
    if tracked is None:
        yield
        return
    try:
        yield
    except Exception as error:
        print(f"QUARANTINED FAIL ({tracked}): {name}: {str(error)[:1000]}", file=sys.stderr, flush=True)
    else:
        print(f"QUARANTINED PASS ({tracked}): {name}", file=sys.stderr, flush=True)
