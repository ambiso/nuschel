// SPDX-License-Identifier: EUPL-1.2
// HTML5 drag and drop: library items (songs, albums, playlists) carry JSON
// under DRAG_TYPE; queue entries being reordered carry their id under DRAG_ENTRY_TYPE.
import { DRAG_ENTRY_TYPE, DRAG_TYPE, decodeDragItem, encodeDragItem } from "./logic.js";
import { store } from "./store.svelte.js";

/** dragstart handler for a library item. */
export function startItemDrag(event, kind, library, id, label, namespace) {
  if (!store.browseItemCurrent({ library, namespace })) {
    event.preventDefault();
    return;
  }
  if (!event.dataTransfer) return;
  event.dataTransfer.setData(DRAG_TYPE, encodeDragItem(kind, library, id, namespace));
  if (label) event.dataTransfer.setData("text/plain", label);
  event.dataTransfer.effectAllowed = "copy";
}

/** dragstart handler for a queue entry. */
export function startEntryDrag(event, entryId) {
  if (!event.dataTransfer) return;
  event.dataTransfer.setData(DRAG_ENTRY_TYPE, String(entryId));
  event.dataTransfer.effectAllowed = "move";
}

/** Types are readable during dragover; the data only on drop. */
export const carriesItem = (event) => Boolean(event.dataTransfer?.types.includes(DRAG_TYPE));
export const carriesEntry = (event) => Boolean(event.dataTransfer?.types.includes(DRAG_ENTRY_TYPE));

export function droppedItem(event) {
  return decodeDragItem(event.dataTransfer?.getData(DRAG_TYPE) ?? "");
}

export function droppedEntry(event) {
  const id = Number(event.dataTransfer?.getData(DRAG_ENTRY_TYPE));
  return Number.isInteger(id) ? id : null;
}

/** True when the pointer left `element` for something outside it. */
export function leftElement(event, element) {
  return !event.relatedTarget || !element.contains(event.relatedTarget);
}
