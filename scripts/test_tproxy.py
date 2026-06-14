#!/usr/bin/env python3
"""
TPROXY integration test for shadowquic.

Runs inside Docker with NET_ADMIN capability.
Uses the pre-built shadowquic binary (mounted from host, NOT built in Docker).

Tests IPv4 (and IPv6 when available):
  - TCP echo via socat
  - UDP echo via socat
  - Log verification: "accepted tcp connection" / "accepted udp connection"
"""

import subprocess
import sys
import time
import os
import grp

BINARY = os.environ.get("SHADOWQUIC_BIN", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "shadowquic"))
CONFIG_PATH = "/tmp/tproxy_config.yaml"
LOG_PATH = "/tmp/shadowquic.log"
BIND_ADDR = "[::]:1089"
TEST_IP4 = "10.255.255.1"
TEST_IP6 = "fd00:dead:beef::1"
TEST_PORT = 8080
SQ_GROUP = "shadowquic"


def run(cmd, check=True, capture=False, timeout=15):
    kwargs = {}
    if capture:
        kwargs["stdout"] = subprocess.PIPE
        kwargs["stderr"] = subprocess.STDOUT
    result = subprocess.run(cmd, shell=True, text=True, timeout=timeout, **kwargs)
    if check and result.returncode != 0:
        sys.exit(f"Command failed (exit {result.returncode}): {cmd}")
    return result


def setup_iptables(ipv6_available):
    """Configure iptables and ip6tables for TPROXY (IPv4 + optionally IPv6)."""
    print("[*] Setting up iptables rules...")

    # --- IPv4 routing & iptables ---
    run("ip rule add fwmark 1 table 100", check=False)
    run("ip route add local 0.0.0.0/0 dev lo table 100", check=False)

    for ipt in ["iptables", "ip6tables"]:
        if not ipv6_available and ipt == "ip6tables":
            continue
        run(f"{ipt} -t mangle -N DIVERT 2>/dev/null || true", check=False)
        run(f"{ipt} -t mangle -F DIVERT", check=False)
        run(f"{ipt} -t mangle -A DIVERT -j MARK --set-mark 1", check=False)
        run(f"{ipt} -t mangle -A DIVERT -j ACCEPT", check=False)
        run(f"{ipt} -t mangle -A PREROUTING -p tcp -m socket -j DIVERT", check=False)
        run(f"{ipt} -t mangle -A PREROUTING -p udp -m socket -j DIVERT", check=False)

    # IPv4 test IP
    dst4 = TEST_IP4
    run(f"iptables -t mangle -A PREROUTING -p tcp -d {dst4} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)
    run(f"iptables -t mangle -A PREROUTING -p udp -d {dst4} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)
    run(f"iptables -t mangle -A OUTPUT -m owner --gid-owner {SQ_GROUP} -j RETURN", check=False)
    run(f"iptables -t mangle -A OUTPUT -p tcp -d {dst4} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)
    run(f"iptables -t mangle -A OUTPUT -p udp -d {dst4} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)
    run(f"iptables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {dst4} -p tcp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)
    run(f"iptables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {dst4} -p udp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)
    run(f"iptables -t nat -A PREROUTING -d {dst4} -p udp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)

    if ipv6_available:
        print("[*] IPv6 available, setting up ip6tables rules...")
        dst6 = TEST_IP6
        run("ip -6 rule add fwmark 1 table 100", check=False)
        run("ip -6 route add local ::/0 dev lo table 100", check=False)
        run(f"ip6tables -t mangle -A PREROUTING -p tcp -d {dst6} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)
        run(f"ip6tables -t mangle -A PREROUTING -p udp -d {dst6} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)
        run(f"ip6tables -t mangle -A OUTPUT -m owner --gid-owner {SQ_GROUP} -j RETURN", check=False)
        run(f"ip6tables -t mangle -A OUTPUT -p tcp -d {dst6} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)
        run(f"ip6tables -t mangle -A OUTPUT -p udp -d {dst6} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)
        run(f"ip6tables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {dst6} -p tcp --dport {TEST_PORT} -j DNAT --to-destination [::1]:{TEST_PORT}", check=False)
        run(f"ip6tables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {dst6} -p udp --dport {TEST_PORT} -j DNAT --to-destination [::1]:{TEST_PORT}", check=False)
        run(f"ip6tables -t nat -A PREROUTING -d {dst6} -p udp --dport {TEST_PORT} -j DNAT --to-destination [::1]:{TEST_PORT}", check=False)
    else:
        print("[*] IPv6 unavailable, skipping IPv6 TPROXY tests")

    print("[+] iptables rules set up.")


def write_config():
    """Write shadowquic TPROXY config (dual-stack)."""
    config = f"""inbound:
  type: tproxy
  bind-addr: "{BIND_ADDR}"
outbound:
  type: direct
log-level: trace
"""
    os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
    with open(CONFIG_PATH, "w") as f:
        f.write(config)
    print(f"[+] Config written to {CONFIG_PATH}")


def start_servers(ipv6_available):
    """Start TCP and UDP echo servers on IPv4+IPv6 loopback."""
    subprocess.Popen(
        ["socat", f"TCP4-LISTEN:{TEST_PORT},fork,bind=127.0.0.1", "EXEC:cat"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    subprocess.Popen(
        ["socat", f"UDP4-LISTEN:{TEST_PORT},fork,bind=127.0.0.1", "EXEC:cat"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    if ipv6_available:
        subprocess.Popen(
            ["socat", f"TCP6-LISTEN:{TEST_PORT},fork,bind=[::1]", "EXEC:cat"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        subprocess.Popen(
            ["socat", f"UDP6-LISTEN:{TEST_PORT},fork,bind=[::1]", "EXEC:cat"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        print(f"[+] Echo servers on 127.0.0.1:{TEST_PORT} and [::1]:{TEST_PORT}")
    else:
        print(f"[+] Echo servers on 127.0.0.1:{TEST_PORT}")


def start_shadowquic():
    """Start shadowquic under the shadowquic group."""
    with open(LOG_PATH, "w") as logfile:
        proc = subprocess.Popen(
            ["sg", SQ_GROUP, "-c", f"{BINARY} -c {CONFIG_PATH}"],
            stdout=logfile, stderr=subprocess.STDOUT,
        )
    time.sleep(2)
    if proc.poll() is not None:
        with open(LOG_PATH) as f:
            print(f"[!] shadowquic exited early:\n{f.read()}")
        sys.exit(1)
    print(f"[+] shadowquic started (PID={proc.pid}), logging to {LOG_PATH}")
    return proc


def test_tcp(ip, label):
    """Test TCP connectivity through TPROXY using socat echo."""
    print(f"\n[*] Testing {label} TCP to [{ip}]:{TEST_PORT} ...")
    payload = f"HELLO_{label.upper().replace('-','_')}_12345"
    addr = f"[{ip}]" if ":" in ip else ip
    try:
        result = subprocess.run(
            ["bash", "-c", f"echo '{payload}' | socat -t 3 - TCP:{addr}:{TEST_PORT}"],
            capture_output=True, text=True, timeout=10,
        )
        stdout = result.stdout.strip()
        if payload == stdout:
            print(f"  [PASS] {label} TCP: echo matched ({stdout})")
            return True
        else:
            print(f"  [FAIL] {label} TCP: expected '{payload}', got '{stdout[:200]}'")
            return False
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] {label} TCP: connection timed out")
        return False
    except Exception as e:
        print(f"  [FAIL] {label} TCP: {e}")
        return False


def test_udp(ip, label):
    """Test UDP connectivity through TPROXY using socat."""
    print(f"\n[*] Testing {label} UDP to [{ip}]:{TEST_PORT} ...")
    payload = f"HELLO_{label.upper().replace('-','_')}_67890"
    addr = f"[{ip}]" if ":" in ip else ip
    try:
        result = subprocess.run(
            ["bash", "-c", f"echo '{payload}' | socat -t 3 - UDP:{addr}:{TEST_PORT}"],
            capture_output=True, text=True, timeout=10,
        )
        stdout = result.stdout.strip()
        if payload in stdout:
            print(f"  [PASS] {label} UDP: echo matched ({stdout})")
            return True
        else:
            print(f"  [FAIL] {label} UDP: unexpected response: {stdout[:200]}")
            return False
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] {label} UDP: connection timed out")
        return False
    except Exception as e:
        print(f"  [FAIL] {label} UDP: {e}")
        return False


def check_logs():
    """Verify shadowquic logs show accepted tproxy connections."""
    print(f"\n[*] Checking shadowquic logs for TPROXY accept messages ...")
    try:
        with open(LOG_PATH) as f:
            log_content = f.read()
    except Exception as e:
        print(f"  [FAIL] Cannot read log: {e}")
        return False

    print("  --- shadowquic log output ---")
    for line in log_content.splitlines():
        if line.strip():
            print(f"  {line.strip()}")
    print("  --- end log ---")

    tcp_accepted = "accepted tcp connection" in log_content
    udp_accepted = "accepted udp connection" in log_content

    ok = True
    if tcp_accepted:
        print("  [PASS] Log shows TCP connection accepted by TPROXY")
    else:
        print("  [FAIL] Log missing 'accepted tcp connection'")
        ok = False
    if udp_accepted:
        print("  [PASS] Log shows UDP connection accepted by TPROXY")
    else:
        print("  [FAIL] Log missing 'accepted udp connection'")
        ok = False
    return ok


def has_ipv6():
    """Check if IPv6 TPROXY is available."""
    try:
        r = subprocess.run("ip -6 addr show lo", shell=True, capture_output=True, text=True, timeout=3)
        if "::1" not in r.stdout:
            return False
        r = subprocess.run("ip -6 addr add fd00::1/128 dev lo", shell=True, capture_output=True, timeout=3)
        if r.returncode != 0:
            return False
        subprocess.run("ip -6 addr del fd00::1/128 dev lo", shell=True, capture_output=True, timeout=3)
        return True
    except Exception:
        return False


def main():
    results = {}

    if not os.path.isfile(BINARY):
        sys.exit(f"Binary not found: {BINARY}")

    run("command -v socat >/dev/null 2>&1 || (echo 'Acquire::ForceIPv4 true;' > /etc/apt/apt.conf.d/99force-ipv4 && apt-get update -qq && apt-get install -y -qq socat)", check=False)

    try:
        grp.getgrnam(SQ_GROUP)
        print(f"[*] Group '{SQ_GROUP}' already exists")
    except KeyError:
        run(f"groupadd {SQ_GROUP}")
        print(f"[+] Created group '{SQ_GROUP}'")

    ipv6_ok = has_ipv6()

    write_config()
    setup_iptables(ipv6_ok)
    start_servers(ipv6_ok)
    proc = start_shadowquic()

    try:
        results["tcp-v4"] = test_tcp(TEST_IP4, "IPv4")
        results["udp-v4"] = test_udp(TEST_IP4, "IPv4")
        if ipv6_ok:
            results["tcp-v6"] = test_tcp(TEST_IP6, "IPv6")
            results["udp-v6"] = test_udp(TEST_IP6, "IPv6")
        else:
            print("\n[*] Skipping IPv6 tests (Docker runtime limitation)")
        results["logs"] = check_logs()
    finally:
        print("\n[*] Cleaning up...")
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    # Summary
    print("\n" + "=" * 50)
    print("TPROXY TEST RESULTS")
    print("=" * 50)
    for name, passed in results.items():
        status = "PASS" if passed else "FAIL"
        print(f"  {name:8s}: {status}")
    print("=" * 50)

    if all(results.values()):
        print("All tests PASSED!")
    else:
        print("Some tests FAILED!")
        sys.exit(1)


if __name__ == "__main__":
    main()
