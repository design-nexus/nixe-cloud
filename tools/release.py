#!/usr/bin/env python3
"""Publish an already-built standalone release with source/provenance checksums."""
import argparse, hashlib, json, os, re, subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
def run(*args,capture=False):
 r=subprocess.run(args,cwd=ROOT,check=True,text=True,stdout=subprocess.PIPE if capture else None)
 return r.stdout.strip() if capture else None
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--tag',default='v0.1.0');p.add_argument('--publish',action='store_true');a=p.parse_args()
if not re.fullmatch(r'v\d+\.\d+\.\d+(?:[-.][A-Za-z0-9.-]+)?',a.tag):raise SystemExit('Invalid version tag')
if run('git','status','--porcelain',capture=True):raise SystemExit('Commit source changes before packaging a release')
spec=json.loads((ROOT/'packaging.json').read_text());app=spec['application'];revision=run('git','rev-parse','HEAD',capture=True)
archive=ROOT/'dist'/f'{app}-x86_64.tar.gz'
if not archive.exists():raise SystemExit('Build/package the app first: python3 tools/package.py')
run('python3','tools/release-notes.py')
provenance=ROOT/'dist/build-info.json'
if not provenance.exists() or json.loads(provenance.read_text())['revision']!=revision:raise SystemExit('Rebuild artifacts for this source revision')
assets=[archive,ROOT/'dist/CHANGELOG.md',provenance]
if app=='git':assets+=sorted((ROOT/'dist').glob('*.pkg.tar.zst'))
checksums=ROOT/'dist/SHA256SUMS';checksums.write_text(''.join(hashlib.sha256(f.read_bytes()).hexdigest()+'  '+f.name+'\n' for f in assets));assets.append(checksums)
if a.publish:
 repo='design-nexus/nyx-'+app
 exists=subprocess.run(['gh','release','view',a.tag,'--repo',repo],cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode==0
 if exists:raise SystemExit('Release already exists; do not replace published artifacts')
 run('gh','release','create',a.tag,*map(str,assets),'--repo',repo,'--target',revision,'--title',app+' '+a.tag,'--notes-file',str(ROOT/'dist/CHANGELOG.md'))
print('Release assets ready: '+str(checksums))
