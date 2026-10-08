#!/usr/bin/env python3
"""Exercise installer routing and migration with isolated files and commands."""
import os
from pathlib import Path
import subprocess
import tempfile

SOURCE = (Path(__file__).resolve().parents[2] / 'install.sh').read_text()
FINGERPRINT = '52130299581DAD685226900CA89CA1A6A1E74251'


def check(architecture):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        etc = root / 'etc'
        (etc / 'pacman.d').mkdir(parents=True)
        conf = etc / 'pacman.conf'
        original = '[options]\nInclude = ' + str(etc / 'pacman.d/omacloud.conf') + '\n'
        conf.write_text(original)
        (etc / 'pacman.d/omacloud.conf').write_text('[omacloud]\n')
        home = root / 'home'
        hooks = home / '.config/omarchy/hooks/pre-refresh-pacman.d'
        hooks.mkdir(parents=True)
        (hooks / 'omacloud').write_text('old hook')
        log = root / 'commands'
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        commands = {
            'uname': f'echo {architecture}',
            'sudo': 'exec "$@"',
            'curl': 'echo "curl $*" >> "$TEST_LOG"; echo key > "$4"',
            'gpg': f'echo "fpr:::::::::{FINGERPRINT}:"',
            'pacman-key': 'echo "pacman-key $*" >> "$TEST_LOG"',
            'pacman': 'echo "pacman $*" >> "$TEST_LOG"; [[ $1 != -Q ]]',
            'omarchy-pkg-add': 'echo "install $*" >> "$TEST_LOG"',
            'getent': 'echo "tester:x:1000:1000::${TEST_HOME}:/bin/bash"',
            'id': 'case "$1" in -gn) echo "$(/usr/bin/id -gn)" ;; -un) echo "$(/usr/bin/id -un)" ;; esac',
        }
        # Use the actual username so chown of the isolated hook succeeds.
        username = subprocess.check_output(['id', '-un'], text=True).strip()
        for name, body in commands.items():
            path = bin_dir / name
            path.write_text('#!/bin/bash\n' + body + '\n')
            path.chmod(0o755)
        script = root / 'install.sh'
        script.write_text(SOURCE.replace('/etc/', str(etc) + '/'))
        env = {**os.environ, 'PATH': f'{bin_dir}:/usr/bin:/bin',
               'TEST_LOG': str(log), 'TEST_HOME': str(home),
               'USER': username, 'SUDO_USER': username}
        result = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
        if architecture not in ('x86_64', 'aarch64'):
            assert result.returncode == 1, result
            assert 'Unsupported architecture' in result.stderr, result.stderr
            assert conf.read_text() == original
            assert not log.exists()
            assert (hooks / 'omacloud').read_text() == 'old hook'
        else:
            assert result.returncode == 0, result.stderr
            database = 'omacloud' if architecture == 'x86_64' else 'omacloud-aarch64'
            repo_conf = etc / f'pacman.d/{database}.conf'
            assert f'[{database}]' in repo_conf.read_text()
            assert conf.read_text().count(f'Include = {repo_conf}') == 1
            assert f'{database}-signing-key.asc' in log.read_text()
            assert 'install omacloud' in log.read_text()
            assert (hooks / database).exists()
            if architecture == 'aarch64':
                assert not (etc / 'pacman.d/omacloud.conf').exists()
                assert not (hooks / 'omacloud').exists()
            # Re-running must not duplicate the Include or break the hooks.
            rerun = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
            assert rerun.returncode == 0, rerun.stderr
            assert conf.read_text().count(f'Include = {repo_conf}') == 1
        print(f'{architecture}: passed')


for machine in ('x86_64', 'aarch64', 'riscv64'):
    check(machine)
