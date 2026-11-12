// SPDX-License-Identifier: EUPL-1.2
// Build-only evidence from modules actually emitted by Vite/Rolldown, including
// worker chunks. No npm install, network, dynamic source evaluation or SPDX text
// substitution. Missing owners/actual license files stop the build.
import fs from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const cargoDirectories = new Map([
  ["src/sdk/", "nuschl-client-wasm"],
  ["src/audio-codec/", "nuschl-audio-wasm"],
  ["src/content-codec/", "nuschl-content-wasm"],
]);
const slash = (value) => value.split(path.sep).join("/");

export function bundledRuntimeNotices() {
  let root;
  let lock;
  const packages = new Map();
  const cargo = new Set();
  const chunks = [];
  function configResolved(config) {
    root ??= path.resolve(config.root);
    if (root !== path.resolve(config.root)) throw new Error("notice inventory worker root changed");
    lock ??= JSON.parse(fs.readFileSync(path.join(root, "package-lock.json"), "utf8"));
  }
  function owner(file) {
    for (let directory = path.dirname(file); directory !== path.dirname(directory); directory = path.dirname(directory)) {
      const candidate = path.join(directory, "package.json");
      if (!fs.existsSync(candidate)) continue;
      const info = JSON.parse(fs.readFileSync(candidate, "utf8"));
      if (typeof info.name === "string" && typeof info.version === "string") return { directory, info };
    }
    throw new Error("emitted third-party module has no package owner");
  }
  function namedOwner(name) {
    const require = createRequire(path.join(root, "vite.config.js"));
    const found = owner(require.resolve(name));
    if (found.info.name !== name) throw new Error("virtual runtime owner does not match its fixed package");
    return found;
  }
  function recordPackage(found, module, phase) {
    const { directory, info } = found;
    if (!/^(?:@[a-z0-9._-]+\/)?[a-z0-9._-]+$/i.test(info.name) || !/^[a-z0-9.+_-]+$/i.test(info.version)) {
      throw new Error("invalid emitted npm package identity");
    }
    const identity = `${info.name}@${info.version}`;
    let entry = packages.get(identity);
    if (!entry) {
      if (packages.size >= 128) throw new Error("too many emitted npm owners");
      const locks = Object.entries(lock.packages ?? {}).filter(([key, item]) =>
        (key === `node_modules/${info.name}` || key.endsWith(`/node_modules/${info.name}`)) && item.version === info.version,
      ).map(([, item]) => item);
      if (!locks.length || locks.some((item) => item.resolved !== locks[0].resolved || item.integrity !== locks[0].integrity)) {
        throw new Error("emitted npm owner is absent or ambiguous in the pinned lock");
      }
      const source = locks[0];
      const url = typeof source.resolved === "string" ? new URL(source.resolved) : null;
      if (!url || url.protocol !== "https:" || url.username || url.password || url.hash ||
          typeof source.integrity !== "string" || !/^sha(?:256|384|512)-[A-Za-z0-9+/=]+$/.test(source.integrity)) {
        throw new Error("emitted npm owner lacks pinned source identity");
      }
      const notices = fs.readdirSync(directory).filter((name) => /^(?:licen[cs]e|copying|notice|third-party-licen[cs]e)(?:[._-].*|$)/i.test(name)).sort();
      const files = [];
      for (const name of notices) {
        const file = path.join(directory, name);
        const stat = fs.lstatSync(file);
        if (stat.isSymbolicLink()) throw new Error("symlinked npm notice");
        if (!stat.isFile()) continue;
        if (stat.size === 0 || stat.size > 4 * 1024 * 1024) throw new Error("empty or oversized npm notice");
        const bytes = fs.readFileSync(file);
        if (bytes.includes(0)) throw new Error("binary npm notice");
        new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        files.push({ name, bytes });
      }
      if (!files.length) throw new Error(`no actual license/notice text for emitted npm owner ${identity}`);
      entry = { name: info.name, version: info.version, declared_license: info.license ?? null,
        source: { url: source.resolved, integrity: source.integrity }, modules: new Set(), phases: new Set(), files };
      packages.set(identity, entry);
    }
    entry.modules.add(module);
    entry.phases.add(phase);
    if (entry.modules.size > 4096) throw new Error("too many emitted modules per npm owner");
  }
  function collect(phase, bundle) {
    for (const output of Object.values(bundle)) {
      if (output.type !== "chunk") continue;
      if (!output.modules || typeof output.modules !== "object") throw new Error("unsupported bundler module evidence shape");
      let count = 0;
      for (const [rawId, info] of Object.entries(output.modules)) {
        if (typeof info.renderedLength === "number" && info.renderedLength <= 0) continue;
        if (++count > 10000 || rawId.length > 4096) throw new Error("oversized module inventory");
        const id = rawId.split("?")[0];
        if (id === "\0rolldown/runtime.js") { recordPackage(namedOwner("rolldown"), "generated runtime helpers", phase); continue; }
        if (id.startsWith("\0vite/") || id.startsWith("vite/")) { recordPackage(namedOwner("vite"), id.replace(/^\0/, ""), phase); continue; }
        const relative = slash(path.relative(root, id));
        if (path.isAbsolute(id) && !relative.startsWith("../") && !relative.startsWith("node_modules/") && relative !== "..") {
          for (const [directory, name] of cargoDirectories) if (relative.startsWith(directory)) cargo.add(name);
          continue;
        }
        if (!path.isAbsolute(id) || !id.includes(`${path.sep}node_modules${path.sep}`)) {
          throw new Error(`unclassified emitted module: ${id.replaceAll(root, "[project]")}`);
        }
        const found = owner(id);
        recordPackage(found, slash(path.relative(found.directory, id)), phase);
      }
      chunks.push({ phase, file: output.fileName, rendered_module_count: count });
    }
  }
  function plugin(phase, emit = false) {
    return {
      name: `nuschl-runtime-notices-${phase}`, apply: "build", configResolved,
      buildStart() {
        if (emit) {
          packages.clear(); cargo.clear(); chunks.length = 0;
          lock = JSON.parse(fs.readFileSync(path.join(root, "package-lock.json"), "utf8"));
        }
      },
      generateBundle(_options, bundle) {
        collect(phase, bundle);
        if (!emit) return;
        const records = [];
        for (const entry of [...packages.values()].sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, "en"))) {
          const notices = [];
          for (const file of entry.files) {
            const destination = `licenses/npm/${entry.name}/${entry.version}/${file.name}`;
            this.emitFile({ type: "asset", fileName: destination, source: file.bytes });
            notices.push({ path: destination, sha256: sha256(file.bytes) });
          }
          records.push({ name: entry.name, version: entry.version, declared_license: entry.declared_license, source: entry.source,
            phases: [...entry.phases].sort(), modules: [...entry.modules].sort(), files: notices });
        }
        if (!records.length) throw new Error("empty emitted runtime npm inventory");
        const evidence = { format: 1, scope: "Actual rendered main and worker chunk modules; generated helpers have fixed reviewed owners",
          packages: records, chunks: chunks.sort((a, b) => a.file.localeCompare(b.file, "en")),
          generated_wasm_cargo_roots: [...cargo].sort(),
          packaged_wasm_build_roots: [...cargoDirectories.values()].sort(),
          limits: ["Source graph evidence, not a legal assurance or a dump-regeneration claim.",
            "Generated WASM/glue terms come from the source-authoritative Cargo runtime inventory.",
            "Complete upstream notice files are retained even when they describe more than the rendered helper subset.",
            "Existing Temml/KaTeX/React ancestry notices remain in licenses/temml-*."] };
        this.emitFile({ type: "asset", fileName: "licenses/npm/BUNDLED-NPM.json", source: JSON.stringify(evidence, null, 2) + "\n" });
      },
    };
  }
  return { main: plugin("main", true), worker: () => plugin("worker") };
}
