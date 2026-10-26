#!/usr/bin/env python3
"""Generate a changelog from the previous preserved application release tag."""
import json
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
app = json.loads((root / 'packaging.json').read_text())['application']
tags = subprocess.check_output(['git', 'tag', '-l', app + '-v*', '--sort=-version:refname'], cwd=root, text=True).splitlines()
revision_range = tags[0] + '..HEAD' if tags else 'HEAD'
notes = subprocess.check_output(['git', 'log', revision_range, '--format=- %s (%h)', '--', 'apps', 'crates', 'src', 'data', 'extensions'], cwd=root, text=True)
output = root / 'dist/CHANGELOG.md'
output.parent.mkdir(exist_ok=True)
output.write_text('# ' + app + ' release notes\n\n' + notes)
print(output)
