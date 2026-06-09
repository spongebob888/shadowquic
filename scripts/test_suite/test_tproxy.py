"""
TPROXY integration test for shadowquic.

Builds locally (--features tproxy), then runs inside Docker with NET_ADMIN.
Tests TCP and UDP connectivity through TPROXY and verifies logs.
"""

import subprocess as sp
import sys
import os

WORKSPACE = os.path.abspath(os.path.join(os.path.dirname(__file__), "../.."))

# 1. Build locally (not in Docker) with tproxy feature
print("[*] Building shadowquic with --features tproxy ...")
result = sp.run(
    ["cargo", "build", "--release", "--features", "tproxy"],
    cwd=WORKSPACE,
    capture_output=True,
    text=True,
    timeout=300,
)
if result.returncode != 0:
    lines = result.stderr.splitlines()
    for line in lines[-30:]:
        print(f"    {line}")
    sys.exit(1)
print("[+] Build succeeded")

# 2. Run Docker test
print("[*] Running TPROXY test in Docker ...")
result = sp.run(
    [
        "docker", "run", "--rm", "--cap-add=NET_ADMIN",
        "-v", f"{WORKSPACE}:/app",
        "ubuntu:22.04",
        "bash", "-c",
        "apt-get update -qq && "
        "apt-get install -y -qq python3 socat iproute2 iptables 2>&1 | tail -1 && "
        "python3 /app/scripts/test_tproxy.py",
    ],
    capture_output=True,
    text=True,
    timeout=120,
)

print(result.stdout)
if result.stderr:
    print(result.stderr, file=sys.stderr)

assert "All tests PASSED!" in result.stdout, "TPROXY tests failed!"
print("\n[+] TPROXY integration test PASSED")
