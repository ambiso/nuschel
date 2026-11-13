#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Ranking and claim rules of tools/next-issues.py, with canned issues; no network."""
import hashlib
import importlib.util
import json
import multiprocessing
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "next_issues", Path(__file__).resolve().parent.parent / "tools" / "next-issues.py")
ni = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ni)

REPO = "ambiso/nuschl"
REVISION = "a" * 40
TODO = "# TODO\n\n- [ ] alpha\n- [ ] beta\n"
MAP = ("# source_revision=%s\n# source_sha256=%s\n" % (REVISION, hashlib.sha256(TODO.encode()).hexdigest())
       + "source_id\tkind\tstart\tend\tdisposition\tissues\turls\trelated\tfix\n"
       + "todo-aaaaaaaaaaaaaaaa\tcheckbox\t3\t3\tissue-proposal\t12\t-\t-\t-\n")
T0 = 1_800_000_000


def issue(number, labels=(), blocked=(), thumbs=0, body="", assignees=(), author="ambiso"):
    """`blocked` holds open blocker numbers, or (number, state) pairs."""
    blocked = [(b, "OPEN") if isinstance(b, int) else b for b in blocked]
    return {"number": number, "title": "Issue %d" % number, "url": "u", "createdAt": "2026-01-01T00:%02d:00Z" % number,
            "body": body, "author": {"login": author}, "labels": {"nodes": [{"name": n} for n in labels]},
            "assignees": {"nodes": [{"login": a} for a in assignees]}, "reactions": {"totalCount": thumbs},
            "blockedBy": {"totalCount": len(blocked), "nodes": [
                {"number": n, "state": s, "repository": {"nameWithOwner": REPO}} for n, s in blocked]},
            "subIssuesSummary": {"total": 0, "completed": 0}}


def source(line=3, revision=REVISION):
    return ("Source: [TODO.md](https://github.com/%s/blob/%s/TODO.md#L%d-L%d)\n"
            "<!-- nuschl-todo-import: nuschl-0123456789ab -->" % (REPO, revision, line, line))


def rank(issues, todo=TODO, **options):
    result = ni.analyse({"nameWithOwner": REPO}, issues, MAP, "preface\n---\n\n" + todo, now=T0, **options)
    return result, {r["number"]: r for r in result["issues"]}


def eligible(result):
    return [r["number"] for r in ni.select(result)]


class Ranking(unittest.TestCase):
    def test_order_is_priority_then_thumbs_then_age(self):
        result, by = rank([issue(1, ["status:approved", "priority:low", "priority:high"], thumbs=1),
                           issue(2, ["status:approved"], thumbs=9),
                           issue(3, ["source:user-request", "priority:high"], thumbs=3),
                           issue(4, ["status:approved", "priority:high"], thumbs=1),
                           issue(5, ["status:approved", "priority:urgent"])])
        self.assertEqual(eligible(result), [5, 3, 1, 4, 2])
        self.assertEqual((by[1]["priority"], by[2]["priority"]), ("high", "medium"))

    def test_todo_marker_must_match_the_map(self):
        issues = [issue(12, body=source()), issue(13, body=source()), issue(14, body=source(revision="b" * 40))]
        result, by = rank(issues)
        self.assertEqual(by[12]["approval"], ["todo-import"])
        self.assertEqual([by[13]["todo_marker"], by[14]["todo_marker"]], ["unverified", "unverified"])
        self.assertEqual(eligible(result), [12])
        result, by = rank([issue(12, body=source(), author="someone-else")])
        self.assertEqual((by[12]["todo_marker"], eligible(result)), ("unverified", []))
        result, by = rank(issues, todo=TODO.replace("alpha", "ALPHA"))
        self.assertEqual(eligible(result), [])
        self.assertTrue(result["todo_map_problem"])

    def test_exclusions(self):
        result, by = rank([issue(1, ["status:approved"], assignees=["someone"]),
                           issue(2, ["status:approved", "status:in-progress"]),
                           issue(3, ["status:approved", "status:deferred"]),
                           issue(4, ["status:needs-approval"]),
                           issue(5, ["status:needs-approval", "status:approved"]),
                           issue(6, ["status:approved"], blocked=[(99, "CLOSED")]),
                           issue(7, ["status:approved"])],
                          claims={7: {"agent": "codex", "lease_until": ni.iso(T0 + 60)}})
        self.assertEqual(eligible(result), [5, 6])
        self.assertEqual(by[1]["reasons"], ["assigned: someone"])
        self.assertEqual(by[4]["reasons"], ["no approval", "hold: needs-approval"])
        self.assertTrue(by[7]["reasons"][0].startswith("claimed by codex"))
        result, _ = rank([issue(1, ["status:approved"], assignees=["someone"])], include_assigned=True)
        self.assertEqual(eligible(result), [1])

    def test_blockers_inherit_priority_but_never_approval(self):
        result, by = rank([issue(1, ["status:approved", "priority:urgent"], blocked=[2]),
                           issue(2, blocked=[3]),                                   # unapproved middle link
                           issue(3, ["source:user-request"]),
                           issue(4, ["status:approved", "priority:high"], blocked=[5]),
                           issue(5, ["status:needs-approval"], blocked=[6]),          # held
                           issue(6, blocked=[7]), issue(7, blocked=[6]),             # cycle
                           issue(8, ["status:approved", "status:deferred", "priority:urgent"], blocked=[9]),
                           issue(9)])                                                # behind held work only
        self.assertEqual((by[2]["effective_priority"], by[2]["priority_via"], by[2]["approval"]), ("urgent", [1], []))
        self.assertFalse(by[2]["eligible"])
        self.assertEqual((by[3]["effective_priority"], by[3]["priority_via"]), ("urgent", [1, 2]))
        self.assertEqual(eligible(result), [3])
        self.assertEqual((by[9]["effective_priority"], by[9]["priority_via"]), ("medium", []))
        self.assertEqual([(a["number"], a["need"], a["blocks"]) for a in result["attention"]],
                         [(2, "needs approval", [1]), (5, "held: needs-approval", [4]),
                          (6, "needs approval", [4]), (7, "needs approval", [4])])
        self.assertEqual(sorted(set(result["cycle"])), [6, 7])
        self.assertIn("#1 urgent waits for #2 needs approval, #3 Q1", ni.render(result, ni.select(result), False))


def race(directory, barrier, results, agent):
    barrier.wait()
    try:
        ni.claim(directory, REPO, 7, agent)
        results.put(agent)
    except ni.ClaimError:
        results.put(None)


class Claims(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="next-issues-test-")
        self.addCleanup(scratch.cleanup)
        self.directory = scratch.name

    def test_claim_renew_takeover_release(self):
        ni.claim(self.directory, REPO, 7, "codex", hours=1, note="first", now=T0)
        with self.assertRaises(ni.ClaimError):
            ni.claim(self.directory, REPO, 7, "claude", now=T0 + 60)
        renewed = ni.claim(self.directory, REPO, 7, "codex", hours=2, now=T0 + 60)
        self.assertEqual((renewed["claimed_at"], renewed["note"]), (ni.iso(T0), "first"))
        with self.assertRaises(ni.ClaimError):
            ni.release(self.directory, REPO, 7, "claude", force=True, now=T0 + 120)
        taken = ni.claim(self.directory, REPO, 7, "claude", now=T0 + 3 * 3600)
        self.assertEqual(taken["agent"], "claude")
        ni.release(self.directory, REPO, 7, "claude")
        self.assertEqual(ni.read_claims(self.directory, REPO), {})

    def test_existing_store_is_kept(self):
        existing = {"version": 1, "claims": {"ambiso/nuschl#11": {
            "agent": "codex", "claimed_at": ni.iso(T0), "issue": 11, "lease_until": ni.iso(T0 + 86400),
            "note": "link previews", "repo": REPO}}}
        with open(os.path.join(self.directory, "claims.json"), "w", encoding="utf-8") as handle:
            json.dump(existing, handle)
        with self.assertRaises(ni.ClaimError):
            ni.claim(self.directory, REPO, 11, "claude", now=T0 + 60)
        self.assertEqual(ni.acquire_next(self.directory, REPO, "claude", [11, 12], now=T0 + 60)["issue"], 12)
        self.assertIsNone(ni.acquire_next(self.directory, REPO, "claude", [11, 12], now=T0 + 60))
        self.assertEqual(ni.read_claims(self.directory, REPO)[11], existing["claims"]["ambiso/nuschl#11"])

    def test_corrupt_store_is_not_overwritten(self):
        with open(os.path.join(self.directory, "claims.json"), "w", encoding="utf-8") as handle:
            handle.write("{")
        with self.assertRaises(ni.ClaimError):
            ni.claim(self.directory, REPO, 7, "claude")

    def test_two_processes_one_winner(self):
        context = multiprocessing.get_context("fork")
        barrier, results = context.Barrier(2), context.Queue()
        processes = [context.Process(target=race, args=(self.directory, barrier, results, agent))
                     for agent in ("claude", "codex")]
        for process in processes:
            process.start()
        winners = [results.get(timeout=30) for _ in processes]
        for process in processes:
            process.join(30)
        self.assertEqual(sorted(w is None for w in winners), [False, True])
        self.assertEqual(ni.read_claims(self.directory, REPO)[7]["agent"], next(w for w in winners if w))


if __name__ == "__main__":
    unittest.main()
