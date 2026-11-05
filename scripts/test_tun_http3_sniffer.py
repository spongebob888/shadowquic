#!/usr/bin/env python3
"""Local curl HTTP/3 test through the real kernel TUN device (Docker only)."""
import asyncio
import grp
import os
from pathlib import Path
import subprocess
import sys
import time

import test_tun as tun

HOST = "sniffer.test"
BLOCKED = "blocked.test"
PORT = 8443
BODY = b"HTTP3 through Shadowquic TUN\n"
CERT = Path("/tmp/http3-cert.pem")
KEY = Path("/tmp/http3-key.pem")
READY = Path("/tmp/http3-ready")
SERVER_LOG = Path("/tmp/http3-server.log")


async def serve():
    from aioquic.asyncio import QuicConnectionProtocol, serve as quic_serve
    from aioquic.h3.connection import H3_ALPN, H3Connection
    from aioquic.h3.events import HeadersReceived
    from aioquic.quic.configuration import QuicConfiguration
    from aioquic.quic.events import ProtocolNegotiated

    class Server(QuicConnectionProtocol):
        http = None

        def quic_event_received(self, event):
            if isinstance(event, ProtocolNegotiated):
                self.http = H3Connection(self._quic)
            if self.http is not None:
                for request in self.http.handle_event(event):
                    if isinstance(request, HeadersReceived):
                        self.http.send_headers(request.stream_id, [
                            (b":status", b"200"), (b"content-length", str(len(BODY)).encode()),
                        ])
                        self.http.send_data(request.stream_id, BODY, end_stream=True)
                        self.transmit()

    config = QuicConfiguration(is_client=False, alpn_protocols=H3_ALPN)
    config.load_cert_chain(str(CERT), str(KEY))
    server = await quic_serve("127.0.0.1", PORT, configuration=config, create_protocol=Server)
    READY.touch()
    try:
        await asyncio.Future()
    finally:
        server.close()


def curl(host):
    return subprocess.run([
        os.environ["CURL"], "--http3-only", "--interface", tun.DEVICE,
        "--noproxy", "*", "--resolve", f"{host}:{PORT}:{tun.TEST_V4}",
        "--cacert", str(CERT), "--connect-timeout", "3", "--max-time", "5",
        "--silent", "--show-error", "--fail", "--write-out", "HTTP_VERSION=%{http_version}",
        f"https://{host}:{PORT}/",
    ], capture_output=True, timeout=10)


def terminate(proc):
    if proc is not None and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def main():
    if os.environ.get("SHADOWQUIC_TUN_TEST_CONTAINER") != "1" or os.geteuid() != 0:
        raise RuntimeError("Run scripts/test_suite/test_tun.py --http3-sniffer")
    tun.run("groupadd", "--system", "shadowquic-http3-test")
    gid = grp.getgrnam("shadowquic-http3-test").gr_gid
    tun.run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-keyout", str(KEY), "-out", str(CERT), "-subj", f"/CN={HOST}",
            "-addext", f"subjectAltName=DNS:{HOST},DNS:{BLOCKED}")
    tun.CONFIG.write_text(f"""inbounds:
  - tag: tun-in
    type: tun
    name: {tun.DEVICE}
    mtu: 1500
outbounds:
  - {{tag: direct, type: direct}}
  - {{tag: blocked, type: drop}}
router:
  sniffer:
    udp:
      - {{ports: [{PORT}], protocols: [quic]}}
    timeout: 500
  src: |
    return function(ctx)
      assert(ctx.inbound_tag == "tun-in")
      assert(ctx.network_type == "udp")
      assert(ctx.dst_ip_v4 == "{tun.TEST_V4}" and ctx.dst_port == {PORT})
      assert(ctx.dst_domain == nil)
      assert(ctx.sniffer ~= nil, "QUIC hostname was not sniffed")
      assert(ctx.sniffer.type == "QUIC" and ctx.sniffer.version == "1")
      if ctx.sniffer.domain == "{HOST}" then
        info("HTTP3_SNI_ALLOWED")
        return "direct"
      end
      assert(ctx.sniffer.domain == "{BLOCKED}")
      info("HTTP3_SNI_BLOCKED")
      return "blocked"
    end
log-level: debug
""")
    server = proxy = None
    try:
        with SERVER_LOG.open("w") as log:
            server = subprocess.Popen([sys.executable, __file__, "--server"], stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 10
        while not READY.exists():
            if server.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("HTTP/3 server did not start")
            time.sleep(0.05)
        with tun.LOG.open("w") as log:
            proxy = subprocess.Popen([tun.BINARY, "-c", str(tun.CONFIG)], stdout=log, stderr=subprocess.STDOUT, group=gid)
        tun.wait_for_device(proxy)
        tun.run("ip", "route", "add", tun.TEST_V4 + "/32", "dev", tun.DEVICE)
        # Only outbound proxy sockets reach the local server; curl must enter TUN.
        tun.run("iptables", "-t", "nat", "-A", "OUTPUT", "-m", "owner", "--gid-owner", str(gid),
                "-d", tun.TEST_V4, "-p", "udp", "--dport", str(PORT), "-j", "DNAT",
                "--to-destination", f"127.0.0.1:{PORT}")
        result = curl(HOST)
        assert result.returncode == 0, result.stderr.decode()
        assert result.stdout == BODY + b"HTTP_VERSION=3", result.stdout
        print("[PASS] curl --http3-only --interface sq-tun0: HTTP/3 response and sniffed SNI", flush=True)
        result = curl(BLOCKED)
        assert result.returncode != 0 and BODY not in result.stdout, result
        content = tun.LOG.read_text()
        assert "HTTP3_SNI_ALLOWED" in content and "HTTP3_SNI_BLOCKED" in content, content
        assert "sniff completed" in content and "elapsed_ms=" in content, content
        assert "routing request failed" not in content, content
        print("[PASS] Different sniffed SNI selects the blocked outbound", flush=True)
        tun.stop(proxy)
        proxy = None
    finally:
        terminate(proxy)
        terminate(server)
        for path in (tun.LOG, SERVER_LOG):
            if path.exists():
                print(f"--- {path.name} ---\n{path.read_text()}", flush=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["--server"]:
        asyncio.run(serve())
    else:
        main()
