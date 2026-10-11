#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Generate synthetic seeds and run coverage-guided ASan/LSan campaigns."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=120)
    parser.add_argument('--allow-bootstrap', action='store_true', help='use unstable sanitizer flags with system Rust, only in this development build')
    parser.add_argument('--offline', action='store_true')
    parser.add_argument('--reuse-corpus', action='store_true', help='reuse existing synthetic seeds and previous discoveries')
    args = parser.parse_args()
    if not 1 <= args.seconds <= 86400:
        parser.error('seconds: 1..86400')
    version = subprocess.check_output(['rustc', '-V'], text=True).strip()
    if 'nightly' not in version and not args.allow_bootstrap:
        parser.error('use nightly Rust or explicitly --allow-bootstrap for system Rust')
    output = ROOT / 'target/quality/fuzz'
    output.mkdir(parents=True, exist_ok=True)
    corpus = ROOT / 'target/quality/fuzz-corpus'
    artifacts = ROOT / 'target/quality/fuzz-artifacts'
    artifacts.mkdir(parents=True, exist_ok=True)
    flags = '-Zsanitizer=address -Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 -Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Cllvm-args=-sanitizer-coverage-trace-compares'
    base = dict(os.environ, CARGO_BUILD_JOBS='4')
    locked = ['--locked'] + (['--offline'] if args.offline else [])
    checks = []
    def run(name, command, env, timeout):
        start = time.monotonic()
        with (output / (name + '.log')).open('w') as log:
            try:
                status = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=timeout).returncode
            except (OSError, subprocess.TimeoutExpired) as error:
                log.write(str(error)); status = -1
        checks.append(dict(name=name, exit_code=status, seconds=round(time.monotonic()-start, 3), command=command))
        return status == 0
    if args.reuse_corpus:
        ready = all((corpus / name).is_dir() and any((corpus / name).iterdir()) for name in ('decode','icc'))
        checks.append(dict(name='existing-synthetic-corpus', exit_code=0 if ready else 1))
    else:
        ready = run('corpus', ['cargo','run','--release','-p','bigimage-core','--example','quality_corpus'] + locked + ['--',str(corpus)], base, 900)
    instrumented = base | dict(RUSTFLAGS=flags, CARGO_TARGET_DIR=str(ROOT / 'target/quality/fuzz-build'))
    if 'nightly' not in version:
        instrumented['RUSTC_BOOTSTRAP'] = '1'
    ready = ready and run('build', ['cargo','build','--manifest-path','fuzz/Cargo.toml','--target','x86_64-unknown-linux-gnu'] + locked, instrumented, 900)
    if ready:
        for name in ('decode', 'icc'):
            binary = ROOT / 'target/quality/fuzz-build/x86_64-unknown-linux-gnu/debug' / name
            run(name, [str(binary),str(corpus / name), f'-max_total_time={args.seconds}', '-max_len=16384', '-rss_limit_mb=2048','-timeout=3',f'-artifact_prefix={artifacts}/'], base, args.seconds + 60)
    report = dict(rustc=version, bootstrap='nightly' not in version, passed=all(c['exit_code']==0 for c in checks), checks=checks)
    (output / 'summary.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report, indent=2))
    return 0 if report['passed'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
