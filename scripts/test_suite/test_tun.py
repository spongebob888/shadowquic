"""Build an opt-in TUN binary locally, then test real kernel traffic in Docker."""

import argparse
import os
import platform
from pathlib import Path
import subprocess

WORKSPACE = Path(__file__).resolve().parents[2]
FEATURES = "shadowquic-quinn,sunnyquic-noq,ring,plugin-vendored,dns-server,tun"


def main(throughput=False, duration=3, udp_mbps=100, udp_size=1200):
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
    script = "bench_tun.py" if throughput else "test_tun.py"
    command = f"python3 -u /tests/{script}"
    if throughput:
        command += f" --duration {float(duration)} --udp-mbps {float(udp_mbps)} --udp-size {int(udp_size)}"
    print(f"[*] Running TUN {'throughput' if throughput else 'integration'} tests in Docker...", flush=True)
    subprocess.run(
        ["docker", "run", "--rm", "--cap-add=NET_ADMIN", "--device=/dev/net/tun",
         "--sysctl", "net.ipv6.conf.all.disable_ipv6=0",
         "--sysctl", "net.ipv6.conf.default.disable_ipv6=0",
         "--env", "SHADOWQUIC_TUN_TEST_CONTAINER=1",
         "--env", "SHADOWQUIC_BINARY=/shadowquic",
         "--volume", f"{binary}:/shadowquic:ro",
         "--volume", f"{WORKSPACE / 'scripts'}:/tests:ro",
         "ubuntu:22.04", "bash", "-ec",
         "apt-get update -qq && "
         "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3 iproute2 iptables && "
         + command],
        cwd=WORKSPACE, check=True, timeout=max(240, duration * 4 + 180) if throughput else 240,
    )
    print(f"[+] TUN Docker {'throughput' if throughput else 'integration'} tests PASSED", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--throughput", action="store_true", help="run the TCP/UDP echo benchmark")
    parser.add_argument("--duration", type=float, default=3, help="benchmark seconds per protocol/address")
    parser.add_argument("--udp-mbps", type=float, default=100, help="offered UDP payload rate")
    parser.add_argument("--udp-size", type=int, default=1200, help="UDP payload bytes (8..1452)")
    args = parser.parse_args()
    if not 0 < args.duration <= 3600 or not 0 < args.udp_mbps <= 1_000_000 or not 8 <= args.udp_size <= 1452:
        parser.error("duration must be in (0, 3600], udp-mbps in (0, 1000000], and udp-size in 8..1452")
    main(**vars(args))
