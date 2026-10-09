#!/usr/bin/env python3
"""Exercise installer routing and migration with isolated files and commands."""
import os
from pathlib import Path
import subprocess
import tempfile

SOURCE = (Path(__file__).resolve().parents[2] / 'install.sh').read_text()
FINGERPRINT = '52130299581DAD685226900CA89CA1A6A1E74251'


def check(architecture, omarchy_repo=False, omarchy_has_omacloud=False, earlier_repo='omacloud'):
    """Run install.sh twice in an isolated root.

    omarchy_repo: pacman.conf lists Omarchy's [omarchy] repository.
    omarchy_has_omacloud: that repository carries omacloud.
    earlier_repo: the repository an earlier install left behind, or None.
    """
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        etc = root / 'etc'
        (etc / 'pacman.d').mkdir(parents=True)
        conf = etc / 'pacman.conf'
        original = '[options]\n'
        if omarchy_repo:
            original += '[omarchy]\nServer = https://pkgs.omarchy.org/stable/$arch\n'
        home = root / 'home'
        hooks = home / '.config/omarchy/hooks/pre-refresh-pacman.d'
        hooks.mkdir(parents=True)
        if earlier_repo:
            original += 'Include = ' + str(etc / f'pacman.d/{earlier_repo}.conf') + '\n'
            (etc / f'pacman.d/{earlier_repo}.conf').write_text(f'[{earlier_repo}]\n')
            (hooks / earlier_repo).write_text('old hook')
        conf.write_text(original)
        log = root / 'commands'
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        repos = 'echo omarchy' if omarchy_repo else 'true'
        listing = 'echo "omarchy omacloud 0.0.14-1"' if omarchy_has_omacloud else 'echo "omarchy aether 4.32.0-1"'
        commands = {
            'uname': f'echo {architecture}',
            'sudo': 'exec "$@"',
            'curl': 'echo "curl $*" >> "$TEST_LOG"; echo key > "$4"',
            'gpg': f'echo "fpr:::::::::{FINGERPRINT}:"',
            'pacman-key': 'echo "pacman-key $*" >> "$TEST_LOG"',
            'pacman-conf': f'[[ $1 == --repo-list ]] && {repos}',
            'pacman': 'echo "pacman $*" >> "$TEST_LOG"\n'
                      'case "$1" in\n'
                      f'  -Sl) [[ $2 == omarchy ]] || exit 1; {listing} ;;\n'
                      '  -Q) exit 1 ;;\n'
                      'esac',
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
        label = f'{architecture}, omarchy repo={omarchy_repo}, has omacloud={omarchy_has_omacloud}, earlier={earlier_repo}'
        if architecture not in ('x86_64', 'aarch64'):
            assert result.returncode == 1, result
            assert 'Unsupported architecture' in result.stderr, result.stderr
            assert conf.read_text() == original
            assert not log.exists()
            assert (hooks / 'omacloud').read_text() == 'old hook'
        elif omarchy_has_omacloud:
            assert result.returncode == 0, result.stderr
            assert "[omarchy] repository provides omacloud" in result.stdout, result.stdout
            commands_run = log.read_text()
            assert 'install omacloud' in commands_run
            # Nothing of ours is trusted, added or kept.
            assert 'signing-key.asc' not in commands_run
            assert 'pacman-key' not in commands_run
            for database in ('omacloud', 'omacloud-aarch64'):
                assert not (etc / f'pacman.d/{database}.conf').exists()
                assert f'pacman.d/{database}.conf' not in conf.read_text()
                assert not (hooks / database).exists()
            assert '[omarchy]' in conf.read_text()
            if earlier_repo and not (architecture == 'aarch64' and earlier_repo == 'omacloud'):
                assert f'Removing the [{earlier_repo}] repository' in result.stdout, result.stdout
            elif not earlier_repo:
                assert 'Removing the' not in result.stdout, result.stdout
            rerun = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
            assert rerun.returncode == 0, rerun.stderr
            assert 'Removing the' not in rerun.stdout, rerun.stdout
            assert 'Adding the' not in rerun.stdout, rerun.stdout
        else:
            assert result.returncode == 0, result.stderr
            database = 'omacloud' if architecture == 'x86_64' else 'omacloud-aarch64'
            repo_conf = etc / f'pacman.d/{database}.conf'
            assert f'Adding the [{database}] repository' in result.stdout, result.stdout
            assert f'[{database}]' in repo_conf.read_text()
            assert conf.read_text().count(f'Include = {repo_conf}') == 1
            assert f'{database}-signing-key.asc' in log.read_text()
            assert 'install omacloud' in log.read_text()
            assert (hooks / database).exists()
            if omarchy_repo:
                assert '[omarchy]' in conf.read_text()
            if architecture == 'aarch64':
                assert not (etc / 'pacman.d/omacloud.conf').exists()
                assert not (hooks / 'omacloud').exists()
            # Re-running must not duplicate the Include or break the hooks.
            rerun = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
            assert rerun.returncode == 0, rerun.stderr
            assert conf.read_text().count(f'Include = {repo_conf}') == 1
        print(f'{label}: passed')


for machine in ('x86_64', 'aarch64', 'riscv64'):
    check(machine)
for machine in ('x86_64', 'aarch64'):
    # [omarchy] without omacloud: our repository as before.
    check(machine, omarchy_repo=True)
    check(machine, omarchy_repo=True, earlier_repo=None)
    # [omarchy] with omacloud: install from it, fresh or replacing ours.
    check(machine, omarchy_repo=True, omarchy_has_omacloud=True, earlier_repo=None)
check('x86_64', omarchy_repo=True, omarchy_has_omacloud=True, earlier_repo='omacloud')
check('aarch64', omarchy_repo=True, omarchy_has_omacloud=True, earlier_repo='omacloud-aarch64')
# An old ARM install with the wrong-architecture repository.
check('aarch64', omarchy_repo=True, omarchy_has_omacloud=True, earlier_repo='omacloud')
