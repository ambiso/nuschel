# SPDX-License-Identifier: EUPL-1.2
# Metadata only: reuse locked Cargo vendoring; never compile another Rust graph.
# Supply the actual packaged binary roots for each distributed target, including
# helper executables. `inventory` remains inspectable when attribution is missing;
# installers, archives and signing MUST depend on `bundle` or `check`.
{
  pkgs,
  craneLib,
  projectRoot,
  rootsByTarget,
  # Pinned toolchain/native-runtime notices are outside Cargo's dependency graph.
  # [{ name; version; source; files = [{ name; path; }]; }]
  externalNotices ? [ ],
}:
let
  inherit (pkgs) lib;
  source = lib.cleanSourceWith {
    src = projectRoot;
    filter =
      path: _:
      !(builtins.elem (builtins.baseNameOf path) [
        ".git"
        ".jj"
        ".direnv"
        "target"
        "node_modules"
      ]);
  };
  roots = pkgs.writeText "nuschl-license-roots.json" (builtins.toJSON rootsByTarget);
  external = pkgs.writeText "nuschl-external-notices.json" (builtins.toJSON externalNotices);
  collect = pkgs.writeText "nuschl-collect-licenses.py" ''
    import hashlib, json, os, pathlib, re, shutil, subprocess

    source = pathlib.Path.cwd().resolve()
    out = pathlib.Path(os.environ.get('LICENSE_OUT', 'license-output')) / 'share/licenses/nuschl'
    out.mkdir(parents=True)
    metadata = json.loads(pathlib.Path(os.environ.get('METADATA_FILE', 'metadata.json')).read_text())
    packages = {(p['name'], p['version']): p for p in metadata['packages']}
    if len(packages) != len(metadata['packages']):
        raise SystemExit('ambiguous name/version package source; review inventory identity')
    selected = {}
    targets = json.loads(pathlib.Path(os.environ['ROOTS_FILE']).read_text())
    if not targets or len(targets) > 8:
        raise SystemExit('explicit nonempty bounded rootsByTarget is required')
    for target in targets:
        triple, names = target['target'], target['packages']
        if not re.fullmatch(r'[a-zA-Z0-9_.-]{1,80}', triple) or not names or len(names) > 64:
            raise SystemExit('invalid target/package roots')
        command = ['cargo', 'tree', '--locked', '--offline', '--target', triple,
                   '--edges', 'normal,no-proc-macro', '--prefix', 'none', '--format', '{p}']
        for name in names:
            if not re.fullmatch(r'[a-zA-Z0-9_-]{1,100}', name):
                raise SystemExit('invalid packaged root')
            command.extend(['-p', name])
        tree = subprocess.run(command, check=True, capture_output=True, text=True, timeout=120).stdout
        for line in tree.splitlines():
            if not line.strip(): continue
            match = re.match(r'^(\S+) v(\S+)(?:\s|$)', line)
            if not match:
                raise SystemExit('unrecognized cargo tree record: ' + line)
            identity = match.groups()
            if identity not in packages:
                raise SystemExit('tree package absent from metadata: ' + repr(identity))
            selected.setdefault(identity, set()).add(triple)

    issues, supplements = [], {}
    supplements_root = source / 'release/licenses'
    for provenance in sorted(supplements_root.glob('*/provenance.json')):
        family = provenance.parent
        record = json.loads(provenance.read_text())
        for package in record.get('packages', []):
            identity = (package['name'], package['version'])
            if identity not in selected:
                continue
            supplements.setdefault(identity, []).append((family, package))

    def issue(identity, reason):
        issues.append({'name': identity[0], 'version': identity[1], 'reason': reason})

    copied_paths = {}
    def copy_text(path, destination):
        if not path.is_file() or path.is_symlink() or path.stat().st_size > 4 * 1024 * 1024:
            raise ValueError('missing, symlinked or oversized attribution file: ' + str(path))
        data = path.read_bytes()
        if not data.strip() or b'\0' in data:
            raise ValueError('empty or binary attribution file: ' + str(path))
        digest = hashlib.sha256(data).hexdigest()
        key = str(destination).casefold()
        if key in copied_paths and copied_paths[key] != digest:
            raise ValueError('conflicting attribution destination (including Windows case folding): ' + str(destination))
        copied_paths[key] = digest
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        return digest

    def notice_file(path, root):
        relative = path.relative_to(root)
        return bool(re.match(r'(?i)^(licen[cs]e|copying|notice|copyright|unlicense)([._-].*|$)', path.name)
                    or any(part.upper() == 'LICENSES' for part in relative.parts[:-1]))

    entries = []
    for identity, platforms in sorted(selected.items()):
        package = packages[identity]
        root = pathlib.Path(package['manifest_path']).parent.resolve()
        dest = out / 'crates' / (identity[0] + '-' + identity[1])
        copied = []
        for path in sorted(root.rglob('*')):
            if path.is_file() and notice_file(path, root):
                relative = path.relative_to(root)
                try:
                    digest = copy_text(path, dest / relative)
                    copied.append({'path': str(path.relative_to(root)), 'sha256': digest,
                                   'origin': 'published-or-patched-source'})
                except ValueError as error:
                    issue(identity, str(error))
        explicit = package.get('license_file')
        if explicit:
            path = pathlib.Path(explicit)
            if not path.is_absolute(): path = root / path
            path = path.resolve()
            if path.is_file() and not any(f['path'] == str(path.relative_to(root)) for f in copied if path.is_relative_to(root)):
                try:
                    digest = copy_text(path, dest / 'declared-license-file')
                    copied.append({'path': 'declared-license-file', 'sha256': digest,
                                   'origin': 'manifest-license-file'})
                except ValueError as error:
                    issue(identity, str(error))
        for family, supplement in supplements.get(identity, []):
            revision = supplement.get('revision', "")
            if supplement.get('status') != 'resolved' or not re.fullmatch(r'[0-9a-f]{40}', revision):
                issue(identity, 'unresolved or unpinned upstream supplement')
                continue
            vcs = root / '.cargo_vcs_info.json'
            if vcs.is_file() and json.loads(vcs.read_text()).get('git', {}).get('sha1') != revision:
                issue(identity, 'supplement revision differs from published crate VCS metadata')
                continue
            if supplement.get('declared_license') != package.get('license'):
                issue(identity, 'supplement license does not match the locked package')
                continue
            for file in supplement.get('files', []):
                relative = pathlib.PurePosixPath(file['local'])
                if relative.is_absolute() or '..' in relative.parts:
                    issue(identity, 'unsafe supplement file path')
                    continue
                path = family / relative
                try:
                    digest = copy_text(path, dest / 'upstream' / relative)
                    if digest != file['sha256']:
                        issue(identity, 'supplement hash mismatch: ' + str(relative))
                        continue
                    copied.append({'path': 'upstream/' + str(relative), 'sha256': digest,
                                   'origin': 'pinned-upstream', 'repository': supplement['repository'],
                                   'revision': revision, 'upstream_path': file['upstream_path'],
                                   'url': file['url']})
                except ValueError as error:
                    issue(identity, str(error))
        first_party = package['id'] in metadata['workspace_members'] and package.get('license') == 'EUPL-1.2'
        if not copied and not first_party:
            issue(identity, 'no actual license/notice text; generic SPDX fallback forbidden')
        if not package.get('license') and not explicit:
            issue(identity, 'no declared license expression or license-file')
        entries.append({
            'name': identity[0], 'version': identity[1], 'license': package.get('license'),
            'authors': package.get('authors', []), 'repository': package.get('repository'),
            'source': package.get('source') or 'project-path', 'targets': sorted(platforms),
            'first_party': first_party, 'files': copied,
        })

    assets = [
        ('LICENSE', 'EUPL-1.2'),
        ('crates/protocol/proto/LICENSE.mumble', 'Mumble-BSD'),
        ('crates/math/fonts/GUST-FONT-LICENSE.txt', 'NewCMMath-GUST-Font-License'),
    ]
    for source_path, name in assets:
        copy_text(source / source_path, out / name)
    external_entries = []
    external_names = set()
    for component in json.loads(pathlib.Path(os.environ['EXTERNAL_NOTICES']).read_text()):
        name = component['name']
        identity = (name, component['version'])
        if not re.fullmatch(r'[a-zA-Z0-9_.-]{1,100}', name) or name in external_names:
            raise SystemExit('invalid or duplicate external component')
        external_names.add(name)
        files = []
        for file in component['files']:
            relative = pathlib.PurePosixPath(file['name'])
            if relative.is_absolute() or '..' in relative.parts:
                issue(identity, 'unsafe external notice name')
                continue
            try:
                digest = copy_text(pathlib.Path(file['path']).resolve(), out / 'external' / name / relative)
                files.append({'path': str(relative), 'sha256': digest})
            except ValueError as error:
                issue(identity, str(error))
        if not files: issue(identity, 'external component has no actual notice files')
        external_entries.append({'name': name, 'version': component['version'],
                                 'source': component['source'], 'files': files})
    required_external = {'rust-standard-library'}
    if any('windows-gnu' in target['target'] for target in targets):
        required_external.update({'mingw-w64-runtime', 'gcc-runtime'})
    if any(name == 'nuschl-math-worker' for name, _ in selected):
        required_external.add('newcm-math-font')
    if any(name == 'syntect' for name, _ in selected):
        required_external.add('syntect-default-assets')
    if any(name == 'nuschl-client-wasm' for name, _ in selected):
        required_external.add('web-runtime-assets')
    for name in sorted(required_external - external_names):
        issue((name, 'toolchain'), 'pinned non-Cargo runtime notices have not been supplied')
    report = {
        'format': 1, 'scope': 'normal non-proc-macro dependency union of explicit packaged roots/targets',
        'roots_by_target': targets, 'packages': entries, 'unresolved': issues,
        'external_components': external_entries,
        'limits': [
            'Inventory is not a legal compatibility or compliance assurance.',
            'Presence of one notice does not establish every upstream embedded asset was reviewed.',
            'Build/dev/proc-macro dependencies are excluded; separately bundled runtime libraries/assets require explicit review.',
        ],
    }
    (out / 'THIRD-PARTY.json').write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')
    shutil.copyfile(source / 'Cargo.lock', out / 'Cargo.lock')
    (out / 'README.txt').write_text(
        'Third-party source notices and pinned upstream supplements. See THIRD-PARTY.json for scope and provenance.\n'
        'Root EUPL applies to the first-party packages; their source is published with the release.\n'
        'This inventory does not replace review of embedded assets, native libraries, modifications or source obligations.\n')
    print(json.dumps({'runtime_packages': len(entries), 'unresolved': len(issues)}))
  '';
  inventory = craneLib.mkCargoDerivation {
    pname = "nuschl-license-inventory";
    version = "1";
    src = source;
    cargoArtifacts = null;
    cargoVendorDir = craneLib.vendorCargoDeps { src = source; };
    nativeBuildInputs = [ pkgs.python3 ];
    doCheck = false;
    ROOTS_FILE = roots;
    EXTERNAL_NOTICES = external;
    buildPhaseCargoCommand = ''
      cargo metadata --locked --offline --format-version 1 > metadata.json
      python3 ${collect}
    '';
    installPhaseCommand = ''
      cp -R license-output "$out"
    '';
  };
  check = pkgs.runCommand "nuschl-license-completeness" { nativeBuildInputs = [ pkgs.python3 ]; } ''
    python3 - ${inventory}/share/licenses/nuschl/THIRD-PARTY.json <<'PY'
    import json, sys
    report = json.load(open(sys.argv[1]))
    if report['unresolved']:
        for issue in report['unresolved']:
            print(issue['name'], issue['version'], issue['reason'], file=sys.stderr)
        raise SystemExit('runtime attribution inventory is incomplete; inspect the inventory derivation')
    PY
    touch "$out"
  '';
  bundle = pkgs.runCommand "nuschl-release-licenses" { } ''
    test -e ${check}
    cp -R ${inventory} "$out"
  '';
in
{
  inherit inventory check bundle;
}
