#!/usr/bin/env python3
"""Check dual-architecture publication and failures without publishing or keys."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
VERSION = '1.2.3'


def package(path, architecture):
    info = f'pkgname = omacloud\npkgver = {VERSION}-1\narch = {architecture}\n'.encode()
    with tarfile.open(path, 'w') as archive:
        entry = tarfile.TarInfo('.PKGINFO')
        entry.size = len(info)
        archive.addfile(entry, io.BytesIO(info))


def check(host, failure='', manifest=False):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / 'scripts').mkdir()
        (root / 'pkgbuild').mkdir()
        (root / 'bin').mkdir()
        (root / 'Cargo.toml').write_text(f'version = "{VERSION}"\n')
        (root / 'CHANGELOG.md').write_text(f'## {VERSION}\nTest release\n')
        (root / 'install.sh').write_text((ROOT / 'install.sh').read_text())
        script = root / 'scripts/release.sh'
        script.write_text((ROOT / 'scripts/release.sh').read_text())
        verify = root / 'scripts/verify-release.sh'
        verify.write_text('#!/bin/bash\necho "verify $*" >> "$TEST_LOG"\nif [[ $FAILURE == infrastructure ]]; then exit 2; fi\n[[ $FAILURE != verification || $2 != aarch64 ]]\n')
        verify.chmod(0o755)
        other = 'aarch64' if host == 'x86_64' else 'x86_64'
        host_package = root / f'omacloud-{VERSION}-1-{host}.pkg.tar.zst'
        other_package = root / f'omacloud-{VERSION}-1-{other}.pkg.tar.zst'
        package(host_package, host)
        package(other_package, 'wrong' if failure == 'metadata' else other)
        if manifest or failure == 'manifest':
            (root / 'source-commit.txt').write_text(('b' if failure == 'manifest' else 'a') * 40 + '\n')
        commands = {
            'uname': f'echo {host}',
            'git': '''echo "git $*" >> "$TEST_LOG"
case "$1" in status) ;; branch) echo master ;; rev-parse) if [[ $2 == HEAD ]]; then printf "a%.0s" {1..40}; echo; else exit 1; fi ;; esac''',
            'cargo': 'exit 0',
            'makepkg': '[[ $PKGEXT == .pkg.tar.zst && $FAILURE != build ]] || exit 1; cp "$HOST_PACKAGE" "$PKGDEST/"',
            'gpg': '''echo "gpg $*" >> "$TEST_LOG"
if [[ "$*" == *--export* ]]; then echo key; fi
if [[ "$*" == *--detach-sign* ]]; then
  [[ $FAILURE != signing ]] || exit 1
  touch "${@: -1}.sig"
fi''',
            'repo-add': '''echo "repo-add $*" >> "$TEST_LOG"
[[ $FAILURE != database ]] || exit 1
printf '%s\\n' "$4" > "$3"
touch "$3.sig"
files=${3/.db./.files.}
printf '%s\\n' "$4" > "$files"
touch "$files.sig"''',
            'gh': 'echo "gh $*" >> "$TEST_LOG"',
        }
        for name, body in commands.items():
            command = root / 'bin' / name
            command.write_text('#!/bin/bash\n' + body + '\n')
            command.chmod(0o755)
        log = root / 'commands'
        env = {**os.environ, 'PATH': f'{root / "bin"}:/usr/bin:/bin',
               'TEST_LOG': str(log), 'HOST_PACKAGE': str(host_package),
               'FAILURE': failure, 'ARM_VERIFY_IMAGE': 'test/arch-arm'}
        result = subprocess.run(['bash', str(script), VERSION, str(other_package)],
                                env=env, capture_output=True, text=True)
        calls = log.read_text() if log.exists() else ''
        if failure:
            assert result.returncode != 0, result
            if failure == 'infrastructure':
                assert result.returncode == 2
                assert 'gh release create v1.2.3' in calls
                assert 'gh release delete' not in calls
                assert 'git push -q origin --delete' not in calls
            elif failure == 'verification':
                assert 'gh release create v1.2.3' in calls, calls
                assert 'gh release delete v1.2.3' in calls, calls
                assert 'git push -q origin --delete v1.2.3' in calls, calls
            else:
                assert 'gh release create' not in calls, calls
            if failure == 'infrastructure':
                assert 'git tag -d' not in calls
            elif failure in ('metadata', 'manifest'):
                assert 'git tag -a' not in calls, calls
            else:
                assert 'git tag -d v1.2.3' in calls, calls
        else:
            assert result.returncode == 0, result.stderr
            for arch, repo in (('x86_64', 'omacloud'), ('aarch64', 'omacloud-aarch64')):
                assert (root / 'dist' / f'{repo}.db').read_text().strip() == f'omacloud-{VERSION}-1-{arch}.pkg.tar.zst'
                assert (root / 'dist' / f'{repo}-signing-key.asc').read_text() == 'key\n'
                assert f'verify {VERSION} {arch}' in calls
            assert 'gh release create v1.2.3' in calls
        print(f'{host}, {failure or ('matching manifest' if manifest else 'success')}: passed')


for host in ('x86_64', 'aarch64'):
    check(host)
check('aarch64', manifest=True)
for failure in ('metadata', 'manifest', 'build', 'signing', 'database', 'verification', 'infrastructure'):
    check('aarch64', failure)
