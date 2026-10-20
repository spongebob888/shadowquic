"""Offline checks for the standalone probe's evidence and DNS validation."""

import unittest

import dns.flags
import dns.message
import dns.rcode
import dns.rrset

from scripts.dns_0rtt_probe import count_early_packets, validate_reply


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


if __name__ == "__main__":
    unittest.main()
