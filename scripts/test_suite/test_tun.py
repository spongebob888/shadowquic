"""Build an opt-in TUN binary locally, then test real kernel traffic in Docker."""

import os
import platform
from pathlib import Path
import subprocess

WORKSPACE = Path(__file__).resolve().parents[2]
FEATURES = "shadowquic-quinn,sunnyquic-noq,ring,plugin-vendored,dns-server,tun"


def main():
    binary = os.environ.get("SHADOWQUIC_TUN_TEST_BINARY")
    if binary is None:
        target = {
            "x86_64": "x86_64-unknown-linux-musl",
            "aarch64": "aarch64-unknown-linux-musl",
        }.get(platform.machine())
        if target is None:
            raise RuntimeError("Set SHADOWQUIC_TUN_TEST_BINARY to a static Linux binary for this host")
        print(f"[*] Building TUN test binary for {target} (without tproxy)...", flush=True)
        subprocess.run(
            ["cargo", "build", "--release", "--locked", "--no-default-features",
             "--features", FEATURES, "--target", target],
            cwd=WORKSPACE, check=True, timeout=900,
        )
        binary = WORKSPACE / "target" / target / "release" / "shadowquic"
    binary = Path(binary).resolve(strict=True)
    print("[*] Running TUN tests in Docker...", flush=True)
    subprocess.run(
        ["docker", "run", "--rm", "--cap-add=NET_ADMIN", "--device=/dev/net/tun",
         "--sysctl", "net.ipv6.conf.all.disable_ipv6=0",
         "--sysctl", "net.ipv6.conf.default.disable_ipv6=0",
         "--env", "SHADOWQUIC_TUN_TEST_CONTAINER=1",
         "--env", "SHADOWQUIC_BINARY=/shadowquic",
         "--volume", f"{binary}:/shadowquic:ro",
         "--volume", f"{WORKSPACE / 'scripts/test_tun.py'}:/test_tun.py:ro",
         "ubuntu:22.04", "bash", "-ec",
         "apt-get update -qq && "
         "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3 iproute2 iptables && "
         "python3 -u /test_tun.py"],
        cwd=WORKSPACE, check=True, timeout=240,
    )
    print("[+] TUN Docker integration tests PASSED", flush=True)


if __name__ == "__main__":
    main()
