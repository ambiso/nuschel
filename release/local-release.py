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
import resource
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


def archive_tree(path, expected, prefix=None, *, zipped=False, source_root=None):
    # Never parse mutable release archives. Both producers are deterministic;
    # reconstruct from store-matched inputs and compare exact bytes by digest.
    require(source_root is not None and tree(source_root) == expected,
            'canonical archive needs its exact selected input tree')
    with tempfile.TemporaryDirectory(prefix='release-archive-check-') as temporary:
        canonical = Path(temporary) / ('expected.zip' if zipped else 'expected.tar')
        (canonical_zip if zipped else canonical_tar)(source_root, canonical, prefix)
        require(sha(path) == sha(canonical), 'archive differs from canonical selected product')


def nix_instructions(repository, commit):
    return (f'Linux requires Nix; no portable Linux binary is supplied.\n'
            f'nix run github:{repository}/{commit}\n'
            f'nix build github:{repository}/{commit}#server\n')


def check_products(stage, ledger, repository):
    flake = 'git+' + repository.resolve().as_uri() + '?rev=' + ledger['commit']
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
    archive_tree(stage / 'macos-install.tar', mac, 'Nuschl.app',
                 source_root=stage / 'install/Nuschl.app')
    archive_tree(stage / f'dist/nuschl-{version}-macos-universal.zip', mac, 'Nuschl.app', zipped=True,
                 source_root=stage / 'install/Nuschl.app')
    archive_tree(stage / f'dist/nuschl-{version}-windows-x86_64.zip', mapped, zipped=True, source_root=stage / 'install/windows')
    archive_tree(stage / f'dist/nuschl-{version}-web-site.tar',
                 tree(Path(expected['web-site']['output']), size=512 * 1024**2),
                 source_root=Path(expected['web-site']['output']))
    require(load(stage / f'dist/nuschl-{version}-linux-nix.json') == expected,
            'Linux build references differ from actual selected outputs')
    require(sha(stage / 'dist/flake.lock') == sha(stage / 'source/flake.lock'), 'foreign published lockfile')
    require((stage / 'dist/INSTALL-NIX.txt').read_text() ==
            nix_instructions(ledger['repository'], ledger['commit']), 'Linux installation instructions changed')


def repository_name(value):
    return isinstance(value, str) and re.fullmatch(
        r'[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}', value) is not None


# Release notes: the selected flake's git-cliff renders the exact local range once,
# during prepare, with the tagged cliff.toml. The signed ledger binds the result;
# publication verifies and reuses those frozen bytes and never reruns git-cliff.
VERSION_TAG = r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?'
NOTES = {'tags': 4096, 'commits': 20000, 'config': 64 * 1024, 'curated': 64 * 1024,
         'generated': 120_000, 'body': 125_000,  # GitHub stores at most 125,000 characters.
         'seconds': 120, 'cpu': 60, 'memory': 1024**3, 'file': 1024**2}
CLIFF = {'changelog': {'body': str, 'trim': bool, 'render_always': bool},
         'git': {'conventional_commits': bool, 'filter_unconventional': bool,
                 'require_conventional': bool, 'split_commits': bool,
                 'protect_breaking_commits': bool, 'filter_commits': bool, 'tag_pattern': str,
                 'topo_order': bool, 'topo_order_commits': bool, 'sort_commits': str,
                 'recurse_submodules': bool, 'commit_preprocessors': list, 'commit_parsers': list}}
CLIFF_ITEMS = {'commit_parsers': {'message': str, 'body': str, 'group': str, 'skip': bool,
                                  'scope': str, 'default_scope': str},
               'commit_preprocessors': {'pattern': str, 'replace': str}}  # never replace_command
# Remote metadata, environment, clock, randomness, local paths and diff statistics.
CLIFF_TEMPLATE_DENIED = re.compile(r'\b(github|gitlab|gitea|bitbucket|azure_devops|remote|get_env|now|'
                                   r'get_random|repository|statistics|include|import|extends)\b')
NOTES_LEDGER = {'format', 'generator', 'generator_path', 'previous_tag', 'previous_commit', 'range',
                'commit_count', 'config_sha256', 'curated', 'curated_sha256', 'generated_sha256',
                'notes_sha256'}


def cliff_config(raw):
    """Accept only the reviewed git-cliff subset: no commands, remotes, outputs or headers."""
    require(len(raw) <= NOTES['config'], 'oversized cliff.toml')
    config = tomllib.loads(raw.decode('utf-8'))

    def fields(table, schema, where):
        require(isinstance(table, dict) and set(table) <= set(schema) and
                all(type(table[key]) is schema[key] for key in table),
                'cliff.toml ' + where + ' is outside the reviewed git-cliff subset')
    fields(config, {name: dict for name in CLIFF}, 'top level')
    for name, schema in CLIFF.items():
        fields(config.get(name, {}), schema, name)
    git = config.get('git', {})
    for name, schema in CLIFF_ITEMS.items():
        require(len(git.get(name, [])) <= 256, 'too many cliff.toml ' + name)
        for item in git.get(name, []):
            fields(item, schema, name)
    require(git.get('recurse_submodules', False) is False and
            git.get('sort_commits', 'oldest') in ('oldest', 'newest'), 'unsupported cliff.toml git options')
    body = config.get('changelog', {}).get('body', '')
    require(body.strip() and not CLIFF_TEMPLATE_DENIED.search(body),
            'cliff.toml needs its own template without remote/environment/clock/path/statistics data')
    return config


def history(repository, *args):
    return run(['git', '--no-replace-objects', '-C', repository, *args], timeout=60)


def previous_tag(repository, commit, tag):
    """The unique newest ancestor version tag, or (None, None) for a first release."""
    require(history(repository, 'rev-parse', '--is-shallow-repository').strip() == 'false',
            'release notes need complete, non-shallow Git history')
    names = history(repository, 'for-each-ref', '--merged=' + commit, '--count=' + str(NOTES['tags'] + 1),
                    '--format=%(refname:strip=2)', 'refs/tags/').split()
    require(len(names) <= NOTES['tags'], 'too many tags to select the previous release')
    names = [name for name in names if name != tag and re.fullmatch(VERSION_TAG, name)]
    if not names:
        return None, None
    # Each argument starts with refs/tags/v (VERSION_TAG), so none can be an option.
    peeled = history(repository, 'rev-parse', *('refs/tags/' + name + '^{commit}' for name in names)).split()
    require(len(peeled) == len(names) and all(re.fullmatch('[0-9a-f]{40}', c) for c in peeled),
            'cannot resolve version tags to commits')
    candidates = {name: sha for name, sha in zip(names, peeled) if sha != commit}
    if not candidates:
        return None, None
    heads = history(repository, 'merge-base', '--independent', *sorted(set(candidates.values()))).split()
    require(len(heads) == 1, 'ancestor version tags diverge; the previous release is ambiguous')
    names = sorted(name for name, sha in candidates.items() if sha == heads[0])
    final = [name for name in names if '-' not in name] or names
    require(len(final) == 1, 'several version tags name the previous release commit')
    return final[0], heads[0]


def notes_range(commit, previous_commit):
    return commit if previous_commit is None else previous_commit + '..' + commit


def commit_count(repository, commit, previous_commit):
    count = int(history(repository, 'rev-list', '--count', commit,
                        *(['^' + previous_commit] if previous_commit else [])))
    require(1 <= count <= NOTES['commits'], 'release notes commit range is empty or exceeds its bound')
    return count


def select_git_cliff(flake):
    """The single git-cliff of the selected flake's development shell; PATH is never consulted."""
    selected = json.loads(run(['nix', 'eval', '--json', flake + '#devShells.x86_64-linux.default.nativeBuildInputs',
                               '--apply', 'ps: map (p: {drv = p.drvPath; output = p.outPath;}) (builtins.filter '
                               '(p: builtins.isAttrs p && (p.pname or "") == "git-cliff") ps)']))
    require(isinstance(selected, list) and len(selected) == 1 and isinstance(selected[0], dict) and
            set(selected[0]) == {'drv', 'output'} and
            re.fullmatch(r'/nix/store/[0-9a-z]{32}-git-cliff-[^/]+\.drv', selected[0]['drv']) and
            re.fullmatch(r'/nix/store/[0-9a-z]{32}-git-cliff-[^/]+', selected[0]['output']),
            "the selected flake's development shell must provide exactly one git-cliff")
    return selected[0]


def git_cliff_version(executable, scratch):
    with tempfile.TemporaryDirectory(prefix='version-', dir=scratch) as home:
        result = subprocess.run([str(executable), '--version'], capture_output=True, text=True, timeout=30,
                                stdin=subprocess.DEVNULL, cwd=home, env={'HOME': home, 'TZ': 'UTC', 'LC_ALL': 'C'})
    version = result.stdout.strip()
    require(result.returncode == 0 and re.fullmatch(r'git-cliff [0-9]+\.[0-9]+\.[0-9]+', version),
            'unrecognized git-cliff version')
    return version


def tagged_config(repository, commit):
    """The selected commit's own cliff.toml blob (git archive attributes cannot alter it)."""
    size = history(repository, 'cat-file', '-s', commit + ':cliff.toml').strip()
    require(size.isdigit() and int(size) <= NOTES['config'], 'oversized tagged cliff.toml')
    return subprocess.run(['git', '--no-replace-objects', '-C', str(repository), 'cat-file', 'blob',
                           commit + ':cliff.toml'], check=True, capture_output=True, timeout=60).stdout


def history_view(repository, refs, directory):
    """Private bare view of the source objects with only the named refs; the source is never written."""
    objects = Path(history(repository, 'rev-parse', '--path-format=absolute', '--git-common-dir').strip()) / 'objects'
    require(objects.is_absolute() and objects.is_dir() and '\n' not in str(objects), 'source Git objects unavailable')
    view = directory / 'history.git'
    for name in ('objects/info', 'refs/heads', 'refs/tags'):
        (view / name).mkdir(parents=True)
    (view / 'config').write_text('[core]\n\trepositoryformatversion = 0\n\tbare = true\n')
    (view / 'HEAD').write_text('ref: refs/heads/release\n')
    (view / 'objects/info/alternates').write_text(str(objects) + '\n')
    for ref, sha in refs.items():
        (view / ref).write_text(sha + '\n')
    return view


def run_git_cliff(executable, config, view, revisions, output, log):
    """Fixed argv and empty environment: no remotes, commands, user config or network enrichment."""
    def limits():
        for limit, value in ((resource.RLIMIT_AS, NOTES['memory']), (resource.RLIMIT_CPU, NOTES['cpu']),
                             (resource.RLIMIT_FSIZE, NOTES['file']), (resource.RLIMIT_CORE, 0)):
            resource.setrlimit(limit, (value, value))
    # git-cliff silently falls back to its built-in template when --config is
    # missing, and runs from a private directory: pass only existing absolute paths.
    require(all(Path(p).is_absolute() for p in (executable, config, view)) and config.is_file() and
            view.is_dir(), 'git-cliff inputs must be existing absolute paths')
    home = view.parent / 'home'
    home.mkdir()
    argv = [executable, '--config', config, '--repository', view, '--offline', '--no-exec',
            '--strip', 'all', '--', revisions]
    with output.open('xb') as stdout, log.open('xb') as stderr:
        process = subprocess.Popen(list(map(str, argv)), stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                   cwd=home, env={'HOME': str(home), 'TZ': 'UTC', 'LC_ALL': 'C'},
                                   preexec_fn=limits, start_new_session=True)
        try:
            process.wait(timeout=NOTES['seconds'])
        except subprocess.TimeoutExpired:
            raise ValueError('git-cliff exceeded its wall-time bound') from None
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()  # Reap before reporting; a kill request is not exit.
    require(process.returncode == 0, 'git-cliff failed or exceeded its bounds; see ' + str(log))


def notes_bytes(path, limit):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), 'release notes inputs must be regular files')
        raw = stream.read(limit + 1)
    require(len(raw) <= limit, path.name + ' exceeds its release notes bound')
    return raw


def notes_text(raw, what):
    text = raw.decode('utf-8')
    require(not any((ord(c) < 32 and c not in '\n\t') or 127 <= ord(c) < 160 or
                    0x202a <= ord(c) <= 0x202e or 0x2066 <= ord(c) <= 0x2069 for c in text),
            what + ' contains control or bidirectional override characters')
    return text


def compose_notes(curated, generated):
    parts = [part.strip('\n') for part in (curated, generated) if part.strip()]
    require(parts, 'no tagged release notes and git-cliff found no notable changes')
    body = ('\n\n'.join(parts) + '\n').encode()
    require(len(body) <= NOTES['body'], 'release notes exceed the GitHub release body bound')
    return body


def curated_notes(stage, version):
    relative = 'release/notes-' + version + '.md'
    path = stage / 'source' / relative
    if not path.exists() and not path.is_symlink():
        return None, None, ''
    raw = notes_bytes(path, NOTES['curated'])
    return relative, hashlib.sha256(raw).hexdigest(), notes_text(raw, 'tagged release notes')


def stage_notes(repository, stage, tag, commit, version, flake, build=False):
    """Generate the release body once, before signing; returns its ledger entry."""
    repository, stage = Path(repository).resolve(strict=True), Path(stage).resolve(strict=True)
    notes, source = stage / 'notes', stage / 'source/cliff.toml'
    notes.mkdir(mode=0o700)
    scratch = notes / 'tmp'  # Owned, disk-backed stage scratch; never the default /tmp.
    scratch.mkdir(mode=0o700)
    require(source.is_file() and not source.is_symlink(),
            'the selected tag has no cliff.toml; tags before git-cliff release notes cannot be prepared')
    raw = notes_bytes(source, NOTES['config'])
    require(raw == tagged_config(repository, commit), 'exported cliff.toml differs from the selected tagged source')
    cliff_config(raw)
    (notes / 'cliff.toml').write_bytes(raw)
    previous, previous_commit = previous_tag(repository, commit, tag)
    count = commit_count(repository, commit, previous_commit)
    tool = select_git_cliff(flake)
    executable = Path(tool['output']) / 'bin/git-cliff'
    if build and not executable.exists():
        guarded_build(['nix', 'build', '--cores', '2', '--max-jobs', '2', '--no-link', tool['drv'] + '^out'],
                      stage / 'git-cliff-build.log')
    require(executable.is_file() and os.access(executable, os.X_OK),
            "the selected flake's git-cliff is not built; enter its development shell or prepare with --build")
    generator = git_cliff_version(executable, scratch)
    refs = {'refs/heads/release': commit, 'refs/tags/' + tag: commit}
    if previous:
        refs['refs/tags/' + previous] = previous_commit
    with tempfile.TemporaryDirectory(prefix='history-', dir=scratch) as temporary:
        view = history_view(repository, refs, Path(temporary))
        run_git_cliff(executable, notes / 'cliff.toml', view, notes_range(commit, previous_commit),
                      notes / 'git-cliff.md', notes / 'git-cliff.log')
    scratch.rmdir()
    generated = notes_bytes(notes / 'git-cliff.md', NOTES['generated'])
    relative, curated_sha, curated = curated_notes(stage, version)
    body = compose_notes(curated, notes_text(generated, 'git-cliff output'))
    (notes / 'RELEASE-NOTES.md').write_bytes(body)
    for name in ('cliff.toml', 'git-cliff.md', 'RELEASE-NOTES.md'):
        (notes / name).chmod(0o444)
    return {'format': 1, 'generator': generator, 'generator_path': str(executable),
            'previous_tag': previous, 'previous_commit': previous_commit,
            'range': notes_range(commit, previous_commit), 'commit_count': count,
            'config_sha256': hashlib.sha256(raw).hexdigest(), 'curated': relative,
            'curated_sha256': curated_sha, 'generated_sha256': hashlib.sha256(generated).hexdigest(),
            'notes_sha256': hashlib.sha256(body).hexdigest()}


def check_notes(stage, ledger):
    """Verify, never regenerate, the signed ledger's frozen release body. Returns its path and text."""
    notes, entry = stage / 'notes', ledger['release_notes']
    require(isinstance(entry, dict) and set(entry) == NOTES_LEDGER and type(entry['format']) is int and
            entry['format'] == 1 and type(entry['commit_count']) is int and isinstance(entry['generator'], str) and
            re.fullmatch(r'git-cliff [0-9]+\.[0-9]+\.[0-9]+', entry['generator']) and
            all(isinstance(entry[k], str) and re.fullmatch('[0-9a-f]{64}', entry[k])
                for k in ('config_sha256', 'generated_sha256', 'notes_sha256')),
            'invalid release notes ledger entry; prepare again')
    repository = Path(load(stage / 'local.json')['source_repository']).resolve()
    tool = select_git_cliff('git+' + repository.as_uri() + '?rev=' + ledger['commit'])
    require(entry['generator_path'] == tool['output'] + '/bin/git-cliff',
            "release notes were not generated by the selected flake's git-cliff")
    previous, previous_commit = previous_tag(repository, ledger['commit'], ledger['tag'])
    require(entry['previous_tag'] == previous and entry['previous_commit'] == previous_commit and
            entry['range'] == notes_range(ledger['commit'], previous_commit) and
            entry['commit_count'] == commit_count(repository, ledger['commit'], previous_commit),
            'release notes range changed since preparation; prepare again')
    raw = notes_bytes(notes / 'cliff.toml', NOTES['config'])
    cliff_config(raw)
    generated = notes_bytes(notes / 'git-cliff.md', NOTES['generated'])
    relative, curated_sha, curated = curated_notes(stage, ledger['version'])
    body = notes_bytes(notes / 'RELEASE-NOTES.md', NOTES['body'])
    require(raw == notes_bytes(stage / 'source/cliff.toml', NOTES['config']) and
            hashlib.sha256(raw).hexdigest() == entry['config_sha256'] and
            hashlib.sha256(generated).hexdigest() == entry['generated_sha256'] and
            (relative, curated_sha) == (entry['curated'], entry['curated_sha256']) and
            body == compose_notes(curated, notes_text(generated, 'git-cliff output')) and
            hashlib.sha256(body).hexdigest() == entry['notes_sha256'],
            'frozen release notes or their configuration changed after preparation')
    return notes / 'RELEASE-NOTES.md', body.decode()


def prepare(args):
    repository = args.source_repo.resolve(strict=True)
    commit = revision(repository, args.tag)
    require(repository_name(args.repository), 'invalid repository')
    stage = args.output.resolve()
    stage.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = stage / 'source'
    export(repository, commit, source)
    version = tomllib.loads((source / 'Cargo.toml').read_text())['workspace']['package']['version']
    require(args.tag == 'v' + version, 'tag must match workspace version')
    flake = 'git+' + repository.as_uri() + '?rev=' + commit
    release_notes = stage_notes(repository, stage, args.tag, commit, version, flake, args.build)
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
    # Nix store directories are read-only; this staging root also receives notices.
    windows.chmod(0o755)
    shutil.copytree(Path(products['windows']['output']) / 'share/licenses/nuschl', windows / 'licenses')
    shutil.copytree(Path(products['macos-app']['output']) / 'Nuschl.app', install / 'Nuschl.app')
    check_installs(stage, source)
    a = authority(source)
    policy = a.load(source / 'release/notice-roots.json')
    for product in ('default', 'server'):
        a.verify_notices(Path(products[product]['output']) / 'share/licenses/nuschl', policy, 'linux-nix')
    web = Path(products['web-site']['output'])
    a.verify_notices(web / 'licenses/nuschl', policy, 'web')
    tree(web, size=512 * 1024**2)
    require((web / 'index.html').is_file() and (web / 'app/index.html').is_file(), 'web entry points missing')
    dist = stage / 'dist'
    dist.mkdir()
    canonical_tar(install / 'Nuschl.app', stage / 'macos-install.tar', 'Nuschl.app')
    canonical_tar(web, dist / f'nuschl-{version}-web-site.tar')
    canonical_zip(windows, dist / f'nuschl-{version}-windows-x86_64.zip')
    canonical_zip(install / 'Nuschl.app', dist / f'nuschl-{version}-macos-universal.zip', 'Nuschl.app')
    write(dist / f'nuschl-{version}-linux-nix.json', products)
    shutil.copyfile(source / 'flake.lock', dist / 'flake.lock')
    (dist / 'INSTALL-NIX.txt').write_text(nix_instructions(args.repository, commit))
    base = f'https://github.com/{args.repository}/releases/download/{args.tag}/'
    # The producer owns all updater naming/path/URL/size rules.
    run([sys.executable, source / 'release/make-update.py', '--version', version, '--base-url', base,
         '--platform', f'windows-x86_64={windows}', '--platform', f'macos-universal={install / "Nuschl.app"}',
         '--output', dist])
    payloads = tree(dist)
    del payloads['update.json']
    ledger = {'format': 2, 'tag': args.tag, 'version': version, 'commit': commit,
              'repository': args.repository, 'source_kind_detail': 'exact local Git revision; no CI claim',
              'publisher_tool_sha256': sha(Path(__file__)), 'products': products,
              'source_tree': tree(source), 'installations': tree(install), 'macos_archive_sha256': sha(stage / 'macos-install.tar'),
              'assets': payloads, 'source_kind': 'local-nix; not GitHub CI provenance',
              'release_notes': release_notes}
    write(stage / 'ledger.json', ledger)
    write(stage / 'local.json', {'source_repository': str(repository), 'flake': flake})
    write(dist / 'macos-verification.json', verification_record(stage, ledger))
    shutil.copyfile(stage / 'ledger.json', dist / 'release-ledger.json')
    raw = load(dist / 'update.json')
    raw['notes'] = f'https://github.com/{args.repository}/releases/tag/{args.tag}\nledger-sha256={sha(stage / "ledger.json")}'
    (dist / 'update.json').write_text(json.dumps(raw, indent=1) + '\n')
    print(f'Prepared {stage}; unsigned, local Mac integrity checked. Native Mac verification is optional.')


def validate(stage):
    ledger = load(stage / 'ledger.json')
    require(set(ledger) == {'format', 'tag', 'version', 'commit', 'repository',
        'source_kind_detail', 'publisher_tool_sha256', 'products', 'source_tree',
        'installations', 'macos_archive_sha256', 'assets', 'source_kind', 'release_notes'} and
        type(ledger['format']) is int and ledger['format'] == 2 and
        isinstance(ledger['version'], str) and len(ledger['version']) <= 100 and
        repository_name(ledger['repository']) and
        ledger['tag'] == 'v' + ledger['version'], 'invalid ledger/source repository')
    repo = Path(load(stage / 'local.json')['source_repository'])
    require(revision(repo, ledger['tag']) == ledger['commit'], 'immutable selected tag moved')
    require(tree(stage / 'source') == ledger['source_tree'], 'source bytes differ from prepared ledger')
    snapshot(repo, ledger['commit'], stage / 'source')
    require(tree(stage / 'install') == ledger['installations'], 'installation changed after preparation')
    require(sha(stage / 'macos-install.tar') == ledger['macos_archive_sha256'], 'Mac archive changed')
    actual = tree(stage / 'dist')
    require(sha(stage / 'dist/release-ledger.json') == sha(stage / 'ledger.json'), 'public ledger differs')
    for name in ('update.json', 'update.json.sig', 'SHA256SUMS', 'release-ledger.json', 'macos-verification.json'):
        actual.pop(name, None)
    require(actual == ledger['assets'], 'release payload changed or assets added/removed')
    manifest = load(stage / 'dist/update.json')
    require(manifest['version'] == ledger['version'] and
            f'ledger-sha256={sha(stage / "ledger.json")}' in manifest.get('notes', '').splitlines(),
            'signed source ledger binding absent')
    expected, updater_assets = {}, set()
    for platform, relative in [('windows-x86_64', 'windows'), ('macos-universal', 'Nuschl.app')]:
        expected[platform] = []
        for name, path in module(stage / 'source/release/make-update.py').files_of(stage / 'install' / relative):
            asset = platform + '--' + name.replace('/', '__')
            updater_assets.add(asset)
            expected[platform].append({'path': name, 'url': f'https://github.com/{ledger["repository"]}/releases/download/{ledger["tag"]}/{asset}',
                                       'size': path.stat().st_size, 'sha256': sha(path)})
            require(sha(stage / 'dist' / asset) == sha(path), 'updater asset differs from installation')
    allowed = updater_assets | {f'nuschl-{ledger["version"]}-{suffix}' for suffix in
        ('windows-x86_64.zip', 'macos-universal.zip', 'web-site.tar', 'linux-nix.json')} | {'flake.lock', 'INSTALL-NIX.txt'}
    require(set(actual) == allowed, 'unexpected/missing release asset outside source-selected product set')
    require(manifest['platforms'] == expected, 'manifest file set/hash/URLs changed')
    check_installs(stage, stage / 'source')
    check_products(stage, ledger, repo)
    record = load(stage / 'dist/macos-verification.json')
    require(set(record) == {'format', 'native_macos_verified', 'native_report_sha256', 'integrity'} and
            record['format'] == 1 and type(record['native_macos_verified']) is bool and
            record['integrity'] == mac_integrity(stage / 'install/Nuschl.app'),
            'local Mac integrity record changed')
    if record['native_macos_verified']:
        require(record['native_report_sha256'] == mac_gate(stage, ledger) and
                record['native_report_sha256'] is not None, 'unsupported native Mac claim')
    else:
        require(record['native_report_sha256'] is None, 'inconsistent native Mac status')
    check_notes(stage, ledger)
    return ledger


def mac_integrity(app):
    """Pinned locally produced ad-hoc format; NOT native Apple acceptance."""
    parser = module(ROOT / 'tests/cross_macho.py')
    info = (app / 'Contents/Info.plist').read_bytes()
    resources_path = app / 'Contents/_CodeSignature/CodeResources'
    require(resources_path.stat().st_size <= 8 * 1024**2, 'oversized CodeResources')
    resources = resources_path.read_bytes()
    records = plistlib.loads(resources)
    require(isinstance(records, dict) and isinstance(records.get('files2'), dict) and
            len(records['files2']) <= 4096, 'unsupported resource envelope')
    checked = set()
    for name, record in records['files2'].items():
        if isinstance(record, dict) and 'hash2' in record:
            relative = authority(ROOT).checked_relative(name)
            file = app / 'Contents' / relative
            require(file.is_file() and not file.is_symlink() and
                    bytes.fromhex(sha(file)) == record['hash2'], 'bundle resource digest mismatch')
            checked.add(name)
    require(all('Resources/' + name in checked for name in tree(app / 'Contents/Resources')),
            'bundle resource envelope does not cover every resource')
    programs = {}
    for name in PROGRAMS:
        binary = app / 'Contents/MacOS' / name
        require(binary.stat().st_size <= 512 * 1024**2, 'oversized Mach-O')
        programs[name] = parser.verify_adhoc(binary.read_bytes(),
            'org.nuschl.Nuschl' if name == 'nuschl' else name,
            info=info if name == 'nuschl' else None,
            resources=resources if name == 'nuschl' else None)
    return {'format': 1, 'scope': 'local structural/ad-hoc integrity only', 'programs': programs,
            'all_resources_covered': True,
            'limits': ['Not Apple codesign/Gatekeeper acceptance or publisher identity',
                       'No native macOS runtime, Seatbelt, audio or updater proof',
                       'No Developer ID/notarization; experimental Mac distribution']}


def verification_record(stage, ledger):
    native = mac_gate(stage, ledger)
    return {'format': 1, 'native_macos_verified': native is not None,
            'native_report_sha256': native,
            'integrity': mac_integrity(stage / 'install/Nuschl.app')}


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
    path = stage / 'macos-native/report.json'
    if not path.exists() and not path.is_symlink():
        return None
    report = load(path)
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
    verification = verification_record(stage, ledger)
    require(not (stage / 'dist/update.json.sig').exists(), 'already signed; never overwrite staged signatures')
    public = stage / 'source/crates/update/update-trust.json'
    run([args.signer, 'key-status', '--secret-file', args.sign_key, '--public-file', public])
    manifest = load(stage / 'dist/update.json')
    (stage / 'dist/macos-verification.json').write_text(json.dumps(verification, indent=2, sort_keys=True) + '\n')
    manifest['notes'] += ('\nmacos-verification-sha256=' + sha(stage / 'dist/macos-verification.json') +
                          '\nnative_macos_verified=' + str(verification['native_macos_verified']).lower())
    (stage / 'dist/update.json').write_text(json.dumps(manifest, indent=1) + '\n')
    run([args.signer, 'sign', '--secret-file', args.sign_key, '--public-file', public,
         '--manifest', stage / 'dist/update.json', '--signature-file', stage / 'dist/update.json.sig'])
    verify_signature(stage, args.signer)
    (stage / 'dist/SHA256SUMS').write_text(checksum_text(stage / 'dist'))
    print('Local hybrid signature and both-component verification passed; nothing uploaded.')


def publish(args):
    stage = args.stage.resolve(strict=True)
    ledger = validate(stage)
    verification = verification_record(stage, ledger)
    notes = load(stage / 'dist/update.json')['notes'].splitlines()
    require(load(stage / 'dist/macos-verification.json') == verification and
            'macos-verification-sha256=' + sha(stage / 'dist/macos-verification.json') in notes and
            'native_macos_verified=' + str(verification['native_macos_verified']).lower() in notes,
            'Mac verification status differs from signed binding')
    verify_signature(stage, args.signer)
    # The release body is the notes frozen at prepare time; never regenerated here.
    notes_file, notes_body = check_notes(stage, ledger)
    native_report = load(stage / 'macos-native/report.json') if verification['native_macos_verified'] else None
    if native_report and native_report['source'] == 'github-native-success':
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
    run([args.gh, 'release', 'create', tag, '--repo', repo, '--verify-tag', '--draft',
         '--title', 'Nuschl ' + ledger['version'], '--notes-file', notes_file,
         *sorted((stage / 'dist').iterdir())], timeout=1800)
    drafts = json.loads(run([args.gh, 'api', '--paginate', '--slurp', f'repos/{repo}/releases?per_page=100']))
    matches = [r for page in drafts for r in page if r['tag_name'] == tag]
    require(len(matches) == 1 and matches[0]['draft'] is True, 'created draft missing/ambiguous')
    remote = json.loads(run([args.gh, 'api', f'repos/{repo}/releases/{matches[0]["id"]}']))
    require(remote['draft'] is True and len(remote['assets']) == len(expected), 'draft assets incomplete')
    require(all(a.get('state') == 'uploaded' for a in remote['assets']) and
            {a['name']: {'size': a['size'], 'digest': a.get('digest')} for a in remote['assets']} == expected,
            'uploaded draft bytes differ; remains unpublished')
    require(isinstance(remote.get('body'), str) and
            remote['body'].replace('\r\n', '\n').strip() == notes_body.strip(),
            'draft release notes differ from the frozen notes; remains unpublished')
    # Last local check before making the complete draft public.
    validate(stage)
    verify_signature(stage, args.signer)
    require(check_notes(stage, ledger)[1] == notes_body, 'frozen release notes changed during upload')
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
