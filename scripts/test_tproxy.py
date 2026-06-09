#!/usr/bin/env python3
"""
TPROXY integration test for shadowquic.

Runs inside Docker with NET_ADMIN capability.
Uses the pre-built shadowquic binary (mounted from host, NOT built in Docker).

Flow:
  1. Test traffic to TEST_IP:PORT is MARKed → routed to loopback → PREROUTING
  2. TPROXY rule catches it → delivers to shadowquic on port 1089
  3. Shadowquic reads original dst, makes outbound connection to TEST_IP:PORT
  4. DNAT rule (only for shadowquic group) redirects to 127.0.0.1:8080
  5. HTTP/UDP servers respond → shadowquic proxies back to client

Tests:
  - TCP connectivity via socat (echo)
  - UDP connectivity via socat (echo)
  - Log verification: "accepted tcp connection" and "accepted udp connection"
"""

import subprocess
import sys
import time
import os
import grp

BINARY = "/app/target/release/shadowquic"
CONFIG_PATH = "/tmp/tproxy_config.yaml"
LOG_PATH = "/tmp/shadowquic.log"
BIND_ADDR = "0.0.0.0:1089"
TEST_IP = "10.255.255.1"
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


def setup_iptables():
    """Configure iptables for TPROXY."""
    print("[*] Setting up iptables rules...")

    # Policy routing: traffic marked with 1 goes through table 100
    run("ip rule add fwmark 1 table 100", check=False)
    run("ip route add local 0.0.0.0/0 dev lo table 100", check=False)

    # DIVERT chain for already-established sockets
    run("iptables -t mangle -N DIVERT 2>/dev/null || true", check=False)
    run("iptables -t mangle -F DIVERT", check=False)
    run("iptables -t mangle -A DIVERT -j MARK --set-mark 1", check=False)
    run("iptables -t mangle -A DIVERT -j ACCEPT", check=False)

    # Existing socket connections go to DIVERT
    run("iptables -t mangle -A PREROUTING -p tcp -m socket -j DIVERT", check=False)
    run("iptables -t mangle -A PREROUTING -p udp -m socket -j DIVERT", check=False)

    # TPROXY: ONLY redirect traffic destined to TEST_IP:TEST_PORT to shadowquic
    run(f"iptables -t mangle -A PREROUTING -p tcp -d {TEST_IP} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)
    run(f"iptables -t mangle -A PREROUTING -p udp -d {TEST_IP} --dport {TEST_PORT} -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089", check=False)

    # OUTPUT: skip marking for shadowquic's own traffic (avoid loop)
    run(f"iptables -t mangle -A OUTPUT -m owner --gid-owner {SQ_GROUP} -j RETURN", check=False)

    # OUTPUT: mark test traffic to TEST_IP → routing table 100 → loopback → PREROUTING → TPROXY
    run(f"iptables -t mangle -A OUTPUT -p tcp -d {TEST_IP} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)
    run(f"iptables -t mangle -A OUTPUT -p udp -d {TEST_IP} --dport {TEST_PORT} -j MARK --set-mark 1", check=False)

    # OUTPUT nat: DNAT shadowquic's outbound to local servers (TCP works; UDP via raw socket needs PREROUTING DNAT)
    run(f"iptables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {TEST_IP} -p tcp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)
    run(f"iptables -t nat -A OUTPUT -m owner --gid-owner {SQ_GROUP} -d {TEST_IP} -p udp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)
    # Raw socket UDP might bypass OUTPUT nat, also add PREROUTING DNAT for the raw-sent packets
    run(f"iptables -t nat -A PREROUTING -d {TEST_IP} -p udp --dport {TEST_PORT} -j DNAT --to-destination 127.0.0.1:{TEST_PORT}", check=False)

    print("[+] iptables rules set up.")


def write_config():
    """Write shadowquic TPROXY config file."""
    config = f"""\
inbound:
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


def start_servers():
    """Start TCP and UDP echo servers on 127.0.0.1:8080."""
    # TCP echo server
    subprocess.Popen(
        ["socat", f"TCP-LISTEN:{TEST_PORT},fork,bind=127.0.0.1", "EXEC:cat"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    # UDP echo server
    subprocess.Popen(
        ["socat", f"UDP-LISTEN:{TEST_PORT},fork,bind=127.0.0.1", "EXEC:cat"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)
    print(f"[+] TCP and UDP echo servers started on 127.0.0.1:{TEST_PORT}")


def start_shadowquic():
    """Start shadowquic under the shadowquic group."""
    # Run via sg to set gid
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


def test_tcp():
    """Test TCP connectivity through TPROXY using socat echo."""
    print(f"\n[*] Testing TCP to {TEST_IP}:{TEST_PORT} ...")
    payload = "HELLO_TPROXY_TCP_12345"
    try:
        result = subprocess.run(
            ["bash", "-c", f"echo '{payload}' | socat -t 3 - TCP:{TEST_IP}:{TEST_PORT}"],
            capture_output=True, text=True, timeout=10,
        )
        stdout = result.stdout.strip()
        if payload == stdout:
            print(f"  [PASS] TCP: echo matched ({stdout})")
            return True
        else:
            print(f"  [FAIL] TCP: expected '{payload}', got '{stdout[:200]}'")
            return False
    except subprocess.TimeoutExpired:
        print("  [FAIL] TCP: connection timed out")
        return False
    except Exception as e:
        print(f"  [FAIL] TCP: {e}")
        return False


def test_udp():
    """Test UDP connectivity through TPROXY using socat."""
    print(f"\n[*] Testing UDP to {TEST_IP}:{TEST_PORT} ...")
    payload = "HELLO_TPROXY_UDP_67890"
    try:
        result = subprocess.run(
            ["bash", "-c", f"echo '{payload}' | socat -t 3 - UDP:{TEST_IP}:{TEST_PORT}"],
            capture_output=True, text=True, timeout=10,
        )
        stdout = result.stdout.strip()
        if payload in stdout:
            print(f"  [PASS] UDP: echo matched ({stdout})")
            return True
        else:
            print(f"  [FAIL] UDP: unexpected response: {stdout[:200]}")
            return False
    except subprocess.TimeoutExpired:
        print("  [FAIL] UDP: connection timed out")
        return False
    except Exception as e:
        print(f"  [FAIL] UDP: {e}")
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

    # Print all non-empty log lines for visibility
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


def main():
    results = {}

    if not os.path.isfile(BINARY):
        sys.exit(f"Binary not found: {BINARY}")

    # Install dependency
    run("command -v socat >/dev/null 2>&1 || (apt-get update -qq && apt-get install -y -qq socat)", check=False)

    # Create shadowquic group
    try:
        grp.getgrnam(SQ_GROUP)
        print(f"[*] Group '{SQ_GROUP}' already exists")
    except KeyError:
        run(f"groupadd {SQ_GROUP}")
        print(f"[+] Created group '{SQ_GROUP}'")

    write_config()
    setup_iptables()
    start_servers()
    proc = start_shadowquic()

    try:
        results["tcp"] = test_tcp()
        results["udp"] = test_udp()
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
