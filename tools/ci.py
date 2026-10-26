#!/usr/bin/env python3
"""Hosted checks: require published dependency sources; never fetch/run models."""
import json
from pathlib import Path
import subprocess
root = Path(__file__).resolve().parents[1]
names = subprocess.check_output(['git', 'ls-files', '*Cargo.toml'], cwd=root, text=True).splitlines()
if any('file://' in (root / name).read_text() for name in names):
    raise SystemExit('Publish immutable shared dependency sources before enabling hosted CI.')
spec = json.loads((root / 'packaging.json').read_text())
for command in spec.get('prepare', []):
    subprocess.run(command, cwd=root, check=True)
command = ['cargo', 'build', '--release', '--locked', '--manifest-path', spec['manifest']]
for package in spec['packages']:
    command += ['-p', package]
subprocess.run(command, cwd=root, check=True)
for command in spec['test_commands']:
    subprocess.run(command, cwd=root, check=True)
subprocess.run(['python3', 'tools/package.py', '--no-build'], cwd=root, check=True)

subprocess.run(['python3', 'tools/release-notes.py'], cwd=root, check=True)
