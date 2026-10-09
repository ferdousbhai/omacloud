#!/usr/bin/env python3
"""Exercise carrying a release to omarchy-pkgs with stubbed GitHub access.

gh and curl are stubs; git is the real one with github.com URLs pointed at
local bare repositories, so pushes can be inspected.
"""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
RECIPE = ROOT / 'packaging/omarchy-pkgs/omacloud'
TARBALL = b'omacloud v1.2.3 tarball'
SUM = hashlib.sha256(TARBALL).hexdigest()
GIT = shutil.which('git')
WORK = tempfile.TemporaryDirectory()  # one directory per run, removed at exit
IDENTITY = {'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.com',
            'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.com'}


def git(*args, cwd=None):
    return subprocess.run([GIT, *args], cwd=cwd, check=True, capture_output=True, text=True,
                          env={**os.environ, **IDENTITY}).stdout.strip()


def remote(root, name, files, branch=None):
    """A bare repository with master, and a branch adding files."""
    seed = root / 'seed'
    shutil.rmtree(seed, ignore_errors=True)
    git('init', '-q', '-b', 'master', str(seed))
    (seed / 'pkgbuilds/other').mkdir(parents=True)
    (seed / 'pkgbuilds/other/PKGBUILD').write_text('pkgname=other\n')

    def add(files):
        for path, text in files.items():
            (seed / path).parent.mkdir(parents=True, exist_ok=True)
            (seed / path).write_text(text)
    if not branch:
        add(files)
    git('add', '-A', cwd=seed)
    git('commit', '-qm', 'base', cwd=seed)
    if branch:
        git('switch', '-qc', branch, cwd=seed)
        add(files)
        git('add', '-A', cwd=seed)
        git('commit', '-qm', 'omacloud 0.0.14', cwd=seed)
    bare = root / 'remotes' / f'{name}.git'
    git('clone', '-q', '--bare', str(seed), str(bare))
    return bare


def recipe(**replacements):
    files = {f'pkgbuilds/omacloud/{path}': (RECIPE / path).read_text()
             for path in ('PKGBUILD', '.omarchy/package.json')}
    for old, new in replacements.items():
        assert old in files['pkgbuilds/omacloud/PKGBUILD'], old
        files['pkgbuilds/omacloud/PKGBUILD'] = files['pkgbuilds/omacloud/PKGBUILD'].replace(old, new)
    return files


def check(state, upstream=None, released=None, on_fork=None, dry_run=False, failure=''):
    """Run the script once against fresh remotes.

    state: what gh says omacom/omarchy-pkgs#778 is.
    upstream: the recipe on Omarchy's master once merged.
    released: the recipe at the tag, by default the one in this checkout.
    on_fork: the recipe on #778's branch, by default 0.0.14's.
    """
    root = Path(tempfile.mkdtemp(dir=WORK.name))
    fork = remote(root, 'ferdousbhai/omarchy-pkgs', on_fork or recipe(), branch='omacloud')
    upstream_bare = remote(root, 'omacom/omarchy-pkgs', upstream or {})
    tag = root / 'tag'
    for path, text in (released or recipe()).items():
        target = tag / path.replace('pkgbuilds/omacloud/', 'packaging/omarchy-pkgs/omacloud/')
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
    (root / 'scripts').mkdir()
    shutil.copy(ROOT / 'scripts/omarchy-pkgs.sh', root / 'scripts')
    bin_dir = root / 'bin'
    bin_dir.mkdir()
    commands = {
        'gh': '''#!/usr/bin/python3
import base64, os, pathlib, sys
root = pathlib.Path(os.environ['FIXTURE'])
args = sys.argv[1:]
with open(root / 'calls', 'a') as log: log.write('gh ' + ' '.join(args) + '\\n')
if args[0] == 'api':
    assert args[1].endswith('?ref=v1.2.3'), args
    path = args[1].split('/contents/')[1].split('?')[0]
    print(base64.b64encode((root / 'tag' / path).read_bytes()).decode())
elif args[:2] == ['pr', 'view']: print(os.environ['STATE'])
elif args[:2] == ['pr', 'list']: print('')
elif args[:2] == ['pr', 'create']: print('https://github.com/omacom/omarchy-pkgs/pull/999')
''',
        'curl': '''#!/bin/bash
echo "curl $*" >> "$FIXTURE/calls"
[[ $FAILURE != download ]] || exit 22
printf '%s' "$TARBALL"
''',
        'git': f'''#!/bin/bash
args=()
for argument; do args+=("${{argument/https:\\/\\/github.com\\//$FIXTURE/remotes/}}"); done
exec {GIT} "${{args[@]}}"
''',
    }
    for name, body in commands.items():
        path = bin_dir / name
        path.write_text(body)
        path.chmod(0o755)
    fork_before = git('rev-parse', 'omacloud', cwd=fork)
    result = subprocess.run(
        ['bash', str(root / 'scripts/omarchy-pkgs.sh'), '1.2.3', *(['--dry-run'] if dry_run else [])],
        cwd=root, capture_output=True, text=True,
        env={**os.environ, 'PATH': f'{bin_dir}:/usr/bin:/bin', 'FIXTURE': str(root), 'STATE': state,
             'FAILURE': failure, 'TARBALL': TARBALL.decode()})
    calls = (root / 'calls').read_text()
    rendered = (root / 'dist/omarchy-pkgs-1.2.3/omacloud/PKGBUILD')
    fork_branches = git('for-each-ref', '--format=%(refname:short)', 'refs/heads', cwd=fork).split()
    return result, calls, rendered, fork, fork_before, fork_branches, upstream_bare


def show(bare, ref, path='pkgbuilds/omacloud/PKGBUILD'):
    return git('show', f'{ref}:{path}', cwd=bare)


# While #778 is open, the rendered recipe goes to its branch.
result, calls, rendered, fork, before, branches, _ = check('OPEN')
assert result.returncode == 0, result.stderr
assert 'Pushed omacloud 1.2.3' in result.stdout, result.stdout
pkgbuild = show(fork, 'omacloud')
assert 'pkgver=1.2.3\n' in pkgbuild and f"sha256sums=('{SUM}')" in pkgbuild, pkgbuild
assert pkgbuild == rendered.read_text().strip()
assert git('log', '-1', '--format=%s', 'omacloud', cwd=fork) == 'omacloud 1.2.3'
assert git('rev-parse', 'omacloud^', cwd=fork) == before
assert 'archive/refs/tags/v1.2.3.tar.gz' in calls
assert 'pr create' not in calls
print('open, new version: passed')

# Already there: nothing is pushed.
current = recipe(**{'pkgver=0.0.14': 'pkgver=1.2.3'})
current['pkgbuilds/omacloud/PKGBUILD'] = '\n'.join(
    f"sha256sums=('{SUM}')" if line.startswith('sha256sums=') else line
    for line in current['pkgbuilds/omacloud/PKGBUILD'].split('\n'))
result, calls, _, fork, before, _, _ = check('OPEN', on_fork=current)
assert result.returncode == 0, result.stderr
assert 'already builds omacloud 1.2.3' in result.stdout, result.stdout
assert git('rev-parse', 'omacloud', cwd=fork) == before
print('open, already current: passed')

# A dry run pushes nothing.
result, calls, rendered, fork, before, branches, _ = check('OPEN', dry_run=True)
assert result.returncode == 0, result.stderr
assert 'Dry run: would push' in result.stdout, result.stdout
assert git('rev-parse', 'omacloud', cwd=fork) == before
assert 'pkgver=1.2.3' in rendered.read_text()
print('open, dry run: passed')

# Merged, and Omarchy's copy differs only in what its bot bumps.
bumped = recipe(**{'pkgrel=1': 'pkgrel=2'})
result, calls, _, fork, before, branches, upstream = check('MERGED', upstream=bumped)
assert result.returncode == 0, result.stderr
assert "sync-upstream bot picks up v1.2.3" in result.stdout, result.stdout
assert 'pr create' not in calls
assert branches == ['master', 'omacloud'], branches
print('merged, version only: passed')

# Merged, and the recipe changed beyond the version: a new pull request.
changed = recipe(**{"'libsecret')": "'libsecret' 'openssl')"})
result, calls, _, fork, before, branches, upstream = check('MERGED', upstream=recipe(), released=changed)
assert result.returncode == 0, result.stderr
assert 'omacloud-1.2.3' in branches, branches
assert "'openssl'" in show(fork, 'omacloud-1.2.3') and 'pkgver=1.2.3' in show(fork, 'omacloud-1.2.3')
assert git('rev-parse', 'omacloud-1.2.3^', cwd=fork) == git('rev-parse', 'master', cwd=upstream)
assert 'pr create --repo omacom/omarchy-pkgs --base master --head ferdousbhai:omacloud-1.2.3' in calls, calls
assert 'Next: follow https://github.com/omacom/omarchy-pkgs/pull/999' in result.stdout, result.stdout
print('merged, recipe changed: passed')

result, calls, _, _, _, branches, _ = check('MERGED', upstream=recipe(), released=changed, dry_run=True)
assert result.returncode == 0, result.stderr
assert 'Dry run: would push' in result.stdout and 'pr create' not in calls
assert branches == ['master', 'omacloud'], branches
print('merged, dry run: passed')

# Closed without merging, or the tarball unavailable: fail, change nothing.
for state, failure in (('CLOSED', ''), ('OPEN', 'download')):
    result, calls, _, fork, before, branches, _ = check(state, failure=failure)
    assert result.returncode != 0, result
    assert git('rev-parse', 'omacloud', cwd=fork) == before
    assert branches == ['master', 'omacloud'], branches
    print(f"{state.lower()}{', ' + failure + ' fails' if failure else ''}: passed")
