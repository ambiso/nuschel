# SPDX-License-Identifier: EUPL-1.2
"""Curated evidence exceptions, never an automatic SPDX-to-boilerplate fallback.
Only six explicitly reviewed HPKE identities may use their declared MPL2 terms.
"""
import hashlib
import json
import re
from pathlib import PurePosixPath
import tomllib

POLICY_ID = "hpke-mpl2-six-pins-v1"
REVISIONS = {"0.7.0": "c5fb80f37530ee9b2df9501ae5ff8cb4a973a4bd",
             "0.8.0": "42d68bd49b244b09bd02626c6ffa95e54dc64f91"}
ALLOWED = {(name, version): revision for version, revision in REVISIONS.items()
           for name in ("hpke-rs", "hpke-rs-crypto", "hpke-rs-libcrux")}
PATHS = {"hpke-rs": "crates/protocols/hpke", "hpke-rs-crypto": "crates/protocols/hpke/traits",
         "hpke-rs-libcrux": "crates/protocols/hpke/libcrux_provider"}
MPL_SHA256 = "3f3d9e0024b1921b067d6f7f88deb4a60cbe7a78e76c64e3f1d7fc3b779b9d04"
STEWARDS = "https://raw.githubusercontent.com/mozilla/bedrock/a15178c3c7c976c67b3641af77cae0b66093a175/media/MPL/2.0/index.txt"


def safe(value):
    path = PurePosixPath(value)
    if not value or len(value) > 512 or path.is_absolute() or "\\" in value or any(p in ("", ".", "..") for p in value.split("/")):
        raise ValueError("unsafe evidence path")
    return path


def sha(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 4 * 1024**2:
        raise ValueError("missing/linked/oversized curated evidence")
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate(entry, family, package_root, project_root):
    policy_path = family / "declared-license-policy.json"
    if policy_path.stat().st_size > 128 * 1024:
        raise ValueError("oversized curated policy")
    policy = json.loads(policy_path.read_bytes())
    rows = policy.get("packages", [])
    actual = {(row["name"], row["version"]): row["revision"] for row in rows}
    if policy.get("format") != 1 or policy.get("id") != POLICY_ID or len(rows) != 6 or actual != ALLOWED:
        raise ValueError("curated decision is limited to six exact reviewed pins")
    identity = (entry["name"], entry["version"])
    if (entry.get("evidence_kind") != "declared-license-supplement" or identity not in ALLOWED or
            entry.get("declared_license_policy") != POLICY_ID or entry.get("revision") != ALLOWED[identity] or
            entry.get("declared_license") != "MPL-2.0" or entry.get("status") != "resolved"):
        raise ValueError("unapproved declared-license supplement")
    row = next(item for item in rows if (item["name"], item["version"]) == identity)
    expected_tree = f'https://github.com/celabshq/libcrux/tree/{ALLOWED[identity]}/{PATHS[identity[0]]}'
    source = row["source_access"]
    expected_crate = f'https://static.crates.io/crates/{identity[0]}/{identity[0]}-{identity[1]}.crate'
    if (source.get("published_crate_url") != expected_crate or source.get("repository_tree_url") != expected_tree or
            not re.fullmatch(r"[0-9a-f]{64}", source.get("published_crate_sha256", "")) or entry.get("source_access") != source):
        raise ValueError("missing/mismatched immutable corresponding-source links")
    roles = {item.get("role"): item for item in entry["files"]}
    if len(entry["files"]) != 4 or set(roles) != {"package-declaration", "license-steward", "source-header-index", "source-access"}:
        raise ValueError("missing declared license/source/header evidence")
    for item in entry["files"]:
        if sha(family / safe(item["local"])) != item["sha256"]:
            raise ValueError("curated evidence hash mismatch")
    declaration = row["package_declaration"]
    if roles["package-declaration"] != declaration:
        raise ValueError("mismatched verbatim package declaration")
    parsed = tomllib.loads((family / safe(declaration["local"])).read_text())["package"]
    if (parsed.get("name"), parsed.get("version"), parsed.get("license")) != (*identity, "MPL-2.0"):
        raise ValueError("declaration does not grant the selected terms")
    if parsed.get("authors") != row["authors"] or parsed.get("repository") != row["repository"]:
        raise ValueError("author/repository declaration was rewritten")
    terms = roles["license-steward"]
    if (terms["local"] != row["steward_file"] or terms["url"] != STEWARDS or
            terms["sha256"] != MPL_SHA256 or row["steward_sha256"] != MPL_SHA256):
        raise ValueError("not the reviewed canonical Mozilla steward text")
    headers = json.loads((family / safe(row["header_file"])).read_bytes())
    if roles["source-header-index"]["local"] != row["header_file"] or headers["source_archive_sha256"] != source["published_crate_sha256"]:
        raise ValueError("missing published source/header identity")
    actual_rust = {file.relative_to(package_root).as_posix() for file in (package_root / "src").rglob("*.rs")}
    if not actual_rust or {file["path"] for file in headers["files"]} != actual_rust:
        raise ValueError("incomplete source-header scan")
    for file in headers["files"]:
        if sha(package_root / safe(file["path"])) != file["sha256"]:
            raise ValueError("source header scan differs from compiled source")
    instructions = (family / safe(row["instructions_file"])).read_text()
    if roles["source-access"]["local"] != row["instructions_file"] or not all(value in instructions for value in (expected_crate, expected_tree, source["published_crate_sha256"], "MPL-2.0")):
        raise ValueError("missing corresponding Source Code Form instructions")
    modified = source.get("modified_source")
    if identity == ("hpke-rs-libcrux", "0.7.0"):
        if (not modified or modified.get("project_path") != "vendor/hpke-rs-libcrux" or
                modified.get("bundled_archive") != "hpke-rs-libcrux-0.7.0-source.tar" or not modified.get("files")):
            raise ValueError("patched corresponding source is not specified")
        directory = project_root / modified["project_path"]
        actual_files = {file.relative_to(directory).as_posix() for file in directory.rglob("*") if file.is_file()}
        if actual_files != set(modified["files"]):
            raise ValueError("patched source file set differs from curated evidence")
        for name, digest in modified["files"].items():
            if sha(directory / safe(name)) != digest:
                raise ValueError("patched corresponding-source bytes differ")
    elif modified is not None:
        raise ValueError("unexpected modified-source exception")
    return row


def verify_bundled(report, root, notice_hash, project_root):
    """Verify corresponding patched source and the curated decision in a payload.
    Ordinary packages cannot acquire this exception just by declaring an SPDX ID.
    """
    import tarfile
    hpke = [item for item in report['packages'] if item['name'] in PATHS]
    if not hpke:
        if report.get('source_archives') or report.get('curated_evidence_policies'):
            raise ValueError('unexpected curated policy/source payload')
        return
    family = project_root / 'release/licenses/crypto-upstreams'
    policy_path = family / 'declared-license-policy.json'
    policy = json.loads(policy_path.read_bytes())
    decisions = {(item['name'], item['version']): item for item in policy['packages']}
    expected = [{'id': POLICY_ID, 'path': 'EVIDENCE-POLICY-hpke-mpl2.json', 'sha256': sha(policy_path)}]
    if report.get('curated_evidence_policies') != expected or notice_hash(root / expected[0]['path']) != expected[0]['sha256']:
        raise ValueError('missing or foreign curated declared-license decision')
    for item in hpke:
        identity = (item['name'], item['version'])
        if identity not in ALLOWED or item.get('corresponding_source') != decisions[identity]['source_access']:
            raise ValueError('missing exact-pin corresponding-source declaration')
    required = [decisions[(item['name'], item['version'])]['source_access']['modified_source']
                for item in hpke if (item['name'], item['version']) == ('hpke-rs-libcrux', '0.7.0')]
    archives = report.get('source_archives', [])
    if len(archives) != len(required):
        raise ValueError('missing/extra patched corresponding source archive')
    for expected, archive in zip(required, archives):
        if (archive.get('path') != expected['bundled_archive'] or archive.get('files') != expected['files'] or
                archive.get('project_path') != expected['project_path'] or archive.get('package') != 'hpke-rs-libcrux' or
                archive.get('version') != '0.7.0'):
            raise ValueError('corresponding source identity differs from reviewed policy')
        path = root / safe(archive['path'])
        if path.stat().st_size != archive['size'] or sha(path) != archive['sha256']:
            raise ValueError('corresponding source archive digest mismatch')
        found = set()
        with tarfile.open(path, 'r:') as source:
            for member in source:
                if not member.isfile() or not member.name.startswith('hpke-rs-libcrux-0.7.0/'):
                    raise ValueError('unsafe source archive member')
                name = member.name.removeprefix('hpke-rs-libcrux-0.7.0/')
                safe(name)
                if name in found or name not in expected['files'] or member.size > 4 * 1024**2:
                    raise ValueError('duplicate/foreign/oversized source member')
                found.add(name)
                stream = source.extractfile(member)
                if stream is None or hashlib.sha256(stream.read()).hexdigest() != expected['files'][name]:
                    raise ValueError('corresponding source file bytes differ')
        if found != set(expected['files']):
            raise ValueError('incomplete patched corresponding source')
