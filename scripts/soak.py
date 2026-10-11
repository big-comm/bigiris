#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Run the synthetic core soak and retain an explicit pending/final status."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=28800)
    parser.add_argument('--background', action='store_true')
    args = parser.parse_args()
    if not 1 <= args.seconds <= 86400:
        parser.error('seconds: 1..86400')
    output = ROOT / 'target/quality/long-soak'
    output.mkdir(parents=True, exist_ok=True)
    status = output / 'status.json'
    if status.exists():
        previous = json.loads(status.read_text())
        if previous.get('status') == 'running':
            pid = previous.get('pid')
            try:
                command = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
                if str(Path(__file__).resolve()).encode() in command:
                    parser.error(f'long soak already running (pid {pid}); keep its evidence')
            except FileNotFoundError:
                pass
    if args.background:
        with (output / 'supervisor.log').open('w') as log:
            child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--seconds', str(args.seconds)],
                cwd=ROOT, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        print(json.dumps({'pid': child.pid, 'status_file': str(output / 'status.json')}))
        return 0
    binary = ROOT / 'target/release/examples/quality_probe'
    report = dict(status='running', seconds_requested=args.seconds, pid=os.getpid(),
        started_utc=datetime.now(timezone.utc).isoformat(), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    def save():
        temp = output / 'status.new'
        temp.write_text(json.dumps(report, indent=2)+'\n'); temp.replace(status)
    save()
    try:
        with (output / 'probe.log').open('w') as log:
            result = subprocess.run([str(binary),'soak',str(args.seconds)],cwd=ROOT,stdout=log,stderr=subprocess.STDOUT,timeout=args.seconds+120)
        report.update(status='passed' if result.returncode == 0 else 'failed', exit_code=result.returncode)
    except (OSError, subprocess.TimeoutExpired) as error:
        report.update(status='failed', error=str(error))
    report['ended_utc'] = datetime.now(timezone.utc).isoformat();save()
    return 0 if report['status']=='passed' else 1

if __name__ == '__main__':
    raise SystemExit(main())
