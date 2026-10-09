#!/usr/bin/env python3
"""Check that the Omarchy recipe builds the same package as pkgbuild/PKGBUILD.

The two differ only in where the source comes from: ours archives this
checkout and takes its version from git (so it needs git), the recipe
downloads a tag's tarball. Dependencies, architectures, options, license and
what build(), check() and package() run must match.
"""
import difflib
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
OURS = ROOT / 'pkgbuild/PKGBUILD'
RECIPE = ROOT / 'packaging/omarchy-pkgs/omacloud/PKGBUILD'
ARRAYS = ('depends', 'optdepends', 'makedepends', 'arch', 'options', 'license')
FUNCTIONS = ('build', 'check', 'package')
# Lines allowed to differ: entering the source directory (a checkout copy vs
# the tarball's directory) and our rustup pin.
IGNORED = re.compile(r'^\s*(cd \S+|export RUSTUP_TOOLCHAIN=stable);?$')


def read(pkgbuild, expression):
    """Evaluate a bash expression after sourcing a PKGBUILD."""
    return subprocess.run(['bash', '--noprofile', '--norc', '-c', f'source "$1" && {expression}', '_', str(pkgbuild)],
                          check=True, capture_output=True, text=True).stdout


def describe(pkgbuild):
    fields = {name: read(pkgbuild, f'printf "%s\\n" "${{{name}[@]}}"').splitlines() for name in ARRAYS}
    for name in FUNCTIONS:
        # declare -f prints the body without comments, in a fixed layout.
        body = read(pkgbuild, f'declare -f {name}').splitlines()
        fields[f'{name}()'] = [line for line in body if not IGNORED.match(line)]
    return fields


def differences(ours, recipe):
    ours, recipe = describe(ours), describe(recipe)
    ours['makedepends'] = [name for name in ours['makedepends'] if name != 'git']  # pkgver() runs git
    return ['\n'.join(difflib.unified_diff(ours[field], recipe[field], f'pkgbuild/PKGBUILD {field}',
                                           f'recipe {field}', lineterm=''))
            for field in ours if ours[field] != recipe[field]]


drift = differences(OURS, RECIPE)
assert not drift, 'The Omarchy recipe drifted from pkgbuild/PKGBUILD:\n' + '\n'.join(drift)
print('recipe matches pkgbuild/PKGBUILD: passed')

# The check itself catches a change in each kind of field.
for old, new in (("'libsecret'", "'libsecret' 'openssl'"),
                 ("options=('!debug' '!lto')", "options=('!debug')"),
                 ('-p omacloud-core --lib', '-p omacloud-core'),
                 ('target/release/omacloud-app', 'target/release/omacloud-gtk')):
    with tempfile.TemporaryDirectory() as directory:
        changed = Path(directory) / 'PKGBUILD'
        shutil.copy(RECIPE, changed)
        text = changed.read_text()
        assert old in text, old
        changed.write_text(text.replace(old, new, 1))
        assert differences(OURS, changed), f'not caught: {old} -> {new}'
print('drift is caught: passed')
