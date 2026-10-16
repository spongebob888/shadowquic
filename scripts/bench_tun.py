#!/usr/bin/env python3
"""Measure verified TCP/UDP echo throughput through TUN -> direct in Docker."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import grp
import json
import os
import socket
import socketserver
import struct
import subprocess
import threading
import time

import test_tun as tun

PATTERN = bytes(range(256))
TCP_CHUNK = PATTERN * 256


class TcpEcho(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(15)
        while chunk := self.request.recv(65536):
            self.request.sendall(chunk)


class UdpEcho(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        sock.sendto(data, self.client_address)


class UdpServerV6(socketserver.UDPServer):
    address_family = socket.AF_INET6


def echo_servers(resources):
    # One persistent UDP worker per address avoids spawning a thread per packet.
    for ip, tcp, udp in [("127.0.0.1", tun.TcpServer, socketserver.UDPServer),
                         ("::1", tun.TcpServerV6, UdpServerV6)]:
        for server_class, handler in [(tcp, TcpEcho), (udp, UdpEcho)]:
            server = resources.enter_context(server_class((ip, tun.PORT), handler))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            resources.callback(server.shutdown)


def tcp_echo(ip, duration):
    with socket.create_connection((ip, tun.PORT), timeout=15) as sock:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        start = time.monotonic()

        def send():
            sent = 0
            while time.monotonic() - start < duration:
                sock.sendall(TCP_CHUNK)
                sent += len(TCP_CHUNK)
            sock.shutdown(socket.SHUT_WR)
            return sent

        with ThreadPoolExecutor(max_workers=1) as pool:
            sender = pool.submit(send)
            received = 0
            while chunk := sock.recv(len(TCP_CHUNK)):
                offset = received % len(PATTERN)
                expected = (PATTERN * 257)[offset:offset + len(chunk)]
                if chunk != expected:
                    raise RuntimeError(f"TCP echo payload mismatch for {ip}")
                received += len(chunk)
            elapsed = time.monotonic() - start
            sent = sender.result()
        if received != sent or not received:
            raise RuntimeError(f"TCP echo incomplete for {ip}: {received}/{sent} bytes")
    return {"protocol": "tcp", "ip": ip, "sent_bytes": sent,
            "received_bytes": received, "elapsed_seconds": elapsed,
            "received_mbps": received * 8 / elapsed / 1_000_000}


def udp_echo(ip, duration, size, mbps):
    family = socket.AF_INET6 if ":" in ip else socket.AF_INET
    body = (PATTERN * ((size + 255) // 256))[:size - 8]
    done = threading.Event()
    with socket.socket(family, socket.SOCK_DGRAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
        sock.settimeout(0.1)
        sock.connect((ip, tun.PORT))
        start = time.monotonic()

        def send():
            sent = 0
            try:
                while not done.is_set() and time.monotonic() - start < duration:
                    packet = struct.pack("!Q", sent) + body
                    if sock.send(packet) != size:
                        raise RuntimeError("partial UDP send")
                    sent += 1
                    delay = start + sent * size * 8 / (mbps * 1_000_000) - time.monotonic()
                    if delay > 0:
                        time.sleep(min(delay, max(0, start + duration - time.monotonic())))
                return sent, time.monotonic() - start
            finally:
                done.set()

        with ThreadPoolExecutor(max_workers=1) as pool:
            sender = pool.submit(send)
            seen = set()
            duplicates = 0
            last_reply = start
            deadline = start + duration + 15
            try:
                while time.monotonic() < deadline:
                    try:
                        packet = sock.recv(65536)
                    except socket.timeout:
                        if done.is_set() and time.monotonic() - last_reply >= 0.5:
                            break
                        continue
                    if len(packet) != size or packet[8:] != body:
                        raise RuntimeError(f"UDP echo payload mismatch for {ip}")
                    seq, = struct.unpack_from("!Q", packet)
                    if seq in seen:
                        duplicates += 1
                    seen.add(seq)
                    last_reply = time.monotonic()
            finally:
                done.set()
            sent, sending_elapsed = sender.result()
        if not seen or max(seen) >= sent:
            raise RuntimeError(f"UDP echo missing or invalid for {ip}")
        # Include the sending interval, but exclude the idle drain grace period.
        elapsed = max(sending_elapsed, last_reply - start)
    received = len(seen)
    return {"protocol": "udp", "ip": ip, "sent_packets": sent,
            "received_packets": received, "duplicate_packets": duplicates,
            "loss_percent": (sent - received) * 100 / sent,
            "sent_bytes": sent * size, "received_bytes": received * size,
            "elapsed_seconds": elapsed,
            "received_mbps": received * size * 8 / elapsed / 1_000_000,
            "offered_mbps": sent * size * 8 / sending_elapsed / 1_000_000,
            "target_mbps": mbps}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=float, default=3, help="seconds per protocol/address")
    parser.add_argument("--udp-mbps", type=float, default=100, help="offered UDP payload rate")
    parser.add_argument("--udp-size", type=int, default=1200, help="UDP payload bytes (8..1452)")
    args = parser.parse_args()
    if not 0 < args.duration <= 3600 or not 0 < args.udp_mbps <= 1_000_000:
        parser.error("duration must be in (0, 3600] and udp-mbps in (0, 1000000]")
    if not 8 <= args.udp_size <= 1452:
        parser.error("udp-size must be 8..1452 to fit the IPv6 MTU")
    if os.environ.get("SHADOWQUIC_TUN_TEST_CONTAINER") != "1" or os.geteuid() != 0:
        raise RuntimeError("Run scripts/test_suite/test_tun.py --throughput instead")

    tun.run("groupadd", "--system", "shadowquic-tun-bench")
    gid = grp.getgrnam("shadowquic-tun-bench").gr_gid
    tun.CONFIG.write_text(f"""inbounds:
  - tag: tun-in
    type: tun
    name: {tun.DEVICE}
    mtu: 1500
    default-outbound: direct
outbounds:
  - tag: direct
    type: direct
log-level: warn
""")
    proc = None
    results = []
    try:
        with ExitStack() as resources:
            echo_servers(resources)
            with tun.LOG.open("w") as log:
                proc = subprocess.Popen([tun.BINARY, "-c", str(tun.CONFIG)],
                                        stdout=log, stderr=subprocess.STDOUT, group=gid)
            tun.wait_for_device(proc)
            tun.setup_routes(gid)
            for ip in (tun.TEST_V4, tun.TEST_V6):
                for benchmark, extra in [(tcp_echo, ()), (udp_echo, (args.udp_size, args.udp_mbps))]:
                    result = benchmark(ip, args.duration, *extra)
                    results.append(result)
                    loss = f", loss {result['loss_percent']:.2f}%" if "loss_percent" in result else ""
                    print(f"{result['protocol'].upper()} {ip}: {result['received_mbps']:.2f} Mbps"
                          f" echoed ({result['received_bytes']} bytes{loss})", flush=True)
            tun.stop(proc)
            proc = None
        print(json.dumps({"path": "kernel TUN -> direct -> echo", "results": results}, indent=2), flush=True)
    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        if tun.LOG.exists() and tun.LOG.stat().st_size:
            print(tun.LOG.read_text(), flush=True)


if __name__ == "__main__":
    main()
