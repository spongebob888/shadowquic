"""Generate small synthetic MMDB fixtures using only the Python standard library.

Run this file to reproduce Mihomo-Test.mmdb, Sing-GeoIP-Test.mmdb, and
Mihomo-Invalid-Test.mmdb. No real geolocation data is included.
"""

from ipaddress import ip_network
from pathlib import Path


def header(kind, size):
    prefix = bytes([kind << 5 if kind < 8 else 0])
    extended = bytes([kind - 7]) if kind >= 8 else b""
    if size < 29:
        return bytes([prefix[0] | size]) + extended
    assert size < 285
    return bytes([prefix[0] | 29]) + extended + bytes([size - 29])


def encode(value):
    if isinstance(value, str):
        data = value.encode()
        return header(2, len(data)) + data
    if isinstance(value, list):
        return header(11, len(value)) + b"".join(map(encode, value))
    if isinstance(value, dict):
        return header(7, len(value)) + b"".join(
            encode(key) + encode(item) for key, item in value.items()
        )
    # Explicit MMDB integer type and value for metadata; plain ints are uint32.
    kind, number = value if isinstance(value, tuple) else (6, value)
    data = number.to_bytes((number.bit_length() + 7) // 8, "big")
    return header(kind, len(data)) + data


def write_database(filename, database_type, ip_version, entries):
    nodes = [[None, None]]
    for prefix, value in entries:
        network = ip_network(prefix)
        bits = network.max_prefixlen
        depth = network.prefixlen
        if ip_version == 6 and network.version == 4:
            bits, depth = 128, depth + 96
        node = 0
        for level in range(depth):
            branch = (int(network.network_address) >> (bits - level - 1)) & 1
            if level == depth - 1:
                assert nodes[node][branch] is None
                nodes[node][branch] = encode(value)
            else:
                if nodes[node][branch] is None:
                    nodes[node][branch] = len(nodes)
                    nodes.append([None, None])
                node = nodes[node][branch]

    count = len(nodes)
    data = bytearray()
    offsets = {}
    tree = bytearray()
    for children in nodes:
        for child in children:
            if child is None:
                pointer = count
            elif isinstance(child, int):
                pointer = child
            else:
                if child not in offsets:
                    offsets[child] = count + 16 + len(data)
                    data.extend(child)
                pointer = offsets[child]
            tree.extend(pointer.to_bytes(3, "big"))

    metadata = {
        "binary_format_major_version": (5, 2),
        "binary_format_minor_version": (5, 0),
        "build_epoch": (9, 0),
        "database_type": database_type,
        "description": {"en": "Synthetic routing test"},
        "ip_version": (5, ip_version),
        "languages": [],
        "node_count": count,
        "record_size": (5, 24),
    }
    Path(__file__).with_name(filename).write_bytes(
        tree + bytes(16) + data + b"\xab\xcd\xefMaxMind.com" + encode(metadata)
    )


if __name__ == "__main__":
    write_database("Mihomo-Test.mmdb", "Meta-geoip0", 6, [
        ("8.8.8.0/24", ["us", "google"]),
        ("8.8.4.0/24", ["US", "GOOGLE", "us", "", "Google"]),
        ("223.5.5.0/24", "cn"),
        ("192.168.0.0/16", "private"),
        ("2001:4860::/32", ["US", "google"]),
        ("2001:db8::/32", "TEST"),
        ("9.9.9.0/24", []),
        ("9.9.10.0/24", ""),
    ])
    write_database("Sing-GeoIP-Test.mmdb", "sing-geoip", 4, [
        ("8.8.8.0/24", "us"),
        ("223.5.5.0/24", "CN"),
        ("192.168.0.0/16", "private"),
    ])
    write_database("Mihomo-Invalid-Test.mmdb", "Meta-geoip0", 4, [
        ("8.8.8.0/24", ["us", 42]),
        ("9.9.9.0/24", 42),
        ("223.5.5.0/24", "cn"),
    ])
