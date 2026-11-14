// SPDX-License-Identifier: EUPL-1.2
// The emoji index shared by the picker and message rendering, loaded once from
// this origin (emoji.js). Until it arrives, or if it cannot be loaded, emoji
// stay text and the picker says so.
import { loadEmojiIndex } from "./emoji.js";

export const emoji = $state({ index: null, failed: false });

export function loadEmoji() {
  loadEmojiIndex(import.meta.env.BASE_URL).then(
    (index) => { emoji.index = index; },
    () => { emoji.failed = true; },
  );
}
