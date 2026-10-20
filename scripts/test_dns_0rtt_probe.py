"""Offline checks for the standalone probe's evidence and DNS validation."""

import asyncio
import unittest
from unittest.mock import Mock

import dns.flags
import dns.message
import dns.rcode
import dns.rrset
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.connection import QuicConnection
from aioquic.quic.events import ConnectionTerminated, StreamDataReceived, StreamReset

from scripts.dns_0rtt_probe import (
    ProbeProtocol,
    Target,
    count_early_packets,
    validate_reply,
)


class ProbeValidationTests(unittest.TestCase):
    def test_counts_only_early_packets_carrying_the_dns_stream(self):
        def packet(kind, stream):
            return {
                "name": "transport:packet_sent",
                "data": {
                    "header": {"packet_type": kind},
                    "frames": [{"frame_type": "stream", "stream_id": stream}],
                },
            }

        qlog = {
            "traces": [
                {
                    "events": [
                        packet("0RTT", 2),  # HTTP/3 setup is not a DNS request.
                        packet("0RTT", 0),
                        packet("1RTT", 0),  # Normal data is not evidence of 0-RTT.
                    ]
                }
            ]
        }
        self.assertEqual(count_early_packets(qlog, 0), 1)
        self.assertEqual(count_early_packets(qlog, None), 0)

    def test_validates_dns_answer_instead_of_just_a_successful_handshake(self):
        query = dns.message.make_query("example.com", "A")
        reply = dns.message.make_response(query)
        reply.answer.append(
            dns.rrset.from_text("example.com.", 60, "IN", "A", "192.0.2.1")
        )
        self.assertEqual(validate_reply(query, reply.to_wire()), ["192.0.2.1"])

        for invalid in ("id", "question", "truncated", "nxdomain", "empty"):
            with self.subTest(invalid=invalid):
                bad = dns.message.from_wire(reply.to_wire())
                if invalid == "id":
                    bad.id ^= 1
                elif invalid == "question":
                    bad.question = dns.message.make_query("other.example", "A").question
                elif invalid == "truncated":
                    bad.flags |= dns.flags.TC
                elif invalid == "nxdomain":
                    bad.set_rcode(dns.rcode.NXDOMAIN)
                else:
                    bad.answer.clear()
                with self.assertRaises(ValueError):
                    validate_reply(query, bad.to_wire())


class MultiplexingTests(unittest.IsolatedAsyncioTestCase):
    def protocol(self, alpn):
        connection = QuicConnection(
            configuration=QuicConfiguration(is_client=True, alpn_protocols=[alpn])
        )
        protocol = ProbeProtocol(
            connection, target=Target("test", "127.0.0.1", "dns.test", alpn)
        )
        protocol.transmit = Mock()
        return protocol

    async def test_doq_interleaved_fragments_stay_on_their_own_streams(self):
        protocol = self.protocol("doq")
        first = asyncio.create_task(protocol.query(b"query1"))
        second = asyncio.create_task(protocol.query(b"query2"))
        await asyncio.sleep(0)
        self.assertEqual(sorted(protocol.requests), [0, 4])
        protocol.quic_event_received(
            StreamDataReceived(data=b"\x00", end_stream=False, stream_id=0)
        )
        protocol.quic_event_received(
            StreamDataReceived(data=b"\x00\x06second", end_stream=True, stream_id=4)
        )
        self.assertEqual(await second, (b"second", 4, None))
        self.assertFalse(first.done())
        protocol.quic_event_received(
            StreamDataReceived(data=b"\x05first", end_stream=True, stream_id=0)
        )
        self.assertEqual(await first, (b"first", 0, None))
        self.assertEqual(
            protocol.stream_id,
            0,
            "0-RTT evidence must stay attached to the first query",
        )
        self.assertEqual(protocol.requests, {})

    async def test_http3_stream_reset_does_not_break_another_request(self):
        protocol = self.protocol("h3")
        first = asyncio.create_task(protocol.query(b"query1"))
        second = asyncio.create_task(protocol.query(b"query2"))
        await asyncio.sleep(0)
        protocol.quic_event_received(StreamReset(stream_id=0, error_code=0x10C))
        protocol.http_event_received(
            HeadersReceived(
                headers=[
                    (b":status", b"200"),
                    (b"content-type", b"application/dns-message"),
                ],
                stream_id=4,
                stream_ended=False,
            )
        )
        protocol.http_event_received(
            DataReceived(data=b"second", stream_id=4, stream_ended=True)
        )
        with self.assertRaises(ConnectionError):
            await first
        self.assertEqual(await second, (b"second", 4, "200"))
        self.assertEqual(protocol.requests, {})

    async def test_handshake_wait_reports_connection_termination(self):
        protocol = self.protocol("doq")
        protocol.quic_event_received(
            ConnectionTerminated(error_code=0, frame_type=None, reason_phrase="closed")
        )
        with self.assertRaises(ConnectionError):
            await protocol.wait_handshake()


if __name__ == "__main__":
    unittest.main()
