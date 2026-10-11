#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
import argparse,hashlib,subprocess,json,struct,time,zlib
from pathlib import Path
root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description='Synthetic release format and sandbox smoke tests')
parser.add_argument('--binary',type=Path,default=root/'target/release/bigiris')
parser.add_argument('--output',type=Path,default=root/'target/quality/round2/release-smoke')
args=parser.parse_args();binary=args.binary.resolve();out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
def chunk(kind,data): return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
source=out/'fixture.png'
source.write_bytes(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>2I5B',16,16,8,2,0,0,0))+chunk(b'IDAT',zlib.compress((b'\0'+bytes([30,70,120])*16)*16))+chunk(b'IEND',b''))
checks=[]
def run(name,args):
 start=time.monotonic();p=subprocess.run([str(binary),*map(str,args)],capture_output=True,timeout=90)
 checks.append({'name':name,'exit_code':p.returncode,'seconds':round(time.monotonic()-start,3)})
 if p.returncode: raise RuntimeError(p.stderr.decode(errors='replace'))
 return p.stdout
run('self-test',['--self-test'])
for f in ['png','jpeg','webp','avif','tiff','bmp','gif','ico','pnm','tga','qoi','hdr','exr']:
 run('encode-'+f,['convert','--to',f,'--overwrite','replace',source])
 ext={'jpeg':'jpg','tiff':'tiff','pnm':'ppm'}.get(f,f)
 path=out/('fixture.'+ext)
 assert path.is_file(), (f,path)
 run('resize-'+f,['resize','--max-edge','8','--overwrite','replace',path])
 resized=out/('fixture_8.'+ext)
 assert resized.is_file(), (f,resized)
 dimensions=struct.unpack('<II',run('verify-'+f,['--codec-worker','dimensions',resized]))
 assert max(dimensions)==8, (f,dimensions)
report={'passed':True,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'checks':checks};(out.parent/'release-smoke.json').write_text(json.dumps(report,indent=2));print(len(checks),'checks passed')
