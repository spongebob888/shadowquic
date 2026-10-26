#!/usr/bin/env python3
"""Kernel TUN integration test, run only in the isolated NET_ADMIN Docker runner.

Uses a prebuilt binary. Client routes enter TUN; group-specific DNAT keeps the
proxy's literal-IP outbounds out of TUN. Fake domains resolve to distinct local
echo servers, checking restoration and original reply addresses end to end.
"""

from contextlib import ExitStack
import grp
import ipaddress
import json
import os
from pathlib import Path
import socket
import socketserver
import struct
import subprocess
import threading
import time

BINARY = os.environ.get("SHADOWQUIC_BINARY", "/shadowquic")
DEVICE = "sq-tun0"
TEST_V4 = "198.51.100.10"
TEST_V6 = "2001:db8:100::10"
FAKE_V4 = "198.18.16.0/20"
FAKE_V6 = "2001:2:100::/64"
PORT = 8080
CONFIG = Path("/tmp/tun-config.yaml")
LOG = Path("/tmp/tun-shadowquic.log")


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=15)


class TcpEcho(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(10)
        data = bytearray()
        while chunk := self.request.recv(65536):
            data.extend(chunk)
            if len(data) > 262144:
                raise RuntimeError("unexpected oversized TCP test payload")
        # Wait for client EOF before responding, exercising TCP half-closes.
        self.request.sendall(self.server.prefix + data)


class UdpEcho(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        sock.sendto(self.server.prefix + data, self.client_address)


class TcpServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class UdpServer(socketserver.ThreadingUDPServer):
    allow_reuse_address = True
    daemon_threads = True


class TcpServerV6(TcpServer):
    address_family = socket.AF_INET6


class UdpServerV6(UdpServer):
    address_family = socket.AF_INET6


def echo_servers(resources):
    for ip, prefix in [("127.0.0.1", b"v4:"), ("127.0.0.2", b"alt-v4:"),
                       ("::1", b"v6:"), ("::2", b"alt-v6:")]:
        classes = (TcpServerV6, UdpServerV6) if ":" in ip else (TcpServer, UdpServer)
        for server_class, handler in zip(classes, (TcpEcho, UdpEcho)):
            server = resources.enter_context(server_class((ip, PORT), handler))
            server.prefix = prefix
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            resources.callback(server.shutdown)


def write_config():
    CONFIG.write_text(f"""inbounds:
  - tag: tun-in
    type: tun
    name: {DEVICE}
    mtu: 1500
    max-sessions: 128
    tcp-idle-timeout: 30
    udp-idle-timeout: 30
dns:
  - tag: fake
    type: dns-fakeip
    bind-addr: 127.0.0.1:1053
    ipv4-range: {FAKE_V4}
    ipv6-range: {FAKE_V6}
outbounds:
  - tag: direct
    type: direct
router:
  src: |
    return function(ctx)
      assert(ctx.inbound_tag == "tun-in")
      assert(ctx.src_ip_v4 ~= nil or ctx.src_ip_v6 ~= nil)
      if ctx.dst_port == 53 then return "fake" end
      if ctx.dst_domain ~= nil then
        assert(ctx.dst_domain == "fake4.test" or ctx.dst_domain == "fake4b.test"
            or ctx.dst_domain == "fake6.test" or ctx.dst_domain == "fake6b.test")
        assert(ctx.dst_ip_v4 == nil and ctx.dst_ip_v6 == nil)
      else
        assert(ctx.dst_ip_v4 == "{TEST_V4}" or ctx.dst_ip_v6 == "{TEST_V6}",
               "fake-IP restoration did not run before routing")
      end
      return "direct"
    end
log-level: trace
""")
    with open("/etc/hosts", "a") as hosts:
        hosts.write("\n127.0.0.1 fake4.test\n127.0.0.2 fake4b.test\n"
                    "::1 fake6.test\n::2 fake6b.test\n")


def setup_routes(gid):
    run("ip", "route", "add", TEST_V4 + "/32", "dev", DEVICE)
    run("ip", "-6", "route", "add", TEST_V6 + "/128", "dev", DEVICE)
    run("ip", "route", "add", FAKE_V4, "dev", DEVICE)
    run("ip", "-6", "route", "add", FAKE_V6, "dev", DEVICE)
    # Only the proxy's own literal-IP requests are translated to the echo
    # servers. Ordinary client sockets must traverse the kernel TUN route.
    for command, ip, target in [("iptables", TEST_V4, f"127.0.0.1:{PORT}"),
                                 ("ip6tables", TEST_V6, f"[::1]:{PORT}")]:
        for protocol in ("tcp", "udp"):
            run(command, "-t", "nat", "-A", "OUTPUT", "-m", "owner", "--gid-owner", str(gid),
                "-d", ip, "-p", protocol, "--dport", str(PORT), "-j", "DNAT", "--to-destination", target)


def wait_for_device(proc):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"shadowquic exited early ({proc.returncode})")
        result = subprocess.run(["ip", "-j", "address", "show", "dev", DEVICE],
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            addresses = json.loads(result.stdout)[0]["addr_info"]
            if any(addr["local"] == "fdfe:dcba:9876::1"
                   and not addr.get("tentative", False)
                   and "tentative" not in addr.get("flags", []) for addr in addresses):
                return
        time.sleep(0.05)
    raise TimeoutError("TUN device did not become ready")


def tcp_test(ip, prefix):
    payload = (b"TUN_TCP_HALF_CLOSE_" * 4096)[:65536]
    with socket.create_connection((ip, PORT), timeout=10) as sock:
        sock.settimeout(10)
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        reply = bytearray()
        while chunk := sock.recv(65536):
            reply.extend(chunk)
    assert reply == prefix + payload, f"TCP echo mismatch for {ip}"
    print(f"  [PASS] TCP {ip}: data and half-close", flush=True)


def udp_test(ips, prefixes):
    family = socket.AF_INET6 if ":" in ips[0] else socket.AF_INET
    with socket.socket(family, socket.SOCK_DGRAM) as sock:
        sock.settimeout(10)
        # Reuse a client socket while destinations change, as in TPROXY.
        for i, (ip, prefix) in enumerate(zip(ips, prefixes)):
            payload = f"TUN_UDP_PACKET_{i}".encode()
            sock.sendto(payload, (ip, PORT))
            reply, source = sock.recvfrom(4096)
            assert reply == prefix + payload, f"UDP echo mismatch for {ip}: {reply!r}"
            assert ipaddress.ip_address(source[0]) == ipaddress.ip_address(ip), source
            assert source[1] == PORT, source
    print(f"  [PASS] UDP {ips}: destinations and reply addresses", flush=True)


def skip_name(message, offset):
    # We only need to skip names. Compressed names terminate with a pointer.
    while True:
        length = message[offset]
        offset += 1
        if length == 0:
            return offset
        if length & 0xC0 == 0xC0:
            return offset + 1
        if length > 63:
            raise ValueError("invalid DNS name")
        offset += length


def dns_query(name, record_type, server):
    question = b"".join(bytes([len(label)]) + label.encode("ascii") for label in name.split("."))
    request = struct.pack("!6H", 42, 0x100, 1, 0, 0, 0) + question + b"\0" + struct.pack("!HH", record_type, 1)
    family = socket.AF_INET6 if ":" in server else socket.AF_INET
    with socket.socket(family, socket.SOCK_DGRAM) as sock:
        sock.settimeout(10)
        sock.sendto(request, (server, 53))
        response, source = sock.recvfrom(4096)
    assert ipaddress.ip_address(source[0]) == ipaddress.ip_address(server) and source[1] == 53
    ident, flags, questions, answers, _, _ = struct.unpack_from("!6H", response)
    assert ident == 42 and flags & 0x8000 and flags & 0xF == 0 and answers > 0
    offset = 12
    for _ in range(questions):
        offset = skip_name(response, offset) + 4
    ips = []
    for _ in range(answers):
        offset = skip_name(response, offset)
        ty, cls, ttl, length = struct.unpack_from("!HHIH", response, offset)
        offset += 10
        if ty == record_type and cls == 1:
            assert ttl == 60
            ips.append(str(ipaddress.ip_address(response[offset:offset + length])))
        offset += length
    assert len(ips) == 1, ips
    return ips[0]


def stop(proc):
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
    assert proc.returncode == 0, f"shadowquic failed graceful shutdown ({proc.returncode})"
    assert subprocess.run(["ip", "link", "show", "dev", DEVICE], capture_output=True, timeout=5).returncode != 0, "TUN device leaked after shutdown"
    print("  [PASS] Graceful shutdown removes TUN device", flush=True)


def main():
    if os.environ.get("SHADOWQUIC_TUN_TEST_CONTAINER") != "1" or os.geteuid() != 0:
        raise RuntimeError("Run via scripts/test_suite/test_tun.py inside its isolated Docker container")
    if not Path(BINARY).is_file():
        raise FileNotFoundError(BINARY)
    run("groupadd", "--system", "shadowquic-tun-test")
    gid = grp.getgrnam("shadowquic-tun-test").gr_gid
    run("ip", "-6", "addr", "add", "::2/128", "dev", "lo")
    write_config()
    proc = None
    try:
        with ExitStack() as resources:
            echo_servers(resources)
            with LOG.open("w") as log:
                proc = subprocess.Popen([BINARY, "-c", str(CONFIG)], stdout=log, stderr=subprocess.STDOUT, group=gid)
            wait_for_device(proc)
            setup_routes(gid)
            tcp_test(TEST_V4, b"v4:")
            tcp_test(TEST_V6, b"v6:")
            udp_test([TEST_V4], [b"v4:"])
            udp_test([TEST_V6], [b"v6:"])
            for names, ty, server, pool, prefixes in [
                (["fake4.test", "fake4b.test"], 1, TEST_V4, FAKE_V4, [b"v4:", b"alt-v4:"]),
                (["fake6.test", "fake6b.test"], 28, TEST_V6, FAKE_V6, [b"v6:", b"alt-v6:"]),
            ]:
                ips = [dns_query(name, ty, server) for name in names]
                assert all(ipaddress.ip_address(ip) in ipaddress.ip_network(pool) for ip in ips), ips
                assert ips[0] != ips[1]
                assert dns_query(names[0], ty, server) == ips[0], "fake-IP mapping changed"
                print(f"  [PASS] Intercepted DNS {server}: {ips}", flush=True)
                tcp_test(ips[0], prefixes[0])
                udp_test(ips, prefixes)
            stop(proc)
            proc = None
            content = LOG.read_text()
            assert "fakeip mapping:" in content, "fake-IP restoration was not logged"
            assert "tun-in" in content, "missing inbound tag"
            assert "error accepting request" not in content and "TUN packet loop stopped" not in content, content
            print("All TUN tests PASSED!", flush=True)
    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        if LOG.exists():
            print("--- shadowquic TUN log ---", flush=True)
            print(LOG.read_text(), flush=True)


if __name__ == "__main__":
    main()
