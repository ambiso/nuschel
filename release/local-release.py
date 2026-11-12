#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Local, exact-tag preparation/signing; separate explicit draft-first publication.

Never generates/enrolls keys. The operator supplies a local secret path to sign;
GitHub receives only already-signed assets. Local receipts are operator-controlled
records, not remote attestation or a proof of reproducible compilation.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
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
    spec = importlib.util.spec_from_file_location(path.stem.replace('-', '_'), path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def require(value, reason):
    if not value:
        raise ValueError(reason)


def run(args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, capture_output=True, text=True,
                          timeout=kwargs.pop('timeout', 300), **kwargs).stdout


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        while block := stream.read(1024 * 1024):
            h.update(block)
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
    for path in sorted(root.rglob('*')):
        require(not path.is_symlink(), 'links are not distributable')
        if path.is_dir():
            continue
        require(path.is_file(), 'special file in payload')
        name = path.relative_to(root).as_posix()
        require(not any(x in ('', '.', '..') for x in name.split('/')) and
                '\\' not in name and len(name) <= 512 and name.casefold() not in names,
                'unsafe or case-aliased payload path')
        names.add(name.casefold())
        total += path.stat().st_size
        require(len(result) < files and path.stat().st_size <= 512 * 1024**2 and total <= size,
                'payload count/byte budget exceeded')
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
        expression = f'{flake}#packages.x86_64-linux.{name}'
        selected = json.loads(run(['nix', 'eval', '--json', expression, '--apply',
                                  'p: {drv=p.drvPath; output=p.outPath;}']))
        if args.build:
            guarded_build(['nix', 'build', '--cores', '2', '--max-jobs', '2', '--no-link',
                           flake + '#' + name], stage / (name + '-build.log'))
        raw = json.loads(run(['nix', 'path-info', '--json', selected['output']]))
        info = raw[0] if isinstance(raw, list) else raw[selected['output']]
        require(Path(selected['output']).is_dir() and info.get('narHash', '').startswith('sha256-'),
                'selected Nix output is not built/has no NAR receipt; use --build')
        products[name] = {**selected, 'narHash': info['narHash'], 'source_kind': 'local-nix'}
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
              'repository': args.repository, 'source_repository': str(repository), 'flake': flake,
              'publisher_tool_sha256': sha(Path(__file__)), 'products': products,
              'source_tree': tree(source), 'installations': tree(install), 'macos_archive_sha256': sha(stage / 'macos-install.tar'),
              'assets': payloads, 'source_kind': 'local-nix; not GitHub CI provenance'}
    write(stage / 'ledger.json', ledger)
    raw = load(dist / 'update.json')
    raw['notes'] = f'https://github.com/{args.repository}/releases/tag/{args.tag}\nledger-sha256={sha(stage / "ledger.json")}'
    (dist / 'update.json').write_text(json.dumps(raw, indent=1) + '\n')
    print(f'Prepared {stage}; unsigned. Native Mac verification and local signing remain required.')


def validate(stage):
    ledger = load(stage / 'ledger.json')
    require(ledger['format'] == 1 and ledger['tag'] == 'v' + ledger['version'], 'invalid ledger')
    repo = Path(ledger['source_repository'])
    require(revision(repo, ledger['tag']) == ledger['commit'], 'immutable selected tag moved')
    require(tree(stage / 'source') == ledger['source_tree'], 'source bytes differ from prepared ledger')
    snapshot(repo, ledger['commit'], stage / 'source')
    require(tree(stage / 'install') == ledger['installations'], 'installation changed after preparation')
    require(sha(stage / 'macos-install.tar') == ledger['macos_archive_sha256'], 'Mac archive changed')
    actual = tree(stage / 'dist')
    for name in ('update.json', 'update.json.sig', 'SHA256SUMS'):
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


def mac_gate(stage, ledger):
    report = load(stage / 'macos-native/report.json')
    require(report.get('status') == 'native_apple_static_verified' and
            report.get('artifact_binaries_executed') is False and
            report.get('commit') == ledger['commit'] and
            report.get('ledger_sha256') == sha(stage / 'ledger.json') and
            report.get('archive_sha256') == ledger['macos_archive_sha256'],
            'exact-stage native Apple verification is missing/failed; Linux prepare-only cannot satisfy it')
    require(report.get('source') == 'local-native-operator-controlled', 'unknown native evidence source')
    require(report.get('host', {}).get('platform') == 'darwin' and report.get('commands'),
            'native tool evidence missing')
    return sha(stage / 'macos-native/report.json')


def verify_signature(stage, signer):
    run([signer, 'verify', '--public-file', stage / 'source/crates/update/update-trust.json',
         '--manifest', stage / 'dist/update.json', '--signature-file', stage / 'dist/update.json.sig'])


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
    (stage / 'dist/SHA256SUMS').write_text(''.join(sha(path) + '  ' + path.name + '\n'
        for path in sorted((stage / 'dist').iterdir()) if path.name != 'SHA256SUMS'))
    print('Local hybrid signature and both-component verification passed; nothing uploaded.')


def publish(args):
    stage = args.stage.resolve(strict=True)
    ledger = validate(stage)
    native = mac_gate(stage, ledger)
    require('native-macos-report-sha256=' + native in load(stage / 'dist/update.json')['notes'].splitlines(),
            'native report differs from signed binding')
    verify_signature(stage, args.signer)
    expected = {p.name: {'size': p.stat().st_size, 'digest': 'sha256:' + sha(p)}
                for p in (stage / 'dist').iterdir()}
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
    remote = json.loads(run([args.gh, 'api', f'repos/{repo}/releases/tags/{tag}']))
    require(remote['draft'] is True and len(remote['assets']) == len(expected), 'draft assets incomplete')
    require({a['name']: {'size': a['size'], 'digest': a.get('digest')} for a in remote['assets']} == expected,
            'uploaded draft bytes differ; remains unpublished')
    # Last local check before making the complete draft public.
    validate(stage)
    verify_signature(stage, args.signer)
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
    for command in ('verify-macos', 'sign', 'publish'):
        p = sub.add_parser(command)
        p.add_argument('stage', type=Path)
        if command in ('sign', 'publish'):
            p.add_argument('--signer', required=True)
        if command == 'sign':
            p.add_argument('--sign-key', type=Path, required=True)
        if command == 'publish':
            p.add_argument('--gh', default='gh')
    args = parser.parse_args()
    {'prepare': prepare, 'verify-macos': verify_macos, 'sign': sign, 'publish': publish}[args.command](args)


if __name__ == '__main__':
    main()
