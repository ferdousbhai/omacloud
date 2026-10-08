#!/usr/bin/env python3
"""Exercise CI artifact provenance and signing gates without keys or publication."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SHA = 'a' * 40


def check(failure=''):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / 'scripts').mkdir()
        (root / 'bin').mkdir()
        (root / 'Cargo.toml').write_text('version = "1.2.3"\n')
        (root / 'CHANGELOG.md').write_text('## 1.2.3 (2026-10-08)\nTest release.\n')
        (root / 'install.sh').write_text((ROOT / 'install.sh').read_text())
        script = root / 'scripts/release-ci.sh'
        script.write_text((ROOT / 'scripts/release-ci.sh').read_text())
        for architecture in ('x86_64', 'aarch64'):
            artifact = root / architecture
            artifact.mkdir()
            (artifact / 'source-commit.txt').write_text(('b' * 40 if failure == 'manifest' else SHA) + '\n')
            metadata = f'pkgname = omacloud\npkgver = 1.2.3-1\narch = {architecture}\n'
            if failure == 'metadata' and architecture == 'aarch64':
                metadata = metadata.replace('arch = aarch64', 'arch = x86_64')
            with tarfile.open(artifact / f'omacloud-1.2.3-1-{architecture}.pkg.tar.zst', 'w') as archive:
                entry = tarfile.TarInfo('.PKGINFO')
                entry.size = len(metadata.encode())
                archive.addfile(entry, io.BytesIO(metadata.encode()))
        commands = {
            'gh': '''#!/usr/bin/python3
import base64, os, pathlib, shutil, sys
root = pathlib.Path(os.environ['FIXTURE'])
args = sys.argv[1:]
with open(root / 'calls', 'a') as log: log.write('gh ' + ' '.join(args) + '\\n')
if args[0] == 'api':
    if '/commits/' in args[1]:
        count = root / 'master-reads'
        value = int(count.read_text()) if count.exists() else 0
        count.write_text(str(value + 1))
        print(('b' if os.environ['FAILURE'] == 'master-moved' and value else 'a') * 40)
    else:
        path = args[1].split('/contents/')[1].split('?')[0]
        print(base64.b64encode((root / path).read_bytes()).decode())
elif args[:2] == ['run', 'list']: print('123')
elif args[:2] == ['run', 'view']:
    conclusion = 'failure' if os.environ['FAILURE'] == 'ci' else 'success'
    print('push\\tmaster\\t' + 'a' * 40 + '\\tcompleted\\t' + conclusion)
elif args[:2] == ['run', 'download']:
    architecture = args[args.index('--name') + 1].removeprefix('omacloud-')
    shutil.copytree(root / architecture, args[args.index('--dir') + 1])
''',
            'gpg': '''#!/bin/bash
echo "gpg $*" >> "$FIXTURE/calls"
if [[ "$*" == *--detach-sign* ]]; then
  [[ $FAILURE != signing ]] || exit 1
  touch "${@: -1}.sig"
fi
if [[ "$*" == *--verify* && $FAILURE == signature ]]; then exit 1; fi
if [[ "$*" == *--export* ]]; then echo public-key; fi
''',
            'repo-add': '''#!/bin/bash
[[ $FAILURE != database ]] || exit 1
printf '%s\\n' "$4" > "$3"
touch "$3.sig"
files=${3/.db./.files.}
printf '%s\\n' "$4" > "$files"
touch "$files.sig"
for name in db files; do
  database=${3%.db.tar.gz}
  ln -s "$database.$name.tar.gz" "$database.$name"
  ln -s "$database.$name.tar.gz.sig" "$database.$name.sig"
done
''',
        }
        for name, body in commands.items():
            executable = root / 'bin' / name
            executable.write_text(body)
            executable.chmod(0o755)
        result = subprocess.run(['bash', str(script), '1.2.3'], env={
            **os.environ, 'PATH': f'{root / "bin"}:/usr/bin:/bin',
            'FIXTURE': str(root), 'FAILURE': failure,
        }, capture_output=True, text=True)
        calls = (root / 'calls').read_text()
        if failure:
            assert result.returncode != 0, result
            assert 'gh release create' not in calls, calls
            if failure in ('ci', 'manifest', 'metadata'):
                assert '--detach-sign' not in calls, calls
        else:
            assert result.returncode == 0, result.stderr
            assert 'gh release create v1.2.3' in calls
            assert f'--target {SHA}' in calls
            assets = next((root / 'dist').glob('release-*/assets'))
            for architecture, database in (('x86_64', 'omacloud'), ('aarch64', 'omacloud-aarch64')):
                alias = assets / f'{database}.db'
                assert not alias.is_symlink()
                assert alias.read_text().strip() == f'omacloud-1.2.3-1-{architecture}.pkg.tar.zst'
            assert (assets / 'install.sh').exists()
        print(f'{failure or "success"}: passed')


for failure in ('', 'ci', 'manifest', 'metadata', 'signing', 'signature', 'database', 'master-moved'):
    check(failure)
