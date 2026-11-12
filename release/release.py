#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""One-command local release preparation, then publication of the same signed stage.

The audited local-release.py owns product, signature and publication policy. This
frontend only infers operator inputs, confirms intent and remembers a signed stage.
"""
import argparse
from contextlib import contextmanager
import curses
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import textwrap
import types

ROOT = Path(__file__).resolve().parents[1]


def load_backend():
    path = ROOT / 'release/local-release.py'
    result = types.ModuleType('local_release')
    result.__file__ = str(path)
    exec(compile(path.read_bytes(), str(path), 'exec'), result.__dict__)
    return result


backend = load_backend()
require = backend.require


def git(root, *args, optional=False):
    result = subprocess.run(['git', '-C', str(root), *args], capture_output=True,
                            text=True, timeout=30)
    if optional and result.returncode == 1:
        return ''
    require(result.returncode == 0, 'Git inspection failed; use a committed checkout with an origin remote.')
    return result.stdout.strip()


def github_repository(url):
    for pattern in (r'git@github\.com:([^/]+/[^/]+)',
                    r'ssh://git@github\.com/([^/]+/[^/]+)',
                    r'https://github\.com/([^/]+/[^/]+)'):
        if match := re.fullmatch(pattern, url):
            name = match[1].removesuffix('.git')
            require(backend.repository_name(name), 'Invalid GitHub origin repository.')
            return name
    raise ValueError('Set origin to one unambiguous GitHub SSH or HTTPS repository URL.')


def infer(root, *, publish=False):
    require(sys.platform == 'linux' and os.uname().machine == 'x86_64',
            'Local cross-platform release builds currently require x86_64 Linux.')
    require(not git(root, 'status', '--porcelain=v1', '--untracked-files=all'),
            'Checkout has uncommitted changes; commit them before running just release.')
    urls = git(root, 'config', '--get-all', 'remote.origin.url', optional=True).splitlines()
    require(len(urls) == 1, 'Set exactly one origin URL before running just release.')
    repository = github_repository(urls[0])
    pushes = git(root, 'config', '--get-all', 'remote.origin.pushurl', optional=True).splitlines()
    require(not pushes or len(pushes) == 1 and github_repository(pushes[0]) == repository,
            'Origin fetch and push repositories differ; make them agree before releasing.')
    head = git(root, 'rev-parse', '--verify', 'HEAD^{commit}')
    import tomllib
    version = tomllib.loads(git(root, 'show', head + ':Cargo.toml'))['workspace']['package']['version']
    require(isinstance(version, str) and re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?', version),
            'A release workspace version is required in Cargo.toml.')
    tag = 'v' + version
    # show-ref --quiet has no stdout, so inspect its exit status explicitly.
    result = subprocess.run(['git', '-C', str(root), 'show-ref', '--verify', '--quiet', 'refs/tags/' + tag],
                            capture_output=True, timeout=30)
    require(result.returncode in (0, 1), 'Cannot inspect the local release tag.')
    exists = result.returncode == 0
    require(exists or not publish, 'No local version tag; run just release to prepare it first.')
    commit = backend.revision(root, tag) if exists else head
    tagged_version = tomllib.loads(git(root, 'show', commit + ':Cargo.toml'))['workspace']['package']['version']
    require(tagged_version == version, 'The existing immutable tag has a different version; never move it.')
    public_raw = git(root, 'show', commit + ':crates/update/update-trust.json').encode()
    require(len(public_raw) <= 32 * 1024, 'Tagged public key document is oversized.')
    public = json.loads(public_raw, object_pairs_hook=backend.unique)
    require(isinstance(public, dict) and public.get('format') == 2 and public.get('algorithm') == 'ed25519+mldsa44' and
            isinstance(public.get('keys'), list) and len(public['keys']) == 1 and
            isinstance(public['keys'][0], dict) and isinstance(public['keys'][0].get('key_id'), str) and
            re.fullmatch('[0-9a-f]{64}', public['keys'][0].get('key_id', '')),
            'The tagged source must select one hybrid public signing pair.')
    return {'repository': repository, 'version': version, 'tag': tag, 'commit': commit,
            'head': head, 'create_tag': not exists, 'key_id': public['keys'][0]['key_id'],
            'secret': str(Path.home() / '.local/share/nuschl-release-keys/update-v2.secret.json')}


def cache_root():
    base = Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache')))
    require(base.is_absolute(), 'XDG_CACHE_HOME must be an absolute disk-backed directory.')
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    base = base.resolve(strict=True)
    require(not base.is_relative_to('/tmp') and not base.is_relative_to('/dev/shm'),
            'Use a disk-backed XDG_CACHE_HOME, outside /tmp and /dev/shm.')
    # A custom cache can also be on tmpfs somewhere else. Read mount metadata only.
    mounts = []
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        before, after = line.split(' - ', 1)
        mount = re.sub(r'\\([0-7]{3})', lambda m: chr(int(m[1], 8)), before.split()[4])
        if base.is_relative_to(mount):
            mounts.append((len(mount), after.split()[0]))
    require(mounts and max(mounts)[1] not in ('tmpfs', 'ramfs'), 'Release cache must be disk-backed.')
    root = base / 'nuschl-release'
    root.mkdir(mode=0o700, exist_ok=True)
    private_directory(root)
    return root


def private_directory(path):
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o077,
            'Release staging directory must be owned by you, not linked, and mode 0700.')


@contextmanager
def locked(root):
    fd = os.open(root / 'command.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        require(stat.S_ISREG(os.fstat(fd).st_mode), 'Regular release lock required.')
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ValueError('Another just release is running; wait for it to finish.') from error
        yield
    finally:
        os.close(fd)


@contextmanager
def scratch(area):
    directory = area / 'tmp'
    directory.mkdir(mode=0o700, exist_ok=True)
    previous, cached = os.environ.get('TMPDIR'), tempfile.tempdir
    os.environ['TMPDIR'], tempfile.tempdir = str(directory), None
    try:
        yield
    finally:
        tempfile.tempdir = cached
        if previous is None:
            os.environ.pop('TMPDIR', None)
        else:
            os.environ['TMPDIR'] = previous


def confirm_modal(details):
    require(sys.stdin.isatty() and sys.stdout.isatty(),
            'Confirmation needs an interactive terminal; run just release directly in your terminal.')

    def dialog(screen):
        curses.curs_set(0)
        selected, offset = False, 0
        while True:
            height, width = screen.getmaxyx()
            require(height >= 14 and width >= 60, 'Enlarge the terminal to at least 60 columns by 14 rows.')
            width, height = min(width - 2, 100), height - 2
            top, left = 1, (screen.getmaxyx()[1] - width) // 2
            box = curses.newwin(height, width, top, left)
            box.keypad(True)
            box.border()
            box.addstr(0, 2, ' Confirm Nuschl release ')
            lines = [line for detail in details for line in textwrap.wrap(
                ''.join(c if c.isprintable() else repr(c)[1:-1] for c in detail), width - 6) or ['']]
            available = height - 6
            offset = min(offset, max(0, len(lines) - available))
            for row, line in enumerate(lines[offset:offset + available], 2):
                box.addstr(row, 3, line)
            box.addstr(height - 3, 3, '[ Yes ]' if selected else '  Yes  ', curses.A_REVERSE if selected else 0)
            box.addstr(height - 3, 15, '  No   ' if selected else '[ No ]', 0 if selected else curses.A_REVERSE)
            box.addstr(height - 2, 3, 'Tab: choose  Enter: confirm  PgUp/PgDn: scroll')
            box.refresh()
            key = box.getch()
            if key in (ord('\t'), curses.KEY_LEFT, curses.KEY_RIGHT):
                selected = not selected
            elif key in (curses.KEY_PPAGE, curses.KEY_UP):
                offset = max(0, offset - (available if key == curses.KEY_PPAGE else 1))
            elif key in (curses.KEY_NPAGE, curses.KEY_DOWN):
                offset += available if key == curses.KEY_NPAGE else 1
            elif key in (10, 13, curses.KEY_ENTER):
                return selected
            elif key in (-1, 4, 27, ord('q'), ord('n')):
                return False
    return curses.wrapper(dialog)


def build_signer(selection, area):
    flake = 'git+' + ROOT.as_uri() + '?rev=' + selection['commit']
    backend.guarded_build(['nix', 'build', '--cores', '2', '--max-jobs', '2',
                           '--out-link', str(area / 'signer'), flake + '#update-signer'],
                          area / 'signer-build.log')
    return backend.select_product(flake, 'update-signer')


def signer_path(product):
    path = Path(product['output']) / 'bin/nuschl-update-sign'
    require(path.is_file() and os.access(path, os.X_OK), 'The exact-tag signer is unavailable; prepare again.')
    return path


def signed_identity(stage):
    return {name: backend.sha(stage / name) for name in
            ('ledger.json', 'dist/update.json', 'dist/update.json.sig', 'dist/SHA256SUMS')}


def remember(root, record):
    fd, name = tempfile.mkstemp(prefix='.last-signed-', dir=root)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(record, stream, indent=2, sort_keys=True)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, root / 'last-signed.json')
    finally:
        if os.path.exists(name):
            os.unlink(name)


def verify_stage(stage, signer):
    ledger = backend.validate(stage)
    verification = backend.verification_record(stage, ledger)
    notes = backend.load(stage / 'dist/update.json')['notes'].splitlines()
    require(backend.load(stage / 'dist/macos-verification.json') == verification and
            'macos-verification-sha256=' + backend.sha(stage / 'dist/macos-verification.json') in notes and
            'native_macos_verified=' + str(verification['native_macos_verified']).lower() in notes,
            'Mac verification differs from the signed release.')
    backend.verify_signature(stage, signer)
    require((stage / 'dist/SHA256SUMS').read_text() == backend.checksum_text(stage / 'dist'),
            'Signed release checksums changed.')
    return ledger


def execute(publish=False, *, confirm=None):
    selection, root = infer(ROOT, publish=publish), cache_root()
    confirm = confirm or confirm_modal
    with locked(root):
        if publish:
            pointer = root / 'last-signed.json'
            require(pointer.exists(), 'No successfully signed release; run just release first.')
            record = backend.load(pointer)
            require(isinstance(record, dict) and set(record) == {'format', 'published', 'repository', 'tag',
                    'commit', 'key_id', 'stage', 'created_local_tag', 'identity', 'signer'} and
                    type(record['format']) is int and record['format'] == 1 and
                    isinstance(record['stage'], str) and len(record['stage']) <= 4096 and
                    type(record['created_local_tag']) is bool and record['published'] is False and
                    all(record.get(k) == selection[k] for k in ('repository', 'tag', 'commit', 'key_id')),
                    'Last signed release is already published or belongs to another source/tag; run just release.')
            stage = Path(record['stage'])
            require(stage.is_absolute() and stage.parent.parent == root and stage.name == 'stage' and
                    stage.parent.name.startswith('release-'), 'Invalid remembered release path.')
            private_directory(stage.parent)
            private_directory(stage)
            require(record['identity'] == signed_identity(stage), 'Remembered signed release changed; prepare again.')
            flake = 'git+' + ROOT.as_uri() + '?rev=' + selection['commit']
            require(backend.select_product(flake, 'update-signer') == record['signer'], 'Exact-tag signer changed.')
            signer = signer_path(record['signer'])
            with scratch(stage.parent):
                ledger = verify_stage(stage, signer)
                require(all(ledger[k] == selection[k] for k in ('repository', 'tag', 'commit')),
                        'Remembered release belongs to another source/tag.')
                details = summary(selection, stage, 'PUBLISH these already-signed bytes; no rebuild or signing.')
                details.append('Push only this exact tag to origin without force, then publish; existing identical tags stay unchanged.')
                require(confirm(details), 'Cancelled; signed artifacts kept and nothing published.')
                require(infer(ROOT, publish=True) == selection, 'Source or origin changed during confirmation.')
                require(record['identity'] == signed_identity(stage), 'Signed release changed during confirmation.')
                verify_stage(stage, signer)
                backend.run(['git', '-C', ROOT, 'push', '--no-follow-tags', 'origin',
                             'refs/tags/' + selection['tag'] + ':refs/tags/' + selection['tag']], timeout=300)
                backend.publish(argparse.Namespace(stage=stage, signer=signer, gh='gh'))
            record['published'] = True
            remember(root, record)
            return stage
        area = Path(tempfile.mkdtemp(prefix='release-' + selection['tag'] + '-', dir=root))
        stage = area / 'stage'  # The backend requires a nonexistent child directory.
        print('Release workspace:', area, flush=True)
        details = summary(selection, stage, 'BUILD + SIGN locally for testing; nothing uploaded.')
        details.append('Private key path: ' + selection['secret'])
        if selection['create_tag']:
            details.append('Create LOCAL ' + selection['tag'] + ' at clean committed HEAD; no remote tag yet.')
        require(confirm(details), 'Cancelled; no build/signing/tag change. Last signed release unchanged.')
        require(infer(ROOT) == selection, 'Source or origin changed during confirmation.')
        secret = Path(selection['secret'])
        require(secret.is_file() and not secret.is_symlink(),
                'Existing enrolled private key required at ' + str(secret) + '; never generate a replacement to release.')
        if selection['create_tag']:
            backend.run(['git', '-C', ROOT, 'tag', selection['tag'], selection['commit']])
        with scratch(area):
            signer_product = build_signer(selection, area)
            signer = signer_path(signer_product)
            backend.prepare(argparse.Namespace(source_repo=ROOT, tag=selection['tag'],
                repository=selection['repository'], output=stage, build=True))
            backend.sign(argparse.Namespace(stage=stage, signer=signer, sign_key=Path(selection['secret'])))
            verify_stage(stage, signer)
        record = {k: selection[k] for k in ('repository', 'tag', 'commit', 'key_id')}
        record.update(format=1, stage=str(stage), created_local_tag=selection['create_tag'],
                      identity=signed_identity(stage), signer=signer_product, published=False)
        remember(root, record)
        print('Signed artifacts for testing:', stage / 'dist')
        print('Installed Windows/Mac trees:', stage / 'install')
        print('When ready: just release --publish')
        return stage


def summary(selection, stage, operation):
    return [operation, 'Repository: ' + selection['repository'], 'Source checkout: ' + str(ROOT),
            'Immutable app tag: ' + selection['tag'], 'App commit: ' + selection['commit'],
            'Public hybrid key ID: ' + selection['key_id'], 'Output: ' + str(stage)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--publish', action='store_true', help='publish the last successfully signed stage')
    args = parser.parse_args()
    try:
        execute(args.publish)
    except KeyboardInterrupt:
        print('Release cancelled; any existing signed release is unchanged.', file=sys.stderr)
        return 1
    except (ValueError, OSError, subprocess.SubprocessError, curses.error, KeyError) as error:
        # Never print captured child output: signer diagnostics may contain sensitive data.
        print('Release stopped: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
