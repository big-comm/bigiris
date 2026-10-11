#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Exercise real ONNX inference using synthetic RGBA images and isolated data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import struct
import subprocess
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/bigiris')
    parser.add_argument('--output', type=Path, default=ROOT / 'target/quality/round2/ai-release')
    parser.add_argument('--allow-download', action='store_true')
    args = parser.parse_args()
    binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    cache = ROOT / 'target/quality/ai'
    if not args.allow_download and not any(cache.glob('*/models/*.onnx')):
        parser.error('isolated model cache missing; explicitly use --allow-download')
    env = dict(os.environ, XDG_DATA_HOME=str(cache))
    originals = []

    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

    for index, (width, height) in enumerate([(32, 24), (24, 32)]):
        pixels = bytes(component for y in range(height) for x in range(width)
                       for component in (x * 7 % 256, y * 9 % 256, 120, 0 if x == 0 else 123))
        rows = b''.join(b'\0' + pixels[y*width*4:(y+1)*width*4] for y in range(height))
        path = output / f'synthetic-{index}.png'
        path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>2I5B', width, height, 8, 6, 0, 0, 0))
                         + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))
        originals.append((path, width, height, pixels))
    report = dict(passed=False, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  images=len(originals), model_cache=str(cache))
    started = time.monotonic()
    try:
        result = subprocess.run([str(binary), 'remove-bg', *[str(item[0]) for item in originals]],
                                env=env, capture_output=True, timeout=300)
        (output / 'inference.log').write_bytes(result.stdout + result.stderr)
        report.update(exit_code=result.returncode, seconds=round(time.monotonic()-started, 3),
                      max_rss_kib=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
        if result.returncode:
            raise RuntimeError('inference failed; see inference.log')
        alpha_ranges = []
        for path, width, height, source in originals:
            destination = path.with_name(path.stem + '_nobg.png')
            # Trusted synthetic output only: inspect raw pixels without GTK.
            raw = subprocess.check_output([str(binary), '--codec-worker', 'pixels', str(destination)], timeout=30)
            assert raw[:8] == b'BIGIRIS2' and raw[8] == 4
            assert struct.unpack('<II', raw[12:20]) == (width, height)
            pixels = raw[36:]
            assert len(pixels) == len(source)
            for offset in range(0, len(source), 4):
                assert pixels[offset:offset+3] == source[offset:offset+3], 'RGB changed'
                assert pixels[offset+3] <= source[offset+3], 'original alpha lost'
            alpha_ranges.append([min(pixels[3::4]), max(pixels[3::4])])
        report.update(passed=True, alpha_ranges=alpha_ranges)
    except (OSError, subprocess.SubprocessError, AssertionError, RuntimeError) as error:
        report.update(error=str(error), seconds=round(time.monotonic()-started, 3))
    (output / 'summary.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
