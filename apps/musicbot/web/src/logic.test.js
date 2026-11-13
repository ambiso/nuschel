// SPDX-License-Identifier: EUPL-1.2
import { test, mock } from "node:test";
import assert from "node:assert/strict";
import {
  formatDuration, totalDuration, interpolatePosition, pointerFraction, seekKeyTarget, seekSettled,
  gapIndex, moveTarget, applyMove, stepTarget, parseRoute, buildRoute, routeForLibrary, debounce,
  queueStorageKey, selectedQueue, encodeDragItem, decodeDragItem, handlesSpace,
  mergeHistory, libraryForNamespace, routeForNamespace, browseItemIsCurrent, similarityPercent, sonicPathStart,
} from "./logic.js";

test("formatDuration", () => {
  assert.equal(formatDuration(0), "0:00");
  assert.equal(formatDuration(7.9), "0:07");
  assert.equal(formatDuration(187), "3:07");
  assert.equal(formatDuration(3599), "59:59");
  assert.equal(formatDuration(3600), "1:00:00");
  assert.equal(formatDuration(3723), "1:02:03");
  assert.equal(formatDuration(-5), "0:00");
  assert.equal(formatDuration(null), "");
  assert.equal(formatDuration(NaN), "");
  assert.equal(formatDuration(Infinity), "");
});

test("totalDuration skips unknown durations", () => {
  assert.equal(totalDuration([{ duration: 10 }, { duration: null }, { duration: 5 }]), 15);
  assert.equal(totalDuration([]), 0);
});

test("interpolatePosition advances, caps and never goes negative", () => {
  assert.equal(interpolatePosition(1000, true, 500, 2500, 60), 3000);
  assert.equal(interpolatePosition(1000, false, 500, 2500, 60), 1000);
  assert.equal(interpolatePosition(59000, true, 0, 5000, 60), 60000);
  assert.equal(interpolatePosition(1000, true, 0, 5000, null), 6000);
  // A clock that runs backwards relative to the snapshot does not rewind.
  assert.equal(interpolatePosition(1000, true, 5000, 4000, 60), 1000);
  assert.equal(interpolatePosition(-20, false, 0, 0, 60), 0);
});

test("pointerFraction clamps to the bar", () => {
  assert.equal(pointerFraction(50, 0, 200), 0.25);
  assert.equal(pointerFraction(-10, 0, 200), 0);
  assert.equal(pointerFraction(300, 0, 200), 1);
  assert.equal(pointerFraction(10, 0, 0), 0);
});

test("seekKeyTarget", () => {
  assert.equal(seekKeyTarget("ArrowRight", 10000, 60000), 15000);
  assert.equal(seekKeyTarget("ArrowLeft", 3000, 60000), 0);
  assert.equal(seekKeyTarget("ArrowRight", 58000, 60000), 60000);
  assert.equal(seekKeyTarget("Home", 30000, 60000), 0);
  assert.equal(seekKeyTarget("End", 30000, 60000), 60000);
  assert.equal(seekKeyTarget("PageUp", 0, 60000), 30000);
  assert.equal(seekKeyTarget("a", 0, 60000), null);
});

test("seekSettled", () => {
  const current = { entry: { id: 4 } };
  assert.equal(seekSettled(30000, 4, current, 10000), false);
  assert.equal(seekSettled(30000, 4, current, 30400), true);
  assert.equal(seekSettled(30000, 4, { entry: { id: 5 } }, 0), true);
  assert.equal(seekSettled(30000, 4, null, 0), true);
});

test("gapIndex picks the half of the row", () => {
  assert.equal(gapIndex(3, 0, 40), 3);
  assert.equal(gapIndex(3, 19.9, 40), 3);
  assert.equal(gapIndex(3, 20, 40), 4);
  assert.equal(gapIndex(3, 40, 40), 4);
});

test("moveTarget counts the index without the dragged entry", () => {
  // [a b c d e], dragging b (index 1)
  assert.equal(moveTarget(1, 0, 5), 0); // before a
  assert.equal(moveTarget(1, 1, 5), null); // just before itself
  assert.equal(moveTarget(1, 2, 5), null); // just after itself
  assert.equal(moveTarget(1, 3, 5), 2); // between c and d: [a c b d e]
  assert.equal(moveTarget(1, 5, 5), 4); // after e
  assert.equal(moveTarget(1, 99, 5), 4); // clamped
  assert.equal(moveTarget(1, -3, 5), 0); // clamped
  assert.equal(moveTarget(4, 0, 5), 0);
  assert.equal(moveTarget(0, 1, 1), null);
  assert.equal(moveTarget(5, 0, 5), null); // not in the list
  const list = ["a", "b", "c", "d", "e"];
  assert.deepEqual(applyMove(list, 1, moveTarget(1, 3, 5)), ["a", "c", "b", "d", "e"]);
  assert.deepEqual(applyMove(list, 1, moveTarget(1, 5, 5)), ["a", "c", "d", "e", "b"]);
  assert.deepEqual(applyMove(list, 3, moveTarget(3, 0, 5)), ["d", "a", "b", "c", "e"]);
});

test("applyMove clamps and leaves the input alone", () => {
  const list = ["a", "b", "c"];
  assert.deepEqual(applyMove(list, 0, 10), ["b", "c", "a"]);
  assert.deepEqual(applyMove(list, 2, -1), ["c", "a", "b"]);
  assert.deepEqual(applyMove(list, 7, 0), ["a", "b", "c"]);
  assert.deepEqual(list, ["a", "b", "c"]);
});

test("stepTarget stops at the ends", () => {
  assert.equal(stepTarget(0, -1, 3), null);
  assert.equal(stepTarget(0, 1, 3), 1);
  assert.equal(stepTarget(2, 1, 3), null);
  assert.deepEqual(applyMove(["a", "b", "c"], 0, stepTarget(0, 1, 3)), ["b", "a", "c"]);
  assert.deepEqual(applyMove(["a", "b", "c"], 2, stepTarget(2, -1, 3)), ["a", "c", "b"]);
});

test("routes round trip, including awkward ids", () => {
  const ids = ["plain", "a/b", "what?", "#hash", "with space", "50% & more", "ü/€?#=&+"];
  for (const view of ["album", "artist", "playlist"]) {
    for (const id of ids) {
      const route = { view, library: 2, id };
      const hash = buildRoute(route);
      assert.equal(hash.indexOf("#", 1), -1, hash);
      assert.equal(hash.split("/").length, 4, hash);
      assert.deepEqual(parseRoute(hash), route);
    }
  }
  for (const id of ids) {
    const similar = { view: "similar", library: 2, id, namespace: "page:0" };
    const hash = buildRoute(similar);
    assert.equal(hash.indexOf("#", 1), -1, hash);
    assert.equal(hash.split("?")[0].split("/").length, 4, hash);
    assert.deepEqual(parseRoute(hash), similar);
    for (const to of ids) {
      const path = { view: "path", library: 1, from: id, to, namespace: "page:0" };
      const pathHash = buildRoute(path);
      assert.equal(pathHash.indexOf("#", 1), -1, pathHash);
      assert.equal(pathHash.split("?")[0].split("/").length, 5, pathHash);
      assert.deepEqual(parseRoute(pathHash), path);
    }
  }
  for (const route of [
    { view: "queue" },
    { view: "channels" },
    { view: "history" },
    { view: "albums", library: 1, list: "starred" },
    { view: "artists", library: 0 },
    { view: "playlists", library: 3 },
    { view: "search", q: "a b/c?d#e&f=g+h" },
    { view: "search", q: "" },
  ]) assert.deepEqual(parseRoute(buildRoute(route)), route);
});

test("parseRoute falls back on bad input", () => {
  assert.deepEqual(parseRoute(""), { view: "queue" });
  assert.deepEqual(parseRoute("#"), { view: "queue" });
  assert.deepEqual(parseRoute("#/nope"), { view: "queue" });
  assert.deepEqual(parseRoute("#/album/0"), { view: "queue" });
  assert.deepEqual(parseRoute("#/album/0/%E0%A4%A"), { view: "queue" });
  assert.deepEqual(parseRoute("#/albums"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(parseRoute("#/albums?library=-1&list=bogus"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(parseRoute("#/artists?library=x"), { view: "artists", library: 0 });
  assert.deepEqual(parseRoute("#/album/zz/x"), { view: "album", library: 0, id: "x" });
  assert.deepEqual(parseRoute("#/similar/0"), { view: "queue" });
  assert.deepEqual(parseRoute("#/similar/0/a/b"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a/b/c"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a/%E0%A4%A"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/-3/a/b"), { view: "path", library: 0, from: "a", to: "b" });
});

test("routeForLibrary goes to the matching list view", () => {
  assert.deepEqual(routeForLibrary({ view: "albums", library: 0, list: "random" }, 1), { view: "albums", library: 1, list: "random" });
  assert.deepEqual(routeForLibrary({ view: "album", library: 0, id: "x" }, 1), { view: "albums", library: 1, list: "newest" });
  assert.deepEqual(routeForLibrary({ view: "artist", library: 0, id: "x" }, 1), { view: "artists", library: 1 });
  assert.deepEqual(routeForLibrary({ view: "playlist", library: 0, id: "x" }, 2), { view: "playlists", library: 2 });
  assert.deepEqual(routeForLibrary({ view: "queue" }, 1), { view: "queue" });
  assert.deepEqual(routeForLibrary({ view: "similar", library: 0, id: "x", namespace: "n" }, 1), { view: "queue" });
  assert.deepEqual(routeForLibrary({ view: "path", library: 0, from: "a", to: "b", namespace: "n" }, 0), { view: "queue" });
});

test("detail routes never carry old IDs into a restarted or remapped namespace", () => {
  for (const [view, safeView] of [["album", "albums"], ["artist", "artists"], ["playlist", "playlists"]]) {
    const original = { view, library: 1, id: "old/id?", namespace: "page-a:0" };
    const route = parseRoute(buildRoute(original));
    assert.deepEqual(routeForNamespace(route, 1, "page-a:0"), original);
    const safe = { view: safeView, library: 1, ...(view === "album" ? { list: "newest" } : {}) };
    assert.deepEqual(routeForNamespace(route, 1, null), safe);
    assert.deepEqual(routeForNamespace(route, 1, "page-a:1"), safe);
    assert.deepEqual(routeForNamespace(route, 1, "page-b:0"), safe);
    assert.deepEqual(routeForNamespace(parseRoute(buildRoute({ view, library: 1, id: original.id })), 1, "page-a:0"), safe);
    assert.deepEqual(routeForNamespace(route, 0, "page-a:0"), { ...safe, library: 0 });
  }
});

test("sonic routes never carry old song IDs into a restarted or remapped namespace", () => {
  for (const original of [
    { view: "similar", library: 1, id: "old/id?", namespace: "page-a:0" },
    { view: "path", library: 1, from: "a/b", to: "c?d", namespace: "page-a:0" },
  ]) {
    const route = parseRoute(buildRoute(original));
    assert.deepEqual(routeForNamespace(route, 1, "page-a:0"), original);
    assert.deepEqual(routeForNamespace(route, 1, null), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 1, "page-a:1"), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 1, "page-b:0"), { view: "queue" });
    const { namespace, ...unproved } = original;
    assert.deepEqual(routeForNamespace(parseRoute(buildRoute(unproved)), 1, namespace), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 0, "page-a:0"), { view: "queue" });
  }
});

test("similarity percent hides unknown scores", () => {
  assert.equal(similarityPercent(0.8), 80);
  assert.equal(similarityPercent(0.456), 46);
  assert.equal(similarityPercent(1), 100);
  assert.equal(similarityPercent(0), 0);
  assert.equal(similarityPercent(-1), null);
  assert.equal(similarityPercent(1.2), null);
  assert.equal(similarityPercent(null), null);
  assert.equal(similarityPercent(undefined), null);
  assert.equal(similarityPercent(NaN), null);
});

test("sonic path starts only from another song in the same library", () => {
  assert.equal(sonicPathStart({ library: 1, id: "now" }, { library: 1, id: "there" }), "now");
  assert.equal(sonicPathStart({ library: 1, id: "now" }, { library: 1, id: "now" }), null);
  assert.equal(sonicPathStart({ library: 0, id: "now" }, { library: 1, id: "there" }), null);
  assert.equal(sonicPathStart(null, { library: 1, id: "there" }), null);
  assert.equal(sonicPathStart({ library: 1, id: "" }, { library: 1, id: "there" }), null);
});

test("library reordering follows only an unambiguous old name and drops detail IDs", () => {
  const previous = ["Office", "Home"];
  const current = ["Home", "Office"];
  const library = libraryForNamespace(previous, current, 1);
  assert.equal(library, 0);
  assert.deepEqual(routeForNamespace({ view: "playlist", library: 1, id: "old", namespace: "before" }, library, "after"), { view: "playlists", library: 0 });
  assert.deepEqual(routeForNamespace({ view: "albums", library: 1, list: "starred" }, library, "after"), { view: "albums", library: 0, list: "starred" });
  assert.equal(libraryForNamespace(["Office", "Home", "Home"], ["Office", "Home"], 2), 0);
  assert.equal(libraryForNamespace(previous, ["Office", "Home", "Home"], 1), 0);
  assert.equal(libraryForNamespace(previous, ["Office", "Replacement"], 1), 0);
  assert.equal(libraryForNamespace([], current, 1), 0);
});

test("Add and dragged items lose authority on disconnect and same-name restart", () => {
  const old = decodeDragItem(encodeDragItem("song", 1, "shared-id", "page-a:0"));
  assert.equal(browseItemIsCurrent(old, "page-a:0", 2), true);
  assert.equal(browseItemIsCurrent(old, null, 2), false);
  assert.equal(browseItemIsCurrent(old, "page-a:1", 2), false);
  assert.equal(browseItemIsCurrent(old, "page-b:0", 2), false);
  assert.equal(browseItemIsCurrent({ kind: "song", library: 1, id: "shared-id" }, "page-a:1", 2), false);
  const refreshed = decodeDragItem(encodeDragItem("song", 1, "shared-id", "page-a:1"));
  assert.equal(browseItemIsCurrent(refreshed, "page-a:1", 2), true);
  assert.equal(browseItemIsCurrent(refreshed, "page-a:1", 1), false);
  assert.equal(browseItemIsCurrent({ ...refreshed, library: -1 }, "page-a:1", 2), false);
  assert.equal(browseItemIsCurrent({ ...refreshed, library: undefined }, "page-a:1", 2), false);
});

test("debounce calls once with the last arguments", () => {
  mock.timers.enable({ apis: ["setTimeout"] });
  try {
    const calls = [];
    const fn = debounce((...args) => calls.push(args), 150);
    fn(1);
    fn(2);
    mock.timers.tick(149);
    assert.deepEqual(calls, []);
    fn(3, "x");
    mock.timers.tick(149);
    assert.deepEqual(calls, []);
    mock.timers.tick(1);
    assert.deepEqual(calls, [[3, "x"]]);
    fn(4);
    assert.equal(fn.pending(), true);
    fn.flush();
    assert.deepEqual(calls, [[3, "x"], [4]]);
    fn.flush();
    fn(5);
    fn.cancel();
    mock.timers.tick(500);
    assert.deepEqual(calls, [[3, "x"], [4]]);
    assert.equal(fn.pending(), false);
  } finally {
    mock.timers.reset();
  }
});

test("queue selection storage", () => {
  assert.notEqual(queueStorageKey("/u/abc/"), queueStorageKey("/u/def/"));
  assert.equal(selectedQueue(null, 3), 0);
  assert.equal(selectedQueue("2", 3), 2);
  assert.equal(selectedQueue("5", 3), 2);
  assert.equal(selectedQueue("-1", 3), 0);
  assert.equal(selectedQueue("1.5", 3), 0);
  assert.equal(selectedQueue("1", 0), 0);
});

test("drag items round trip and reject junk", () => {
  assert.deepEqual(decodeDragItem(encodeDragItem("album", 1, "a/b?c", "page:0")), { kind: "album", library: 1, id: "a/b?c", namespace: "page:0" });
  assert.equal(decodeDragItem("not json"), null);
  assert.equal(decodeDragItem(encodeDragItem("artist", 0, "x", "page:0")), null);
  assert.equal(decodeDragItem(encodeDragItem("song", -1, "x", "page:0")), null);
  assert.equal(decodeDragItem(encodeDragItem("song", 0, "", "page:0")), null);
  assert.equal(decodeDragItem(JSON.stringify({ kind: "song", library: 0, id: "old" })), null);
  assert.equal(decodeDragItem(encodeDragItem("song", 0, "x", "")), null);
  assert.equal(decodeDragItem("null"), null);
});

test("Space playback shortcut does not steal control keystrokes", () => {
  assert.equal(handlesSpace(null), false);
  assert.equal(handlesSpace({}), false);
  assert.equal(handlesSpace({ closest: () => null }), false);
  for (const control of ["input", "textarea", "select", "button", "a[href]", "[contenteditable='true']", "[role='slider']"]) {
    const target = { closest: (selector) => selector.split(", ").includes(control) ? target : null };
    assert.equal(handlesSpace(target), true, control);
  }
});

test("history refresh retains older pages, replaces duplicates and prunes expired plays", () => {
  const current = { items: [{ id: 6 }, { id: 5 }, { id: 4 }, { id: 3 }], next_before: 3 };
  const page = { items: [{ id: 7 }, { id: 6, updated: true }, { id: 5 }], next_before: 5, oldest_id: 4, revision: 7 };
  assert.deepEqual(mergeHistory(current, page), {
    items: [{ id: 7 }, { id: 6, updated: true }, { id: 5 }, { id: 4 }],
    next_before: null, oldest_id: 4, revision: 7,
  });
});

test("history refresh leaves a cursor for a gap before already loaded older plays", () => {
  const current = { items: [{ id: 3 }, { id: 2 }], next_before: 2 };
  const page = { items: [{ id: 8 }, { id: 7 }], next_before: 7, oldest_id: 1, revision: 8 };
  const merged = mergeHistory(current, page);
  assert.deepEqual(merged.items.map((item) => item.id), [8, 7, 3, 2]);
  assert.equal(merged.next_before, 7);
  const older = mergeHistory(merged, { items: [{ id: 6 }, { id: 5 }, { id: 4 }, { id: 3 }], next_before: 3, oldest_id: 1, revision: 8 }, true);
  assert.deepEqual(older.items.map((item) => item.id), [8, 7, 6, 5, 4, 3, 2]);
  assert.equal(older.next_before, 3);
});

test("having the oldest retained play does not hide a refresh gap", () => {
  const merged = mergeHistory({ items: [{ id: 2 }, { id: 1 }], next_before: null }, {
    items: [{ id: 8 }, { id: 7 }], next_before: 7, oldest_id: 1, revision: 8,
  });
  assert.equal(merged.next_before, 7);
});
