#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Rank open Nuschl issues in AGENTS.md queue order and claim them atomically.

    next-issues.py [--all] [--json] [--limit N] [--label L]...
    next-issues.py acquire-next --agent NAME [--hours 24] [--note TEXT] [--label L]...
    next-issues.py claim N --agent NAME [--hours 24] [--note TEXT]
    next-issues.py release N --agent NAME [--force]
    next-issues.py claims

Ranking is read-only: one paginated `gh api graphql` query with your gh login.

Eligible: an approval basis (status:approved, source:user-request, or a
nuschl-todo-import marker whose TODO.md source link matches a row of
docs/todo-issue-map.tsv for this issue, with the snapshot matching the map,
on an issue opened by the repository owner),
no open blocker, no hold, not in progress, not assigned and not claimed.

Order: effective priority (urgent > high > medium > low; unset = medium; the
highest of several labels), thumbs-up on the issue, then age. An open blocker
inherits the highest priority of the approved, unheld work that transitively
waits for it ("high^ via #A←#B"), but never its approval: unapproved or held
blockers are listed for a human.

Claims live in claims.json under --claims-dir, changed only under an exclusive
flock and replaced atomically. Re-claiming as the same agent renews a claim;
an expired claim can be taken over. acquire-next ranks first, then re-checks
the claims under the lock and claims the first free eligible issue.
"""
import argparse
import calendar
import contextlib
import fcntl
import graphlib
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

DEFAULT_REPO = "ambiso/nuschl"
CLAIMS_DIR = os.path.expanduser("~/.cache/nuschl-coordination/claims")
MAP_PATH = "docs/todo-issue-map.tsv"
SNAPSHOT_PATH = "docs/history/2026-11-13-todo-snapshot.md"
SNAPSHOT_SEPARATOR = "\n---\n\n"
PRIORITIES = ["urgent", "high", "medium", "low"]
HOLD_LABELS = {"status:deferred": "deferred", "type:tracking": "tracking",
               "type:operator-action": "operator-action", "duplicate": "duplicate",
               "invalid": "invalid", "wontfix": "wontfix", "question": "question"}
MARKER_RE = re.compile(r"<!-- nuschl-todo-import: [a-z0-9-]+ -->")
TODO_LINK = r"github\.com/%s/blob/([0-9a-f]{40})/TODO\.md#L(\d+)(?:-L(\d+))?"
AGENT_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9 ._:@/+()-]{0,63}$")
TIME_FORMAT = "%Y-%m-%dT%H:%M:%SZ"

QUERY = """
query($owner: String!, $name: String!, $cursor: String, $withFiles: Boolean!,
      $mapExpr: String!, $snapshotExpr: String!) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    defaultBranchRef { name target { oid } }
    todoMap: object(expression: $mapExpr) @include(if: $withFiles) { ... on Blob { text } }
    todoSnapshot: object(expression: $snapshotExpr) @include(if: $withFiles) { ... on Blob { text } }
    issues(states: OPEN, first: 100, after: $cursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number title url createdAt body author { login }
        assignees(first: 10) { nodes { login } }
        labels(first: 50) { nodes { name } }
        reactions(content: THUMBS_UP) { totalCount }
        blockedBy(first: 50) { totalCount nodes { number state repository { nameWithOwner } } }
        subIssuesSummary { total completed }
      }
    }
  }
}
"""


class FetchError(Exception):
    pass


class ClaimError(Exception):
    pass


# GitHub

def gh_graphql(variables):
    command = ["gh", "api", "graphql", "-f", "query=" + QUERY]
    for key, value in variables.items():
        if isinstance(value, bool):
            command += ["-F", "%s=%s" % (key, str(value).lower())]
        elif value is not None:
            command += ["-f", "%s=%s" % (key, value)]
    try:
        proc = subprocess.run(command, capture_output=True, text=True, timeout=120, check=False)
        data = json.loads(proc.stdout)
    except (OSError, subprocess.TimeoutExpired, ValueError) as error:
        raise FetchError("gh api graphql failed: %s" % error) from error
    if proc.returncode or data.get("errors") or not data.get("data"):
        raise FetchError("GraphQL error: %s %s" % (json.dumps(data.get("errors"))[:500], proc.stderr.strip()[:200]))
    return data["data"]


def fetch(repo, graphql=gh_graphql):
    owner, name = repo.split("/", 1)
    cursor, issues, first = None, [], None
    while True:
        repository = graphql({"owner": owner, "name": name, "cursor": cursor, "withFiles": first is None,
                              "mapExpr": "HEAD:" + MAP_PATH, "snapshotExpr": "HEAD:" + SNAPSHOT_PATH})["repository"]
        if repository is None:
            raise FetchError("repository %s not found" % repo)
        first = first or repository
        seen = {issue["number"] for issue in issues}
        issues += [issue for issue in repository["issues"]["nodes"] if issue["number"] not in seen]
        page = repository["issues"]["pageInfo"]
        if not page["hasNextPage"]:
            return first, issues
        cursor = page["endCursor"]


# TODO-import verification

def load_todo_map(map_text, snapshot_text):
    """Return ({revision, ranges: {(start, end): issue numbers}}, None) or (None, problem)."""
    if not map_text or not snapshot_text:
        return None, "TODO map or snapshot unavailable"
    meta = dict(re.findall(r"^# (\w+)=(\S+)", map_text, re.M))
    _, separator, source = snapshot_text.partition(SNAPSHOT_SEPARATOR)
    if not separator or hashlib.sha256(source.encode()).hexdigest() != meta.get("source_sha256"):
        return None, "TODO snapshot does not match the map's source_sha256"
    ranges = {}
    for line in map_text.splitlines():
        columns = line.split("\t")
        if line.startswith("#") or columns[0] == "source_id" or not line.strip():
            continue
        try:
            issues = set() if columns[5] == "-" else {int(n) for n in columns[5].split(",")}
            ranges.setdefault((int(columns[2]), int(columns[3])), set()).update(issues)
        except (IndexError, ValueError):
            return None, "malformed TODO map line: %r" % line[:80]
    return {"revision": meta.get("source_revision"), "ranges": ranges}, None


def todo_marker(number, body, author, todo, repo):
    """None without a marker; "verified" when the repository owner opened the issue
    and a source link names a map row for this issue."""
    if not MARKER_RE.search(body):
        return None
    if author != repo.split("/", 1)[0]:
        return "unverified"
    links = [(int(start), int(end or start)) for revision, start, end in re.findall(TODO_LINK % re.escape(repo), body)
             if todo and revision == todo["revision"]]
    return "verified" if any(number in todo["ranges"].get(link, ()) for link in links) else "unverified"


# Ranking

def evaluate(issue, repo, todo):
    labels = sorted(label["name"] for label in issue["labels"]["nodes"])
    priorities = [label[9:] for label in labels if label.startswith("priority:") and label[9:] in PRIORITIES]
    priority = min(priorities, key=PRIORITIES.index, default="medium")
    marker = todo_marker(issue["number"], issue["body"] or "", (issue.get("author") or {}).get("login"), todo, repo)
    approval = [label for label in ("status:approved", "source:user-request") if label in labels]
    approval += ["todo-import"] if marker == "verified" else []
    holds = [HOLD_LABELS[label] for label in labels if label in HOLD_LABELS]
    if "status:needs-approval" in labels and "status:approved" not in labels:
        holds.insert(0, "needs-approval")
    summary = issue.get("subIssuesSummary") or {"total": 0, "completed": 0}
    if summary["total"] > summary["completed"]:
        holds.append("%d open sub-issues" % (summary["total"] - summary["completed"]))
    blockers, external = [], []
    for node in issue["blockedBy"]["nodes"]:
        if node["state"] != "OPEN":
            continue
        other = node["repository"]["nameWithOwner"]
        if other == repo:
            blockers.append(node["number"])
        else:
            external.append("%s#%d" % (other, node["number"]))
    unlisted = issue["blockedBy"]["totalCount"] - len(issue["blockedBy"]["nodes"])
    if unlisted > 0:
        external.append("+%d unlisted" % unlisted)
    return {"number": issue["number"], "title": issue["title"], "url": issue["url"],
            "created_at": issue["createdAt"], "labels": labels, "priority": priority,
            "effective_priority": priority, "priority_via": [], "thumbs_up": issue["reactions"]["totalCount"],
            "sort_thumbs": issue["reactions"]["totalCount"], "approval": approval, "todo_marker": marker,
            "blockers": blockers, "external_blockers": external, "holds": holds,
            "in_progress": "status:in-progress" in labels,
            "assignees": sorted(node["login"] for node in issue["assignees"]["nodes"]), "claim": None}


def walk(start, graph):
    """Breadth-first over open blockers: (node, path from start to its dependent); cycle-safe."""
    parents, queue = {start: None}, [start]
    for node in queue:
        for blocker in graph.get(node, ()):
            if blocker not in parents:
                parents[blocker] = node
                queue.append(blocker)
                path, step = [], node
                while step is not None:
                    path.insert(0, step)
                    step = parents[step]
                yield blocker, path


def resolve(records):
    """Lift blockers to the priority of approved, unheld work waiting for them; never approve them."""
    by_number = {r["number"]: r for r in records}
    graph = {r["number"]: [b for b in r["blockers"] if b in by_number] for r in records}
    best, needed_by = {}, {}
    for root in records:
        if not root["approval"] or root["holds"]:
            continue
        for node, path in walk(root["number"], graph):
            # Highest priority first, then the root's thumbs-up, then the nearest root.
            key = (PRIORITIES.index(root["priority"]), -root["thumbs_up"], len(path), root["created_at"], path)
            best[node] = min(best.get(node, key), key)
            needed_by.setdefault(node, set()).add(root["number"])
    attention = []
    for number, (level, thumbs, _, _, path) in sorted(best.items()):
        record = by_number[number]
        if level < PRIORITIES.index(record["priority"]):
            record.update(effective_priority=PRIORITIES[level], priority_via=path, sort_thumbs=-thumbs)
        need = ("held: " + ", ".join(record["holds"])) if record["holds"] else (
            "needs approval" if not record["approval"] else None)
        if need:
            attention.append({"number": number, "need": need, "blocks": sorted(needed_by[number])})
    try:
        graphlib.TopologicalSorter(graph).prepare()
        cycle = []
    except graphlib.CycleError as error:
        cycle = error.args[1]
    return graph, attention, cycle


def analyse(repository, issues, map_text, snapshot_text, claims=None, now=None,
            include_in_progress=False, include_assigned=False):
    repo = repository["nameWithOwner"]
    todo, problem = load_todo_map(map_text, snapshot_text)
    records = [evaluate(issue, repo, todo) for issue in issues]
    graph, attention, cycle = resolve(records)
    now = time.time() if now is None else now
    for record in records:
        claim = (claims or {}).get(record["number"])
        record["claim"] = claim if claim and claim_active(claim, now) else None
        reasons = [] if record["approval"] else [
            "no approval" + (" (todo marker unverified)" if record["todo_marker"] == "unverified" else "")]
        if record["blockers"] or record["external_blockers"]:
            reasons.append("blocked by " + ", ".join(["#%d" % n for n in record["blockers"]]
                                                     + record["external_blockers"]))
        reasons += ["hold: " + hold for hold in record["holds"]]
        if record["in_progress"] and not include_in_progress:
            reasons.append("in progress")
        if record["assignees"] and not include_assigned:
            reasons.append("assigned: " + ", ".join(record["assignees"]))
        if record["claim"]:
            reasons.append("claimed by %s until %s" % (record["claim"]["agent"], record["claim"]["lease_until"]))
        record.update(reasons=reasons, eligible=not reasons)
    records.sort(key=lambda r: (PRIORITIES.index(r["effective_priority"]), -r["sort_thumbs"],
                                r["created_at"], r["number"]))
    eligible = [r for r in records if r["eligible"]]
    for record in records:
        record["queue_position"] = eligible.index(record) + 1 if record["eligible"] else None
    branch = repository.get("defaultBranchRef") or {}
    return {"repository": repo, "default_branch": branch.get("name"),
            "default_branch_oid": (branch.get("target") or {}).get("oid"),
            "open_issues": len(records), "eligible_issues": len(eligible),
            "todo_map_problem": problem, "cycle": cycle, "attention": attention,
            "graph": graph, "issues": records}


def select(result, show_all=False, labels=(), limit=0):
    rows = [r for r in result["issues"]
            if (show_all or r["eligible"]) and all(label in r["labels"] for label in labels)]
    return rows[:limit] if limit > 0 else rows


# Output

def priority_text(record):
    text = record["effective_priority"]
    if record["priority_via"]:
        text += "^ via " + "←".join("#%d" % n for n in record["priority_via"])
    elif not any(label.startswith("priority:") for label in record["labels"]):
        text += "~"
    return text


def short_state(record):
    if record["eligible"]:
        return "Q%d" % record["queue_position"]
    if record["holds"]:
        return "held"
    if not record["approval"]:
        return "needs approval"
    if record["claim"] or record["assignees"] or record["in_progress"]:
        return "taken"
    return "blocked"


def render(result, rows, show_all):
    by_number = {r["number"]: r for r in result["issues"]}
    names = {"status:approved": "approved", "source:user-request": "user-req", "todo-import": "todo"}
    header = ["Q", "ISSUE", "PRIO", "+1", "APPROVAL"] + (["NOT ELIGIBLE BECAUSE"] if show_all else [])
    table = [[str(r["queue_position"] or "-"), "#%d" % r["number"], priority_text(r), str(r["thumbs_up"]),
              "+".join(names[a] for a in r["approval"]) or "-"] + (["; ".join(r["reasons"]) or "-"] if show_all else [])
             for r in rows]
    widths = [max(len(row[i]) for row in [header] + table) for i in range(len(header))]
    title_width = max(20, shutil.get_terminal_size((120, 24)).columns - sum(widths) - 2 * len(widths))
    lines = ["%s @ %s %s: %d open, %d eligible; showing %d." % (
        result["repository"], result["default_branch"], (result["default_branch_oid"] or "")[:8],
        result["open_issues"], result["eligible_issues"], len(rows))]
    for cells, title in [(header, "TITLE")] + [(row, r["title"]) for row, r in zip(table, rows, strict=True)]:
        title = title if len(title) <= title_width else title[:title_width - 1] + "…"
        lines.append("  ".join(c.ljust(w) for c, w in zip(cells, widths, strict=True)) + "  " + title)
    lines.append("~ unset priority; ^ via #A←#B: inherited from approved #A, which waits for #B (priority only).")
    waiting = []
    for r in result["issues"]:
        if (r["effective_priority"] in ("urgent", "high") and r["approval"] and not r["holds"]
                and (r["blockers"] or r["external_blockers"])):
            parts = ["#%d %s" % (n, short_state(by_number[n])) for n, _ in walk(r["number"], result["graph"])]
            waiting.append("  #%d %s waits for %s" % (r["number"], r["effective_priority"],
                                                      ", ".join(parts + r["external_blockers"])))
    if waiting:
        lines += ["Blocked urgent/high work:"] + waiting
    if result["attention"]:
        lines.append("Blockers of approved work that need a human:")
        lines += ["  #%d %s; blocks %s" % (a["number"], a["need"], ", ".join(
            "#%d (%s)" % (n, by_number[n]["effective_priority"]) for n in a["blocks"])) for a in result["attention"]]
    if result["cycle"]:
        lines.append("warning: dependency cycle " + " -> ".join("#%d" % n for n in result["cycle"]))
    if result["todo_map_problem"]:
        lines.append("warning: %s; TODO-import approvals are off" % result["todo_map_problem"])
    return "\n".join(lines)


# Claims

def iso(timestamp):
    return time.strftime(TIME_FORMAT, time.gmtime(timestamp))


def claim_active(claim, now):
    return calendar.timegm(time.strptime(claim["lease_until"], TIME_FORMAT)) > now


@contextlib.contextmanager
def claims_locked(directory, exclusive=True):
    """flock on <directory>/.lock; the kernel releases it if the holder dies."""
    os.makedirs(directory, mode=0o700, exist_ok=True)
    with open(os.path.join(directory, ".lock"), "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH)
        yield


def load_claims(directory):
    try:
        with open(os.path.join(directory, "claims.json"), encoding="utf-8") as handle:
            state = json.load(handle)
    except FileNotFoundError:
        return {"version": 1, "claims": {}}
    except (OSError, ValueError) as error:
        raise ClaimError("cannot read claims.json: %s; not modifying it" % error) from error
    if not isinstance(state, dict) or state.get("version") != 1 or not isinstance(state.get("claims"), dict):
        raise ClaimError("claims.json is not a version 1 claims file; not modifying it")
    return state


def save_claims(directory, state):
    descriptor, temporary = tempfile.mkstemp(prefix=".claims.json.", dir=directory)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(state, handle, indent=2, sort_keys=True)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, os.path.join(directory, "claims.json"))
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temporary)
        raise


def read_claims(directory, repo):
    """Claims of `repo` by issue number, read under a shared lock."""
    if not os.path.isdir(directory):
        return {}
    with claims_locked(directory, exclusive=False):
        claims = load_claims(directory)["claims"].values()
    return {c["issue"]: c for c in claims if c["repo"] == repo}


def put_claim(state, repo, number, agent, hours, note, now):
    key = "%s#%d" % (repo, number)
    current = state["claims"].get(key)
    if current and current["agent"] != agent and claim_active(current, now):
        raise ClaimError("#%d is claimed by %s until %s" % (number, current["agent"], current["lease_until"]))
    renewing = current and current["agent"] == agent
    state["claims"][key] = {"repo": repo, "issue": number, "agent": agent,
                            "claimed_at": current["claimed_at"] if renewing else iso(now),
                            "lease_until": iso(now + hours * 3600),
                            "note": note or (current.get("note", "") if renewing else "")}
    return state["claims"][key]


def claim(directory, repo, number, agent, hours=24, note="", now=None):
    with claims_locked(directory):
        state = load_claims(directory)
        new = put_claim(state, repo, number, agent, hours, note, time.time() if now is None else now)
        save_claims(directory, state)
        return new


def release(directory, repo, number, agent, force=False, now=None):
    with claims_locked(directory):
        state = load_claims(directory)
        current = state["claims"].get("%s#%d" % (repo, number))
        if not current:
            raise ClaimError("#%d is not claimed" % number)
        expired = not claim_active(current, time.time() if now is None else now)
        if current["agent"] != agent and not (force and expired):
            raise ClaimError("#%d is claimed by %s; only the holder releases it (or --force once it expires)"
                             % (number, current["agent"]))
        del state["claims"]["%s#%d" % (repo, number)]
        save_claims(directory, state)
        return current


def acquire_next(directory, repo, agent, candidates, hours=24, note="", now=None):
    """Claim the first candidate without an active claim; the ranking happened before the lock."""
    with claims_locked(directory):
        state = load_claims(directory)
        now = time.time() if now is None else now
        for number in candidates:
            current = state["claims"].get("%s#%d" % (repo, number))
            if current and claim_active(current, now):
                continue
            new = put_claim(state, repo, number, agent, hours, note, now)
            save_claims(directory, state)
            return new
    return None


# Command line

def agent_name(text):
    if not AGENT_RE.match(text):
        raise argparse.ArgumentTypeError("1-64 characters starting with a letter or digit, e.g. claude or codex")
    return text


def lease_hours(text):
    hours = float(text)
    if not 0 < hours <= 14 * 24:
        raise argparse.ArgumentTypeError("between 0 and 336 hours")
    return hours


def rank(args, graphql, strict=False):
    """Fetch and rank; `strict` (acquire-next) ignores the --include-* options."""
    repository, issues = fetch(args.repo, graphql)
    texts = [(repository.get(name) or {}).get("text") for name in ("todoMap", "todoSnapshot")]
    return analyse(repository, issues, *texts, read_claims(args.claims_dir, args.repo),
                   include_in_progress=args.include_in_progress and not strict,
                   include_assigned=args.include_assigned and not strict)


def main(argv=None, graphql=gh_graphql):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", default=DEFAULT_REPO)
    parser.add_argument("--claims-dir", default=CLAIMS_DIR)
    parser.add_argument("--all", action="store_true", help="also list ineligible issues with reasons")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--limit", type=int, default=15, help="rows to show; 0 = all (default 15)")
    parser.add_argument("--label", action="append", default=[], help="only issues with this label (repeatable)")
    parser.add_argument("--include-in-progress", action="store_true")
    parser.add_argument("--include-assigned", action="store_true")
    commands = parser.add_subparsers(dest="command")
    for name in ("claim", "release", "acquire-next", "claims"):
        command = commands.add_parser(name)
        if name in ("claim", "release"):
            command.add_argument("number", type=int)
        if name != "claims":
            command.add_argument("--agent", type=agent_name, required=True)
        if name in ("claim", "acquire-next"):
            command.add_argument("--hours", type=lease_hours, default=24.0, help="lease (default 24)")
            command.add_argument("--note", default="")
        if name == "release":
            command.add_argument("--force", action="store_true", help="release another agent's expired claim")
        if name == "acquire-next":
            command.add_argument("--label", action="append", default=[])
    args = parser.parse_args(argv)
    try:
        if args.command == "claims":
            for c in sorted(read_claims(args.claims_dir, args.repo).values(), key=lambda c: c["issue"]):
                print("#%d %s until %s%s %s" % (c["issue"], c["agent"], c["lease_until"],
                                               "" if claim_active(c, time.time()) else " (expired)", c["note"]))
        elif args.command == "claim":
            new = claim(args.claims_dir, args.repo, args.number, args.agent, args.hours, args.note)
            print("claimed #%d for %s until %s" % (args.number, args.agent, new["lease_until"]))
        elif args.command == "release":
            release(args.claims_dir, args.repo, args.number, args.agent, args.force)
            print("released #%d" % args.number)
        elif args.command == "acquire-next":
            candidates = [r["number"] for r in select(rank(args, graphql, strict=True), labels=args.label)]
            new = acquire_next(args.claims_dir, args.repo, args.agent, candidates, args.hours, args.note)
            if new is None:
                print("next-issues: no eligible unclaimed issue", file=sys.stderr)
                return 1
            print("claimed #%d for %s until %s" % (new["issue"], args.agent, new["lease_until"]))
        else:
            result = rank(args, graphql)
            rows = select(result, args.all, args.label, args.limit)
            print(json.dumps(dict(result, issues=rows), indent=2) if args.json else render(result, rows, args.all))
    except ClaimError as error:
        print("next-issues: %s" % error, file=sys.stderr)
        return 1
    except FetchError as error:
        print("next-issues: %s" % error, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
