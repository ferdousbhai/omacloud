#!/usr/bin/env python3
"""Execute the verifier's container commands in an isolated mocked runtime."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

SCRIPT = Path(__file__).resolve().parents[1] / 'verify-release.sh'


def check(architecture, failure=''):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        binaries = root / 'binaries'
        binaries.mkdir()
        for name in ('omacloud', 'omacloud-app'):
            (binaries / name).write_text('/home/builder' if failure == 'paths' else 'binary')
        commands = {
            'uname': 'echo "$RUN_ARCH"',
            'pacman-key': 'exit 0',
            'pacman': '''case "$1" in
-Qi) echo "Architecture    : $PACKAGE_ARCH" ;;
-Q) echo "omacloud 1.2.3-1" ;;
esac''',
            'curl': 'echo "exit 0"',
            'omacloud': 'exit 0',
            'sleep': 'exit 0',
        }
        for name, body in commands.items():
            path = root / name
            path.write_text('#!/bin/bash\n' + body + '\n')
            path.chmod(0o755)
        docker = root / 'docker'
        docker.write_text('''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
with open(os.environ['TEST_LOG'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
env = dict(os.environ)
for index, arg in enumerate(args):
    if arg == '-e':
        key, value = args[index + 1].split('=', 1)
        env[key] = value
payload = args[-1].replace('/usr/bin/', os.environ['TEST_BINARIES'] + '/')
result = subprocess.run(['bash', '-euo', 'pipefail', '-c', payload], env=env)
sys.exit(result.returncode)
''')
        docker.chmod(0o755)
        log = root / 'calls'
        env = {**os.environ, 'PATH': f'{root}:/usr/bin:/bin',
               'RUN_ARCH': 'wrong' if failure == 'machine' else architecture,
               'PACKAGE_ARCH': 'wrong' if failure == 'package' else architecture,
               'ARM_VERIFY_IMAGE': 'test/arch-arm', 'TEST_BINARIES': str(binaries),
               'TEST_LOG': str(log)}
        result = subprocess.run(['bash', str(SCRIPT), '1.2.3', architecture],
                                env=env, capture_output=True, text=True)
        assert result.returncode == (1 if failure else 0), result
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert len(calls) == (6 if failure else 1)
        expected_platform = 'linux/amd64' if architecture == 'x86_64' else 'linux/arm64'
        expected_image = 'archlinux:base-devel' if architecture == 'x86_64' else 'test/arch-arm'
        for args in calls:
            assert args[args.index('--platform') + 1] == expected_platform
            assert expected_image in args
        print(f'{architecture}, {failure or "success"}: passed')


for architecture in ('x86_64', 'aarch64'):
    check(architecture)
for failure in ('machine', 'package', 'paths'):
    check('aarch64', failure)
