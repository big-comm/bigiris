#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Execute reproducible quality gates; retain evidence and all failed checks."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gui", action="store_true", help="compile/test GTK and AI features")
    parser.add_argument("--graphical", action="store_true", help="open synthetic test windows; requires display")
    parser.add_argument("--performance", action="store_true", help="release benchmarks, stress and soak")
    parser.add_argument("--soak-seconds", type=int, default=300)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--audit", action="store_true", help="audit Cargo.lock with cargo-audit")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not 1 <= args.soak_seconds <= 86400 or not 1 <= args.samples <= 3600:
        parser.error("samples: 1..3600; soak-seconds: 1..86400")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output or ROOT / "target/quality" / stamp
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", CARGO_TERM_COLOR="never")
    env.setdefault("CARGO_BUILD_JOBS", "4")
    results = []

    def run(name, command, timeout=900, extra_env=None):
        print(f"{name}…", flush=True)
        started = time.monotonic()
        try:
            proc = subprocess.run(command, cwd=ROOT, env=env | (extra_env or {}),
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True, timeout=timeout, check=False)
            code, text = proc.returncode, proc.stdout
        except (OSError, subprocess.TimeoutExpired) as exc:
            code, text = -1, str(exc)
        (output / f"{name}.log").write_text(text)
        results.append({"name": name, "command": command, "exit_code": code,
                        "seconds": round(time.monotonic() - started, 3), "log": f"{name}.log"})
        print(f"  {'OK' if code == 0 else 'FALHOU'} ({results[-1]['seconds']} s)", flush=True)
        if code:
            print(text[-2500:], flush=True)
        return code

    locked = ["--locked"] + (["--offline"] if args.offline else [])
    run("fmt", ["cargo", "fmt", "--all", "--check"])
    run("tests-headless", ["cargo", "test", "--workspace", "--all-targets"] + locked)
    run("clippy-headless", ["cargo", "clippy", "--workspace", "--all-targets"] + locked + ["--", "-D", "warnings"])
    if args.gui or args.graphical:
        run("tests-all-features", ["cargo", "test", "--workspace", "--all-features", "--all-targets"] + locked)
        run("clippy-all-features", ["cargo", "clippy", "--workspace", "--all-features", "--all-targets"] + locked + ["--", "-D", "warnings"])
    run("nautilus", [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"])
    run("desktop", ["desktop-file-validate", "data/com.biglinux.Iris.desktop"])
    run("appstream", ["appstreamcli", "validate", "--no-net", "data/com.biglinux.Iris.metainfo.xml"])
    run("schemas", ["glib-compile-schemas", "--strict", "--dry-run", "data"])
    run("translations", ["msgfmt", "--check", "data/po/en.po", "-o", os.devnull])
    run("shell-syntax", ["bash", "-n", "install.sh"])
    for name in ["pkgbuild/PKGBUILD", "pkgbuild/PKGBUILD.local", "packaging/openmandriva/make-vendor-tarball.sh"]:
        run("syntax-" + name.replace("/", "-"), ["bash", "-n", name])
    if args.graphical:
        if not (env.get("DISPLAY") or env.get("WAYLAND_DISPLAY")):
            parser.error("--graphical exige uma sessão gráfica ou Xvfb")
        run("viewer-lifecycle", ["cargo", "test", "-p", "bigiris", "--features", "gui ai"] + locked +
            ["viewer_lifecycle_releases_state_and_workers", "--", "--ignored", "--test-threads=1"],
            extra_env={"GSETTINGS_BACKEND": "memory"})
    if args.performance:
        build = run("probe-build", ["cargo", "build", "--release", "-p", "bigimage-core", "--example", "quality_probe"] + locked)
        if not build:
            probe = str(ROOT / "target/release/examples/quality_probe")
            run("benchmark", [probe, "benchmark", str(args.samples)])
            run("stress", [probe, "stress", "64"])
            run("soak", [probe, "soak", str(args.soak_seconds)], timeout=args.soak_seconds + 120)
    if args.audit:
        audit = shutil.which("cargo-audit") or str(ROOT / "target/quality/tools/bin/cargo-audit")
        command = [audit, "audit", "--json", "--deny", "unsound", "--db", str(ROOT / "target/quality/advisory-db")]
        if args.offline:
            command += ["--no-fetch"]
        run("rustsec", command)
    report = {"utc": stamp, "architecture": platform.machine(), "logical_cpus": os.cpu_count(),
              "gui": args.gui or args.graphical, "graphical": args.graphical,
              "performance": args.performance, "audit": args.audit,
              "passed": all(r["exit_code"] == 0 for r in results), "checks": results}
    (output / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Evidências: {output / 'summary.json'}", flush=True)
    return 0 if report["passed"] else 1

if __name__ == "__main__":
    raise SystemExit(main())
