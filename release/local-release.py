#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Local, exact-tag preparation/signing; separate explicit draft-first publication.

Never generates/enrolls keys. The operator supplies a local secret path to sign;
GitHub receives only already-signed assets. Local receipts are operator-controlled
records, not remote attestation or a proof of reproducible compilation.
"""
import argparse
import hashlib
import types
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PROGRAMS = ('nuschl', 'nuschl-content-worker', 'nuschl-crypto-worker', 'nuschl-math-worker',
            'nuschl-media-worker', 'nuschl-storage-worker', 'nuschl-transfer-worker',
            'nuschl-update-verify-ed25519', 'nuschl-update-verify-mldsa44')
PRODUCTS = ('windows', 'macos-app', 'default', 'server', 'web-relay', 'web-site')


def module(path):
    # Loading immutable exported policy must not create __pycache__ in its tree.
    result = types.ModuleType(path.stem.replace('-', '_'))
    result.__file__ = str(path)
    exec(compile(path.read_bytes(), str(path), 'exec'), result.__dict__)
    return result


def require(value, reason):
    if not value:
        raise ValueError(reason)


def run(args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, capture_output=True, text=True,
                          timeout=kwargs.pop('timeout', 300), **kwargs).stdout


def sha(path):
    h, count = hashlib.sha256(), 0
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_size <= 4 * 1024**3,
                'hash input must be a bounded regular file')
        while block := stream.read(1024 * 1024):
            count += len(block)
            require(count <= before.st_size, 'file grew during hashing')
            h.update(block)
        after = os.fstat(stream.fileno())
        require(count == before.st_size and (before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
                (after.st_size, after.st_mtime_ns, after.st_ctime_ns), 'file changed during hashing')
    return h.hexdigest()


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON field')
        result[key] = value
    return result


def load(path):
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= 8 * 1024**2,
            'missing, linked or oversized release record')
    return json.loads(path.read_bytes(), object_pairs_hook=unique)


def write(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')


def tree(root, *, files=32768, size=4 * 1024**3):
    require(root.is_dir() and not root.is_symlink(), 'regular tree required')
    result, total, names = {}, 0, set()
    pending, paths, entries = [root], [], 0
    while pending:
        directory = pending.pop()
        require(len(directory.relative_to(root).parts) <= 32, 'tree depth limit exceeded')
        with os.scandir(directory) as children:
            for child in children:
                entries += 1
                require(entries <= files * 4, 'aggregate directory/file entry limit exceeded')
                require(not child.is_symlink(), 'links are not distributable')
                if child.is_dir(follow_symlinks=False):
                    pending.append(Path(child.path))
                else:
                    require(child.is_file(follow_symlinks=False), 'special file in payload')
                    require(len(paths) < files, 'file count limit exceeded')
                    paths.append(Path(child.path))
    for path in sorted(paths):
        name = path.relative_to(root).as_posix()
        require(not any(x in ('', '.', '..') for x in name.split('/')) and
                '\\' not in name and len(name) <= 512 and name.casefold() not in names,
                'unsafe or case-aliased payload path')
        names.add(name.casefold())
        total += path.stat().st_size
        require(path.stat().st_size <= 512 * 1024**2 and total <= size,
                'payload byte budget exceeded')
        result[name] = {'size': path.stat().st_size, 'sha256': sha(path),
                        'executable': bool(path.stat().st_mode & 0o111)}
    require(result, 'empty payload')
    return result


def canonical_tar(root, output, prefix=None):
    entries = tree(root)
    with tarfile.open(output, 'x', format=tarfile.PAX_FORMAT) as archive:
        # Explicit sorted regular files; no owner, timestamps, links or host paths.
        for name, record in entries.items():
            info = tarfile.TarInfo((prefix + '/' if prefix else '') + name)
            info.size, info.mode = record['size'], 0o755 if record['executable'] else 0o644
            with (root / name).open('rb') as stream:
                archive.addfile(info, stream)


def canonical_zip(root, output, prefix=None):
    with zipfile.ZipFile(output, 'x', zipfile.ZIP_DEFLATED) as archive:
        for name, record in tree(root).items():
            info = zipfile.ZipInfo((prefix + '/' if prefix else '') + name, (1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = ((0o100755 if record['executable'] else 0o100644) << 16)
            with (root / name).open('rb') as source, archive.open(info, 'w') as target:
                shutil.copyfileobj(source, target, 1024 * 1024)


def revision(repository, tag):
    require(re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?', tag), 'exact version tag required')
    commit = run(['git', '-C', repository, 'rev-parse', '--verify', 'refs/tags/' + tag + '^{commit}']).strip()
    require(re.fullmatch('[0-9a-f]{40}', commit), 'full commit required')
    return commit


def export(repository, commit, destination):
    destination.mkdir()
    with tempfile.TemporaryFile() as temp:
        subprocess.run(['git', '-C', str(repository), 'archive', commit], stdout=temp, check=True)
        temp.seek(0)
        with tarfile.open(fileobj=temp) as archive:
            archive.extractall(destination, filter='data')


def snapshot(repository, commit, source):
    # Compare all exported bytes and modes to a new immutable Git export.
    with tempfile.TemporaryDirectory(prefix='release-source-') as tmp:
        clean = Path(tmp) / 'source'
        export(repository, commit, clean)
        require(tree(clean) == tree(source), 'release source export changed')


def guarded_build(arguments, log):
    def resources():
        mem = int(next(x.split()[1] for x in Path('/proc/meminfo').read_text().splitlines()
                       if x.startswith('MemAvailable:'))) * 1024
        require(min(mem, shutil.disk_usage('/tmp').free) >= 8 * 1024**3,
                'need at least8GiB MemAvailable AND/tmp free; no build started/continued')
    resources()
    with log.open('x') as output:
        process = subprocess.Popen(arguments, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while process.poll() is None:
                resources()
                time.sleep(5)
            require(process.returncode == 0, 'Nix build failed; retain log')
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


def authority(source):
    return module(source / 'release/prepare-artifacts.py')


def check_installs(stage, source):
    a = authority(source)
    policy = a.load(source / 'release/notice-roots.json')
    for platform, root in [('windows-x86_64', stage / 'install/windows'),
                           ('macos-universal', stage / 'install/Nuschl.app')]:
        tree(root, files=256)
        binaries = root if platform == 'windows-x86_64' else root / 'Contents/MacOS'
        suffix = '.exe' if platform == 'windows-x86_64' else ''
        actual = {p.name for p in binaries.iterdir() if p.is_file()}
        if suffix:
            require({n + suffix for n in PROGRAMS} <= actual and
                    {n for n in actual if n.endswith('.exe')} == {n + suffix for n in PROGRAMS},
                    'all nine fixed Windows programs required, no signer/fixture')
        else:
            require(actual == set(PROGRAMS), 'all nine fixed Mac programs required')
        notices = root / 'licenses' if suffix else root / 'Contents/Resources/licenses'
        a.verify_notices(notices, policy, platform)
    plist = plistlib.loads((stage / 'install/Nuschl.app/Contents/Info.plist').read_bytes())
    version = tomllib.loads((source / 'Cargo.toml').read_text())['workspace']['package']['version']
    expected = {'CFBundleExecutable': 'nuschl', 'CFBundleIdentifier': 'org.nuschl.Nuschl',
                'CFBundlePackageType': 'APPL', 'CFBundleShortVersionString': version,
                'CFBundleVersion': version, 'LSMinimumSystemVersion': '14.2'}
    require(all(type(plist.get(k)) is type(v) and plist[k] == v for k, v in expected.items()),
            'Mac bundle metadata differs from source policy')


def select_product(flake, name):
    selected = json.loads(run(['nix', 'eval', '--json', flake + '#packages.x86_64-linux.' + name,
                              '--apply', 'p: {drv=p.drvPath; output=p.outPath;}']))
    require(re.fullmatch(r'/nix/store/[0-9a-z]{32}-[^/]+\.drv', selected['drv']) and
            re.fullmatch(r'/nix/store/[0-9a-z]{32}-[^/]+', selected['output']),
            'Nix selection must name exact store derivation/output')
    raw = json.loads(run(['nix', 'path-info', '--json', selected['output']]))
    info = raw[0] if isinstance(raw, list) else raw[selected['output']]
    require(Path(selected['output']).is_dir() and
            re.fullmatch(r'sha256-[A-Za-z0-9+/]{43}=', info.get('narHash', '')),
            'selected Nix output is not built/has no NAR receipt; use --build')
    return {**selected, 'narHash': info['narHash'], 'source_kind': 'local-nix'}


def archive_tree(path, expected, prefix=None, *, zipped=False):
    found, total = {}, 0
    with (zipfile.ZipFile(path) if zipped else tarfile.open(path, 'r:')) as archive:
        members = archive.infolist() if zipped else archive
        for item in members:
            name = item.filename if zipped else item.name
            size = item.file_size if zipped else item.size
            require(not (item.is_dir() if zipped else not item.isfile()), 'regular-only archive required')
            require(len(found) < 32768 and name not in found and size <= 512 * 1024**2,
                    'duplicate/oversized archive member')
            total += size
            require(total <= 4 * 1024**3, 'aggregate archive size exceeded')
            if prefix:
                require(name.startswith(prefix + '/'), 'foreign archive root')
                name = name[len(prefix) + 1:]
            require(name in expected and name not in found, 'foreign/duplicate archive file')
            mode = item.external_attr >> 16 if zipped else item.mode
            if zipped:
                require(not item.flag_bits & 1 and mode & 0o170000 == 0o100000,
                        'encrypted or nonregular ZIP member')
            stream = archive.open(item) if zipped else archive.extractfile(item)
            digest, count = hashlib.sha256(), 0
            with stream:
                while block := stream.read(1024 * 1024):
                    count += len(block)
                    require(count <= size, 'archive expanded beyond declared length')
                    digest.update(block)
            require(count == size, 'truncated archive member')
            found[name] = {'size': size, 'sha256': digest.hexdigest(), 'executable': bool(mode & 0o111)}
    require(found == expected, 'archive differs from exact selected product')


def check_products(stage, ledger, repository):
    flake = 'git+file://' + str(repository.resolve()) + '?rev=' + ledger['commit']
    expected = {name: select_product(flake, name) for name in PRODUCTS}
    require(ledger['products'] == expected, 'selected-tag derivation/output/NAR evidence changed')
    windows = Path(expected['windows']['output'])
    mapped = tree(windows / 'bin')
    mapped.update({'licenses/' + name: value for name, value in
                   tree(windows / 'share/licenses/nuschl').items()})
    require(tree(stage / 'install/windows') == mapped, 'staged Windows files differ from selected Nix product')
    mac = tree(Path(expected['macos-app']['output']) / 'Nuschl.app')
    require(tree(stage / 'install/Nuschl.app') == mac, 'staged Mac files differ from selected Nix product')
    version = ledger['version']
    archive_tree(stage / 'macos-install.tar', mac, 'Nuschl.app')
    archive_tree(stage / f'dist/nuschl-{version}-macos-universal.zip', mac, 'Nuschl.app', zipped=True)
    archive_tree(stage / f'dist/nuschl-{version}-windows-x86_64.zip', mapped, zipped=True)
    archive_tree(stage / f'dist/nuschl-{version}-web-site.tar',
                 tree(Path(expected['web-site']['output'])))
    require(load(stage / f'dist/nuschl-{version}-linux-nix.json') == expected,
            'Linux build references differ from actual selected outputs')
    require(sha(stage / 'dist/flake.lock') == sha(stage / 'source/flake.lock'), 'foreign published lockfile')


def prepare(args):
    repository = args.source_repo.resolve(strict=True)
    commit = revision(repository, args.tag)
    require(re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repository), 'invalid repository')
    stage = args.output.resolve()
    stage.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = stage / 'source'
    export(repository, commit, source)
    version = tomllib.loads((source / 'Cargo.toml').read_text())['workspace']['package']['version']
    require(args.tag == 'v' + version, 'tag must match workspace version')
    flake = 'git+file://' + str(repository) + '?rev=' + commit
    products = {}
    for name in PRODUCTS:
        if args.build:
            guarded_build(['nix', 'build', '--cores', '2', '--max-jobs', '2', '--no-link',
                           flake + '#' + name], stage / (name + '-build.log'))
        products[name] = select_product(flake, name)
    install = stage / 'install'
    install.mkdir()
    windows = install / 'windows'
    shutil.copytree(Path(products['windows']['output']) / 'bin', windows)
    shutil.copytree(Path(products['windows']['output']) / 'share/licenses/nuschl', windows / 'licenses')
    shutil.copytree(Path(products['macos-app']['output']) / 'Nuschl.app', install / 'Nuschl.app')
    check_installs(stage, source)
    a = authority(source)
    policy = a.load(source / 'release/notice-roots.json')
    for product in ('default', 'server'):
        a.verify_notices(Path(products[product]['output']) / 'share/licenses/nuschl', policy, 'linux-nix')
    web = Path(products['web-site']['output'])
    a.verify_notices(web / 'licenses/nuschl', policy, 'web')
    require((web / 'index.html').is_file() and (web / 'app/index.html').is_file(), 'web entry points missing')
    dist = stage / 'dist'
    dist.mkdir()
    canonical_tar(install / 'Nuschl.app', stage / 'macos-install.tar', 'Nuschl.app')
    canonical_tar(web, dist / f'nuschl-{version}-web-site.tar')
    canonical_zip(windows, dist / f'nuschl-{version}-windows-x86_64.zip')
    canonical_zip(install / 'Nuschl.app', dist / f'nuschl-{version}-macos-universal.zip', 'Nuschl.app')
    write(dist / f'nuschl-{version}-linux-nix.json', products)
    shutil.copyfile(source / 'flake.lock', dist / 'flake.lock')
    (dist / 'INSTALL-NIX.txt').write_text(f'Linux requires Nix; no portable Linux binary is supplied.\n'
        f'nix run github:{args.repository}/{commit}\n'
        f'nix build github:{args.repository}/{commit}#server\n')
    base = f'https://github.com/{args.repository}/releases/download/{args.tag}/'
    # The producer owns all updater naming/path/URL/size rules.
    run([sys.executable, source / 'release/make-update.py', '--version', version, '--base-url', base,
         '--platform', f'windows-x86_64={windows}', '--platform', f'macos-universal={install / "Nuschl.app"}',
         '--output', dist])
    payloads = tree(dist)
    del payloads['update.json']
    ledger = {'format': 1, 'tag': args.tag, 'version': version, 'commit': commit,
              'repository': args.repository, 'source_kind_detail': 'exact local Git revision; no CI claim',
              'publisher_tool_sha256': sha(Path(__file__)), 'products': products,
              'source_tree': tree(source), 'installations': tree(install), 'macos_archive_sha256': sha(stage / 'macos-install.tar'),
              'assets': payloads, 'source_kind': 'local-nix; not GitHub CI provenance'}
    write(stage / 'ledger.json', ledger)
    write(stage / 'local.json', {'source_repository': str(repository), 'flake': flake})
    shutil.copyfile(stage / 'ledger.json', dist / 'release-ledger.json')
    raw = load(dist / 'update.json')
    raw['notes'] = f'https://github.com/{args.repository}/releases/tag/{args.tag}\nledger-sha256={sha(stage / "ledger.json")}'
    (dist / 'update.json').write_text(json.dumps(raw, indent=1) + '\n')
    print(f'Prepared {stage}; unsigned. Native Mac verification and local signing remain required.')


def validate(stage):
    ledger = load(stage / 'ledger.json')
    require(ledger['format'] == 1 and ledger['tag'] == 'v' + ledger['version'], 'invalid ledger')
    repo = Path(load(stage / 'local.json')['source_repository'])
    require(revision(repo, ledger['tag']) == ledger['commit'], 'immutable selected tag moved')
    require(tree(stage / 'source') == ledger['source_tree'], 'source bytes differ from prepared ledger')
    snapshot(repo, ledger['commit'], stage / 'source')
    require(tree(stage / 'install') == ledger['installations'], 'installation changed after preparation')
    require(sha(stage / 'macos-install.tar') == ledger['macos_archive_sha256'], 'Mac archive changed')
    actual = tree(stage / 'dist')
    require(sha(stage / 'dist/release-ledger.json') == sha(stage / 'ledger.json'), 'public ledger differs')
    for name in ('update.json', 'update.json.sig', 'SHA256SUMS', 'release-ledger.json'):
        actual.pop(name, None)
    require(actual == ledger['assets'], 'release payload changed or assets added/removed')
    manifest = load(stage / 'dist/update.json')
    require(manifest['version'] == ledger['version'] and
            f'ledger-sha256={sha(stage / "ledger.json")}' in manifest.get('notes', '').splitlines(),
            'signed source ledger binding absent')
    expected = {}
    for platform, relative in [('windows-x86_64', 'windows'), ('macos-universal', 'Nuschl.app')]:
        expected[platform] = []
        for name, path in module(stage / 'source/release/make-update.py').files_of(stage / 'install' / relative):
            asset = platform + '--' + name.replace('/', '__')
            expected[platform].append({'path': name, 'url': f'https://github.com/{ledger["repository"]}/releases/download/{ledger["tag"]}/{asset}',
                                       'size': path.stat().st_size, 'sha256': sha(path)})
            require(sha(stage / 'dist' / asset) == sha(path), 'updater asset differs from installation')
    require(manifest['platforms'] == expected, 'manifest file set/hash/URLs changed')
    check_installs(stage, stage / 'source')
    check_products(stage, ledger, repo)
    return ledger


def verify_macos(args):
    stage = args.stage.resolve(strict=True)
    ledger = load(stage / 'ledger.json')
    require(tree(stage / 'source') == ledger['source_tree'], 'staged source changed')
    actual = tree(stage / 'install/Nuschl.app')
    expected = {name.removeprefix('Nuschl.app/'): item for name, item in ledger['installations'].items()
                if name.startswith('Nuschl.app/')}
    require(actual == expected and sha(stage / 'macos-install.tar') == ledger['macos_archive_sha256'],
            'Mac verification input differs from prepared ledger')
    native = module(ROOT / 'tests/macos_native.py')
    report = {'status': 'failed', 'artifact_binaries_executed': False,
              'source': 'local-native-operator-controlled', 'commit': ledger['commit'],
              'ledger_sha256': sha(stage / 'ledger.json'),
              'archive_sha256': ledger['macos_archive_sha256']}
    out = stage / 'macos-native'
    out.mkdir(exist_ok=False)
    try:
        native.verify_native(stage / 'install/Nuschl.app', out, report, local=True)
    finally:
        write(out / 'report.json', report)


def ci_evidence(stage, ledger, gh, run_id, output):
    require(re.fullmatch(r'[1-9][0-9]*', run_id), 'invalid CI run ID')
    repo, commit = ledger['repository'], ledger['commit']
    record = json.loads(run([gh, 'api', f'repos/{repo}/actions/runs/{run_id}']))
    require(record.get('head_sha') == commit and record.get('status') == 'completed' and
            record.get('conclusion') == 'success' and record.get('event') in ('push', 'workflow_dispatch') and
            record.get('head_repository', {}).get('full_name') == repo and
            record.get('path') in ('.github/workflows/build.yml', '.github/workflows/release.yml'),
            'CI run must be successful trusted same-repository/same-commit build')
    jobs = json.loads(run([gh, 'api', f'repos/{repo}/actions/runs/{run_id}/jobs?per_page=100']))['jobs']
    require(any(j.get('name', '').split(' / ')[-1] == 'macos-native-verify' and
                j.get('conclusion') == 'success' and j.get('head_sha') == commit for j in jobs),
            'successful native Mac job absent')
    report_name = 'nuschl-macos-native-verification-' + commit
    artifact_name = 'nuschl-macos-universal-' + commit
    artifacts = json.loads(run([gh, 'api', f'repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100']))['artifacts']
    selected = {}
    for name in (report_name, artifact_name):
        matches = [a for a in artifacts if a.get('name') == name and not a.get('expired')]
        require(len(matches) == 1, 'missing/ambiguous/expired same-run Mac artifact')
        selected[name] = matches[0]['id']
    for name, directory in ((report_name, 'report'), (artifact_name, 'artifact')):
        run([gh, 'run', 'download', run_id, '--repo', repo, '--name', name,
             '--dir', output / directory], timeout=1800)
    original = load(output / 'report/report.json')
    require(original.get('status') == 'native_apple_static_verified' and
            original.get('artifact_name') == artifact_name and
            original.get('source', {}).get('commit') == commit and
            original.get('host', {}).get('platform') == 'darwin' and original.get('commands') and
            original.get('artifact_binaries_executed') is False,
            'downloaded native Mac proof failed or is foreign')
    a = authority(stage / 'source')
    app_info = a.prepare(output / 'artifact', 'macos-universal', output / 'extracted',
        repository=repo, commit=commit, run_id=run_id, attempt=record['run_attempt'],
        version=ledger['version'], policy=a.load(stage / 'source/release/notice-roots.json'))
    require(original.get('source') == app_info and
            original.get('archive_sha256') == sha(output / 'artifact/install.tar') and
            tree(output / 'extracted/Nuschl.app') == tree(stage / 'install/Nuschl.app'),
            'native-verified CI artifact bytes differ from local Mac product')
    return {'status': 'native_apple_static_verified', 'artifact_binaries_executed': False,
            'source': 'github-native-success', 'commit': commit, 'run_id': run_id,
            'run_attempt': record['run_attempt'], 'artifact_ids': selected,
            'ledger_sha256': sha(stage / 'ledger.json'),
            'archive_sha256': ledger['macos_archive_sha256'], 'host': original['host'],
            'commands': original['commands'], 'ci_report_sha256': sha(output / 'report/report.json')}


def import_ci_macos(args):
    stage = args.stage.resolve(strict=True)
    ledger = validate(stage)
    output = stage / 'macos-ci'
    output.mkdir(exist_ok=False)
    report = ci_evidence(stage, ledger, args.gh, args.run_id, output)
    (stage / 'macos-native').mkdir(exist_ok=False)
    write(stage / 'macos-native/report.json', report)
    print('Same-commit successful native CI proof matches every local Mac file; no signing key accessed.')


def mac_gate(stage, ledger):
    report = load(stage / 'macos-native/report.json')
    require(report.get('status') == 'native_apple_static_verified' and
            report.get('artifact_binaries_executed') is False and
            report.get('commit') == ledger['commit'] and
            report.get('ledger_sha256') == sha(stage / 'ledger.json') and
            report.get('archive_sha256') == ledger['macos_archive_sha256'],
            'exact-stage native Apple verification is missing/failed; Linux prepare-only cannot satisfy it')
    require(report.get('source') in ('local-native-operator-controlled', 'github-native-success'),
            'unknown native evidence source')
    require(report.get('host', {}).get('platform') == 'darwin' and report.get('commands'),
            'native tool evidence missing')
    return sha(stage / 'macos-native/report.json')


def verify_signature(stage, signer):
    run([signer, 'verify', '--public-file', stage / 'source/crates/update/update-trust.json',
         '--manifest', stage / 'dist/update.json', '--signature-file', stage / 'dist/update.json.sig'])


def checksum_text(dist):
    return ''.join(sha(path) + '  ' + path.name + '\n'
                   for path in sorted(dist.iterdir()) if path.name != 'SHA256SUMS')


def sign(args):
    stage = args.stage.resolve(strict=True)
    ledger = validate(stage)
    native = mac_gate(stage, ledger)
    require(not (stage / 'dist/update.json.sig').exists(), 'already signed; never overwrite staged signatures')
    public = stage / 'source/crates/update/update-trust.json'
    run([args.signer, 'key-status', '--secret-file', args.sign_key, '--public-file', public])
    manifest = load(stage / 'dist/update.json')
    manifest['notes'] += '\nnative-macos-report-sha256=' + native
    (stage / 'dist/update.json').write_text(json.dumps(manifest, indent=1) + '\n')
    run([args.signer, 'sign', '--secret-file', args.sign_key, '--public-file', public,
         '--manifest', stage / 'dist/update.json', '--signature-file', stage / 'dist/update.json.sig'])
    verify_signature(stage, args.signer)
    (stage / 'dist/SHA256SUMS').write_text(checksum_text(stage / 'dist'))
    print('Local hybrid signature and both-component verification passed; nothing uploaded.')


def publish(args):
    stage = args.stage.resolve(strict=True)
    ledger = validate(stage)
    native = mac_gate(stage, ledger)
    require('native-macos-report-sha256=' + native in load(stage / 'dist/update.json')['notes'].splitlines(),
            'native report differs from signed binding')
    verify_signature(stage, args.signer)
    native_report = load(stage / 'macos-native/report.json')
    if native_report['source'] == 'github-native-success':
        with tempfile.TemporaryDirectory(prefix='release-ci-recheck-') as tmp:
            fresh = ci_evidence(stage, ledger, args.gh, native_report['run_id'], Path(tmp))
            require(fresh == native_report, 'CI evidence changed since local signing')
    expected = {p.name: {'size': p.stat().st_size, 'digest': 'sha256:' + sha(p)}
                for p in (stage / 'dist').iterdir()}
    require((stage / 'dist/SHA256SUMS').read_text() == checksum_text(stage / 'dist'),
            'manual download checksums differ from final signed payload')
    # No "latest" indirection, tag creation, replacement or clobber. A failed
    # upload remains a draft; retry is deliberately manual, never destructive.
    repo, tag = ledger['repository'], ledger['tag']
    ref = json.loads(run([args.gh, 'api', f'repos/{repo}/git/ref/tags/{tag}']))['object']
    if ref['type'] == 'tag':
        ref = json.loads(run([args.gh, 'api', f'repos/{repo}/git/tags/{ref["sha"]}']))['object']
    require(ref['type'] == 'commit' and ref['sha'] == ledger['commit'], 'remote immutable tag differs')
    releases = json.loads(run([args.gh, 'api', '--paginate', '--slurp', f'repos/{repo}/releases?per_page=100']))
    require(not any(r['tag_name'] == tag for page in releases for r in page),
            'release/draft already exists; never overwrite or resume unknown authority')
    notes = stage / 'source/release' / ('notes-' + ledger['version'] + '.md')
    require(notes.is_file(), 'tagged release notes missing')
    run([args.gh, 'release', 'create', tag, '--repo', repo, '--verify-tag', '--draft',
         '--title', 'Nuschl ' + ledger['version'], '--notes-file', notes,
         *sorted((stage / 'dist').iterdir())], timeout=1800)
    drafts = json.loads(run([args.gh, 'api', '--paginate', '--slurp', f'repos/{repo}/releases?per_page=100']))
    matches = [r for page in drafts for r in page if r['tag_name'] == tag]
    require(len(matches) == 1 and matches[0]['draft'] is True, 'created draft missing/ambiguous')
    remote = json.loads(run([args.gh, 'api', f'repos/{repo}/releases/{matches[0]["id"]}']))
    require(remote['draft'] is True and len(remote['assets']) == len(expected), 'draft assets incomplete')
    require(all(a.get('state') == 'uploaded' for a in remote['assets']) and
            {a['name']: {'size': a['size'], 'digest': a.get('digest')} for a in remote['assets']} == expected,
            'uploaded draft bytes differ; remains unpublished')
    # Last local check before making the complete draft public.
    validate(stage)
    verify_signature(stage, args.signer)
    require(expected == {p.name: {'size': p.stat().st_size, 'digest': 'sha256:' + sha(p)}
                         for p in (stage / 'dist').iterdir()}, 'local bytes changed during upload')
    final_remote = json.loads(run([args.gh, 'api', f'repos/{repo}/releases/{remote["id"]}']))
    require(final_remote == remote, 'remote draft changed during final validation')
    run([args.gh, 'release', 'edit', tag, '--repo', repo, '--draft=false'])
    print('Published the complete pre-signed release; no key was uploaded.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('prepare')
    p.add_argument('--source-repo', type=Path, default=ROOT)
    p.add_argument('--tag', required=True)
    p.add_argument('--repository', required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--build', action='store_true', help='build missing exact-tag outputs; may be expensive')
    for command in ('verify-macos', 'import-ci-macos', 'sign', 'publish'):
        p = sub.add_parser(command)
        p.add_argument('stage', type=Path)
        if command in ('sign', 'publish'):
            p.add_argument('--signer', required=True)
        if command == 'sign':
            p.add_argument('--sign-key', type=Path, required=True)
        if command in ('publish', 'import-ci-macos'):
            p.add_argument('--gh', default='gh')
        if command == 'import-ci-macos':
            p.add_argument('--run-id', required=True)
    args = parser.parse_args()
    {'prepare': prepare, 'verify-macos': verify_macos, 'import-ci-macos': import_ci_macos, 'sign': sign, 'publish': publish}[args.command](args)


if __name__ == '__main__':
    main()
