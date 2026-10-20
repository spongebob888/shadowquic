#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["aioquic==1.7.0", "dnspython==2.9.0"]
# ///
"""Probe DoH over HTTP/3 and DoQ, including real 0-RTT on fresh connections.

Run: uv run scripts/dns_0rtt_probe.py --json /tmp/dns-0rtt.json
Certificates are verified. Tickets stay in memory, scoped to each endpoint/ALPN.
"""

import argparse
import asyncio
import base64
import json
import logging
import time
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path

import aioquic
import dns.exception
import dns.flags
import dns.message
import dns.rcode
import dns.rdatatype
from aioquic.asyncio.client import connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import (
    ConnectionTerminated,
    HandshakeCompleted,
    StreamDataReceived,
    StreamReset,
)
from aioquic.quic.logger import QuicLogger


@dataclass(frozen=True)
class Target:
    provider: str
    address: str
    hostname: str
    protocol: str

    @property
    def port(self):
        return 443 if self.protocol == "h3" else 853


class ProbeProtocol(QuicConnectionProtocol):
    def __init__(self, *args, target, **kwargs):
        super().__init__(*args, **kwargs)
        self.target = target
        self.http = H3Connection(self._quic) if target.protocol == "h3" else None
        self.handshake = None
        self.termination = None
        self.local_port = None
        self.reply = None
        self.stream_id = None
        self.body = bytearray()
        self.headers = {}

    def connection_made(self, transport):
        super().connection_made(transport)
        self.local_port = transport.get_extra_info("sockname")[1]

    def fail(self, message):
        if self.reply is not None and not self.reply.done():
            self.reply.set_exception(ConnectionError(message))

    def quic_event_received(self, event):
        if isinstance(event, HandshakeCompleted):
            self.handshake = event
        if isinstance(event, ConnectionTerminated):
            self.termination = event
            self.fail(f"QUIC close 0x{event.error_code:x}: {event.reason_phrase}")
        if isinstance(event, StreamReset) and event.stream_id == self.stream_id:
            self.fail(f"stream reset 0x{event.error_code:x}")

        if self.http is not None:
            for http_event in self.http.handle_event(event):
                if (
                    isinstance(http_event, (HeadersReceived, DataReceived))
                    and http_event.stream_id == self.stream_id
                ):
                    if isinstance(http_event, HeadersReceived):
                        self.headers.update(http_event.headers)
                    else:
                        self.body.extend(http_event.data)
                    if len(self.body) > 65535:
                        self.fail("HTTP DNS response exceeds 65535 bytes")
                    if (
                        http_event.stream_ended
                        and self.reply is not None
                        and not self.reply.done()
                    ):
                        self.reply.set_result(bytes(self.body))
        elif (
            isinstance(event, StreamDataReceived) and event.stream_id == self.stream_id
        ):
            self.body.extend(event.data)
            if len(self.body) > 65537:
                self.fail("DoQ response exceeds one DNS frame")
            if event.end_stream and self.reply is not None and not self.reply.done():
                self.reply.set_result(bytes(self.body))

    async def query(self, wire):
        self.stream_id = self._quic.get_next_available_stream_id()
        self.reply = asyncio.get_running_loop().create_future()
        if self.http is None:
            self._quic.send_stream_data(
                self.stream_id, len(wire).to_bytes(2) + wire, end_stream=True
            )
        else:
            encoded = base64.urlsafe_b64encode(wire).rstrip(b"=")
            self.http.send_headers(
                self.stream_id,
                [
                    (b":method", b"GET"),
                    (b":scheme", b"https"),
                    (b":authority", self.target.hostname.encode()),
                    (b":path", b"/dns-query?dns=" + encoded),
                    (b"accept", b"application/dns-message"),
                ],
                end_stream=True,
            )
        self.transmit()
        body = await self.reply
        if self.http is None:
            if len(body) < 2 or int.from_bytes(body[:2]) != len(body) - 2:
                raise ValueError("invalid DoQ length prefix or extra response")
            return body[2:]
        status = self.headers.get(b":status", b"?")
        if status != b"200":
            raise ValueError(f"HTTP status {status.decode()}")
        if (
            self.headers.get(b"content-type", b"").split(b";", 1)[0].strip().lower()
            != b"application/dns-message"
        ):
            raise ValueError("unexpected HTTP content-type")
        return body


def count_early_packets(qlog, stream_id):
    """Count sent 0-RTT packets containing the DNS request, excluding H3 setup."""
    return sum(
        event["name"] == "transport:packet_sent"
        and event["data"].get("header", {}).get("packet_type") == "0RTT"
        and any(
            frame.get("frame_type") == "stream" and frame.get("stream_id") == stream_id
            for frame in event["data"].get("frames", [])
        )
        for trace in qlog["traces"]
        for event in trace["events"]
    )


def validate_reply(query, wire):
    reply = dns.message.from_wire(wire)
    if not query.is_response(reply) or reply.flags & dns.flags.TC:
        raise ValueError("DNS response does not match query or is truncated")
    if reply.rcode() != dns.rcode.NOERROR:
        raise ValueError(f"DNS {dns.rcode.to_text(reply.rcode())}")
    addresses = [
        record.address
        for answer in reply.answer
        if answer.rdtype == dns.rdatatype.A
        for record in answer
    ]
    if not addresses:
        raise ValueError("DNS response has no A answers")
    return addresses


async def probe_connection(target, query, ticket, args, local_port=0):
    logger = QuicLogger()
    ticket_ready = asyncio.Event()
    tickets = []

    def save_ticket(new_ticket):
        tickets.append(new_ticket)
        ticket_ready.set()

    eligible = bool(
        ticket and ticket.is_valid and ticket.max_early_data_size == 0xFFFFFFFF
    )
    configuration = QuicConfiguration(
        is_client=True,
        alpn_protocols=[target.protocol],
        server_name=target.hostname,
        session_ticket=ticket,
        idle_timeout=args.timeout,
        max_data=65536,
        max_stream_data=8192,
        quic_logger=logger,
    )
    result = {
        "ticket_supplied": ticket is not None,
        "ticket_allows_0rtt": eligible,
        "handshake_completed": False,
        "session_resumed": None,
        "early_data_accepted": None,
        "dns_0rtt_packets": 0,
        "dns_ok": False,
        "answers": [],
        "http_status": None,
        "error": None,
    }
    protocol = None

    def make_protocol(*protocol_args, **protocol_kwargs):
        nonlocal protocol
        protocol = ProbeProtocol(*protocol_args, target=target, **protocol_kwargs)
        return protocol

    started = time.monotonic()
    try:
        # wait_connected=False lets the query and ClientHello leave together.
        # Without an eligible ticket, wait for the ordinary verified handshake.
        async with asyncio.timeout(args.timeout):
            async with connect(
                target.address,
                target.port,
                configuration=configuration,
                create_protocol=make_protocol,
                session_ticket_handler=save_ticket,
                wait_connected=not eligible,
                local_port=local_port,
            ) as protocol:
                wire = await protocol.query(query.to_wire())
                result["answers"] = validate_reply(query, wire)
                result["dns_ok"] = True
                result["query_ms"] = round((time.monotonic() - started) * 1000, 2)
                # Ticket delivery may follow the DNS response; wait explicitly.
                try:
                    await asyncio.wait_for(ticket_ready.wait(), args.ticket_wait)
                except TimeoutError:
                    pass
    except (OSError, ValueError, dns.exception.DNSException) as error:
        result["error"] = f"{type(error).__name__}: {error}"

    trace = logger.to_dict()
    if protocol is not None:
        result["client_port"] = protocol.local_port
        result["dns_0rtt_packets"] = count_early_packets(trace, protocol.stream_id)
        result["http_status"] = protocol.headers.get(b":status", b"").decode() or None
        if protocol.handshake is not None:
            result.update(
                handshake_completed=True,
                session_resumed=protocol.handshake.session_resumed,
                early_data_accepted=protocol.handshake.early_data_accepted,
                alpn=protocol.handshake.alpn_protocol,
            )
        if protocol.termination is not None and not result["dns_ok"]:
            event = protocol.termination
            closed = f"QUIC close 0x{event.error_code:x}: {event.reason_phrase}"
            result["error"] = (
                f"{result['error']}; {closed}" if result["error"] else closed
            )
    result["new_ticket_received"] = bool(tickets)
    result["new_ticket_allows_0rtt"] = bool(
        tickets and tickets[-1].max_early_data_size == 0xFFFFFFFF
    )
    result["elapsed_ms"] = round((time.monotonic() - started) * 1000, 2)
    return result, tickets[-1] if tickets else None, trace


async def probe_target(target, args):
    query = dns.message.make_query(args.domain, "A")
    query.id = 0  # Mandatory for DoQ; also suitable for cache-friendly DoH.
    results = []
    ticket = None
    local_port = 0
    for attempt in range(args.attempts + 1):
        result, new_ticket, trace = await probe_connection(
            target, query, ticket, args, local_port
        )
        result["connection"] = attempt
        if not result["handshake_completed"]:
            verdict = "unreachable / handshake failed"
        elif attempt == 0:
            verdict = "cold connection"
        elif result["dns_0rtt_packets"]:
            verdict = "accepted" if result["early_data_accepted"] else "rejected"
        else:
            verdict = (
                "not offered"
                if not result["ticket_allows_0rtt"]
                else "no early DNS packet"
            )
        result["zero_rtt_result"] = verdict
        results.append(result)
        if args.reuse_port:
            local_port = result.get("client_port", 0)
        if args.qlog_dir:
            args.qlog_dir.mkdir(parents=True, exist_ok=True)
            path = (
                args.qlog_dir
                / f"{target.provider}-{target.address}-{target.protocol}-{attempt}.qlog"
            )
            path.write_text(json.dumps(trace))
        if not result["handshake_completed"]:
            break
        ticket = new_ticket
        if ticket is None:
            break  # No resumption can be tested without a fresh ticket.
    return {**asdict(target), "port": target.port, "connections": results}


async def main(args):
    targets = []
    for provider, hostname, addresses in [
        ("google", "dns.google", ["8.8.8.8", "8.8.4.4"]),
        ("alidns", "dns.alidns.com", ["223.5.5.5", "223.6.6.6"]),
    ]:
        if args.provider not in ("all", provider):
            continue
        for address in addresses if args.all_addresses else addresses[:1]:
            for protocol in (
                ("h3", "doq") if args.protocol == "all" else [args.protocol]
            ):
                targets.append(Target(provider, address, hostname, protocol))
    # Independent endpoints run concurrently, with fresh tickets for each row.
    results = await asyncio.gather(*(probe_target(target, args) for target in targets))
    report = {
        "timestamp_utc": datetime.now(UTC).isoformat(),
        "aioquic_version": aioquic.__version__,
        "query": args.domain,
        "reuse_client_port": args.reuse_port,
        "results": results,
    }
    print(
        f"{'Provider/address':<25} {'ALPN':<5} {'Conn':>4} {'Resumed':<8} {'0-RTT DNS':>9} {'0-RTT result':<30} {'DNS':<5}"
    )
    for endpoint in results:
        for result in endpoint["connections"]:
            resumed = (
                str(result["session_resumed"]).lower()
                if result["handshake_completed"]
                else "unknown"
            )
            print(
                f"{endpoint['provider'] + '/' + endpoint['address']:<25} {endpoint['protocol']:<5} "
                f"{result['connection']:>4} {resumed:<8} {result['dns_0rtt_packets']:>9} "
                f"{result['zero_rtt_result']:<30} {'OK' if result['dns_ok'] else 'FAIL':<5}"
            )
            if result["error"]:
                print(f"  {result['error']}")
        if (
            endpoint["connections"][-1]["handshake_completed"]
            and not endpoint["connections"][-1]["new_ticket_received"]
        ):
            print(
                "  No new session ticket received; resumption could not be continued."
            )
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n")
        print(f"JSON report: {args.json}")
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--provider", choices=["all", "google", "alidns"], default="all"
    )
    parser.add_argument("--protocol", choices=["all", "h3", "doq"], default="all")
    parser.add_argument(
        "--all-addresses",
        action="store_true",
        help="also test each provider's secondary IPv4 address",
    )
    parser.add_argument(
        "--reuse-port",
        action="store_true",
        help="keep each endpoint's client UDP port across fresh connections",
    )
    parser.add_argument("--domain", default="example.com")
    parser.add_argument(
        "--attempts",
        type=int,
        default=3,
        help="fresh connections after the cold connection (default: 3)",
    )
    parser.add_argument("--timeout", type=float, default=5)
    parser.add_argument("--ticket-wait", type=float, default=1)
    parser.add_argument("--json", type=Path)
    parser.add_argument(
        "--qlog-dir", type=Path, help="save packet-level evidence as qlog files"
    )
    args = parser.parse_args()
    if args.attempts < 1 or args.timeout <= 0 or args.ticket_wait <= 0:
        parser.error("attempts, timeout and ticket-wait must be positive")
    logging.basicConfig(level=logging.ERROR)
    asyncio.run(main(args))
