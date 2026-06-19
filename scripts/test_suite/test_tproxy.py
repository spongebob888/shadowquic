"""
TPROXY integration test for shadowquic.

Builds locally (--features tproxy), then runs inside Docker with NET_ADMIN.
Tests TCP and UDP connectivity through TPROXY and verifies logs.
"""

import subprocess as sp
import sys
import os

WORKSPACE = os.path.abspath(os.path.join(os.path.dirname(__file__), "../.."))
MUSL_TARGET = "x86_64-unknown-linux-musl"
MUSL_BINARY = f"/app/target/{MUSL_TARGET}/release/shadowquic"

# 1. Build locally (not in Docker) with tproxy feature
print(f"[*] Building shadowquic with --features tproxy for {MUSL_TARGET} ...")
result = sp.run(
    ["cargo", "build", "--release", "--features", "tproxy", "--target", MUSL_TARGET],
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
docker_args = [
    "docker", "run", "--rm", "--cap-add=NET_ADMIN",
    "--sysctl", "net.ipv6.conf.all.disable_ipv6=0",
    "--sysctl", "net.ipv6.conf.default.disable_ipv6=0",
]
docker_args.extend([
    "-v", f"{WORKSPACE}:/app",
    "ubuntu:22.04",
    "bash", "-c",
    "apt-get update -qq && "
    "apt-get install -y -qq python3 socat iproute2 iptables 2>&1 | tail -1 && "
    f"SHADOWQUIC_BINARY={MUSL_BINARY} python3 /app/scripts/test_tproxy.py",
])
result = sp.run(
    docker_args,
    capture_output=True,
    text=True,
    timeout=120,
)

print(result.stdout)
if result.stderr:
    print(result.stderr, file=sys.stderr)

assert "All tests PASSED!" in result.stdout, "TPROXY tests failed!"
print("\n[+] TPROXY integration test PASSED")
