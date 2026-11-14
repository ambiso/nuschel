"""Known-flaky test steps, listed with their issues in quarantine.txt.

A listed step still runs. Its result is printed as "QUARANTINED PASS" or
"QUARANTINED FAIL (#issue): step: error" with the traceback, and a failure
does not stop the test. A step that is not listed fails as usual.
"""

import contextlib
from pathlib import Path
import sys
import traceback

LIST = Path(__file__).with_name("quarantine.txt")


def issue(name):
    """The issue a quarantined step is tracked in, or None."""
    for line in LIST.read_text().splitlines():
        fields = line.split()
        if fields and not line.startswith("#") and fields[0] == name:
            return fields[1]
    return None


@contextlib.contextmanager
def step(name, evidence=None):
    """Run a step; after a quarantined failure, `evidence()` (e.g. a screenshot) is kept and named."""
    tracked = issue(name)
    if tracked is None:
        yield
        return
    try:
        yield
    except Exception as error:
        report = f"QUARANTINED FAIL ({tracked}): {name}: {type(error).__name__}: {error}\n{traceback.format_exc()}"
        if evidence:
            try:
                report += f"evidence: {evidence()}\n"
            except Exception as failure:
                report += f"no evidence: {failure!r}\n"
        print(report, file=sys.stderr, flush=True)
    else:
        print(f"QUARANTINED PASS ({tracked}): {name}", file=sys.stderr, flush=True)
