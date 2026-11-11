// SPDX-License-Identifier: EUPL-1.2
// The share-link landing page (docs/web.md, "Share links"):
//   https://<web host>/join/<token>#s=<host>[:<port>]&c=<channel path>&fp=sha256:<hex>&policy=nuschel&n=<name>
// The token (empty for links whose target is all in the fragment) is resolved by
// a Nuschel server after connecting. The fragment never leaves the browser.
// Mirrors nuschel_core::share::ShareLink; nothing here is sent or stored except
// one local preference (whether to open the app at once next time).
"use strict";

(function () {
  const MUMBLE_PORT = 64738;
  const PREFERENCE = "nuschel.open-app";

  function parseServer(text) {
    let host = text;
    let port = MUMBLE_PORT;
    const bracketed = /^\[([0-9A-Fa-f:.]+)\](?::(\d{1,5}))?$/.exec(text);
    if (bracketed) {
      host = bracketed[1];
      if (bracketed[2]) port = Number(bracketed[2]);
    } else {
      const plain = /^([A-Za-z0-9._-]{1,253})(?::(\d{1,5}))?$/.exec(text);
      if (!plain) return null;
      host = plain[1];
      if (plain[2]) port = Number(plain[2]);
    }
    if (!(port >= 1 && port <= 65535)) return null;
    return { host: host.toLowerCase(), port: port };
  }

  function parse() {
    const path = /^\/join\/([A-Za-z0-9_-]{16,64})?\/?$/.exec(location.pathname);
    if (!path) return null;
    const params = new URLSearchParams(location.hash.replace(/^#/, ""));
    const server = parseServer(params.get("s") || "");
    if (!server) return null;
    const channel = (params.get("c") || "").split("/").filter(function (part) {
      return part.length > 0;
    });
    let pin = null;
    const fp = params.get("fp");
    if (fp !== null) {
      const match = /^sha256:([0-9A-Fa-f]{64})$/.exec(fp);
      if (!match) return null;
      pin = match[1].toLowerCase();
    }
    const policy = (params.get("policy") || "").split("+").indexOf("nuschel") >= 0;
    const name = (params.get("n") || "").slice(0, 100);
    return {
      token: path[1] || "",
      host: server.host,
      port: server.port,
      channel: channel.slice(0, 64),
      pin: pin,
      policy: policy,
      name: name,
    };
  }

  // The same fragment, written again from what was understood.
  function fragment(link) {
    const host = link.host.indexOf(":") >= 0 ? "[" + link.host + "]" : link.host;
    const parts = ["s=" + encodeURIComponent(link.port === MUMBLE_PORT ? host : host + ":" + link.port)];
    if (link.channel.length) {
      parts.push("c=" + link.channel.map(encodeURIComponent).join("/"));
    }
    if (link.pin) parts.push("fp=sha256:" + link.pin);
    if (link.policy) parts.push("policy=nuschel");
    if (link.name) parts.push("n=" + encodeURIComponent(link.name));
    return parts.join("&");
  }

  function remember(open) {
    try {
      if (open) localStorage.setItem(PREFERENCE, "1");
      else localStorage.removeItem(PREFERENCE);
    } catch (_) {
      // Storage may be off; then the page simply asks every time.
    }
  }

  function remembered() {
    try {
      return localStorage.getItem(PREFERENCE) === "1";
    } catch (_) {
      return false;
    }
  }

  function show() {
    const where = document.getElementById("where");
    const status = document.getElementById("status");
    const link = parse();
    if (!link) {
      document.getElementById("title").textContent = "This link is not complete";
      where.textContent = "Ask for the link again, and copy all of it.";
      return;
    }
    const address = link.port === MUMBLE_PORT ? link.host : link.host + ":" + link.port;
    const server = link.name || address;
    if (link.channel.length) {
      document.getElementById("title").textContent = "Join " + link.channel[link.channel.length - 1];
      where.textContent = "on " + server;
    } else {
      document.getElementById("title").textContent = "Join " + server;
      where.textContent = link.token ? "You were invited to a channel." : "";
    }
    const app = "nuschel://join/" + link.token + "#" + fragment(link);
    const openApp = document.getElementById("open-app");
    openApp.href = app;
    openApp.addEventListener("click", function () {
      remember(true);
      status.textContent = "Opening Nuschel… If nothing happens, get the app below or join in the browser.";
    });
    const web = "t=" + link.token + "&" + fragment(link);
    document.getElementById("open-web").href = "/#" + web;
    document.getElementById("actions").hidden = false;
    document.getElementById("details").hidden = false;
    document.getElementById("detail-server").textContent = address;
    if (link.pin) document.getElementById("detail-pin").textContent = link.pin;
    else document.getElementById("detail-pin-row").hidden = true;
    document.getElementById("detail-policy").hidden = !link.policy;
    if (remembered()) {
      status.textContent = "Opening Nuschel…";
      location.href = app;
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", show);
  } else {
    show();
  }
})();
