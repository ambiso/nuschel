// SPDX-License-Identifier: EUPL-1.2
// Runs in the SDK worker. One transaction owns each append/page; indexes bind
// records to certificate-scoped SDK identity AND conversation, with stable
// (observation time, row id) ordering even for identical timestamps.
export const DATABASE = "nuschl-observed-history-v1";
const MAX_ROWS = 100;
const MAX_BODY = 256 * 1024;
const utf8 = new TextEncoder();
function request(result) {
  return new Promise((resolve, reject) => {
    result.onsuccess = () => resolve(result.result);
    result.onerror = () => reject(new Error("Browser history request failed."));
  });
}
function transactionDone(transaction) {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = resolve;
    transaction.onerror = transaction.onabort = () => reject(new Error("Browser history transaction failed."));
  });
}
function validMessage(message) {
  return message && typeof message.server === "string" && /^[0-9a-f]{64}$/.test(message.server)
    && typeof message.channel === "string" && message.channel.length <= 512
    && typeof message.author === "string" && message.author.length <= 512
    && Number.isSafeInteger(message.observed_at_ms)
    && typeof message.body === "string" && utf8.encode(message.body).length <= MAX_BODY;
}
export async function openHistory(factory = indexedDB, name = DATABASE) {
  const opening = factory.open(name, 1);
  opening.onupgradeneeded = () => {
    const db = opening.result;
    const rows = db.createObjectStore("messages", { keyPath: "id", autoIncrement: true });
    rows.createIndex("scope_time", ["message.server", "message.channel", "message.observed_at_ms", "id"], { unique: true });
    db.createObjectStore("settings");
  };
  const db = await request(opening);
  db.onversionchange = () => db.close();
  return {
    close: () => db.close(),
    async bookmarks(value) {
      const transaction = db.transaction("settings", value === undefined ? "readonly" : "readwrite");
      const done = transactionDone(transaction);
      const store = transaction.objectStore("settings");
      if (value !== undefined) {
        if (typeof value !== "string" || value.length > 1024 * 1024) throw new Error("Saved server list too large.");
        store.put(value, "bookmarks");
        await done;
      } else {
        const saved = await request(store.get("bookmarks"));
        await done;
        return saved || "[]";
      }
    },
    async execute(operation) {
      if (!operation || !/^[0-9]{1,20}$/.test(operation.ticket)) throw new Error("Invalid history ticket.");
      if (operation.kind === "append") {
        if (!validMessage(operation.message) || typeof operation.outgoing !== "boolean") throw new Error("Invalid history message.");
        const transaction = db.transaction("messages", "readwrite");
        const done = transactionDone(transaction);
        const id = await request(transaction.objectStore("messages").add({ message: operation.message, outgoing: operation.outgoing }));
        await done; // The SDK only displays saved status after commit, never request success alone.
        return { id };
      }
      if (operation.kind !== "page" || !/^[0-9a-f]{64}$/.test(operation.identity)
          || typeof operation.channel !== "string" || operation.channel.length > 512
          || operation.limit !== MAX_ROWS || !(operation.before === null || (Number.isSafeInteger(operation.before) && operation.before > 0))) {
        throw new Error("Invalid history page.");
      }
      const transaction = db.transaction("messages", "readonly");
      const done = transactionDone(transaction);
      const store = transaction.objectStore("messages");
      // Queue the cursor synchronously in the anchor request's success handler;
      // no await/task gap may let IndexedDB auto-commit this transaction.
      const page = await new Promise((resolve, reject) => {
        const begin = (anchor) => {
          if (operation.before !== null && (!anchor || anchor.message.server !== operation.identity || anchor.message.channel !== operation.channel)) {
            reject(new Error("History cursor belongs to another conversation."));
            return;
          }
          const lower = [operation.identity, operation.channel, -Number.MAX_SAFE_INTEGER, 0];
          const upper = anchor
            ? [operation.identity, operation.channel, anchor.message.observed_at_ms, anchor.id]
            : [operation.identity, operation.channel, Number.MAX_SAFE_INTEGER, Number.MAX_SAFE_INTEGER];
          const cursor = store.index("scope_time").openCursor(IDBKeyRange.bound(lower, upper, false, !!anchor), "prev");
          const found = [];
          let bytes = 0;
          cursor.onerror = () => reject(new Error("History page could not be read."));
          cursor.onsuccess = () => {
            const current = cursor.result;
            if (!current) { resolve({ rows: found, more: false }); return; }
            const row = current.value;
            if (!validMessage(row.message) || !Number.isSafeInteger(row.id) || row.id <= 0 || typeof row.outgoing !== "boolean") {
              reject(new Error("Invalid stored history record.")); return;
            }
            const rowBytes = utf8.encode(JSON.stringify(row)).length;
            if (found.length >= MAX_ROWS || (found.length && bytes + rowBytes > 4 * 1024 * 1024)) {
              resolve({ rows: found, more: true }); return;
            }
            bytes += rowBytes;
            found.push(row);
            current.continue();
          };
        };
        if (operation.before === null) begin(null);
        else {
          const anchor = store.get(operation.before);
          anchor.onsuccess = () => begin(anchor.result);
          anchor.onerror = () => reject(new Error("History cursor could not be read."));
        }
      });
      await done;
      const rows = page.rows.reverse();
      return { rows, next_before: page.more ? rows[0].id : null };
    },
  };
}
