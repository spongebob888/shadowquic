#!/bin/sh
# https://xtls.github.io/en/document/level-2/tproxy_ipv4_and_ipv6.html
set -eu
MARK=1
# Load kernel modules (if modular and available)
modprobe nft_tproxy 2>/dev/null || true
modprobe nft_socket 2>/dev/null || true

# IPv4 policy routing
ip -4 rule add fwmark "$MARK" lookup 100 2>/dev/null || true
ip -4 route replace local 0.0.0.0/0 dev lo table 100

# IPv6 policy routing
ip -6 rule add fwmark "$MARK" lookup 106 2>/dev/null || true
ip -6 route replace local ::/0 dev lo table 106

# Create dedicated nftables table
nft delete table inet shadowquic 2>/dev/null || true

nft -f - <<EOF
flush ruleset

table inet shadowquic {
        chain prerouting {
                type filter hook prerouting priority filter; policy accept;
                ip daddr { 127.0.0.0/8, 224.0.0.0/4, 255.255.255.255 } return
                meta l4proto tcp ip daddr 192.168.0.0/16 return
                ip daddr 192.168.0.0/16 udp dport != 53 return
                ip6 daddr { ::1, fe80::/10 } return
                meta l4proto tcp ip6 daddr fd00::/8 return
                ip6 daddr fd00::/8 udp dport != 53 return
                meta mark 0x000000ff return
                meta l4proto { tcp, udp } meta mark set 0x00000001 tproxy ip to 127.0.0.1:8900 accept
                meta l4proto { tcp, udp } meta mark set 0x00000001 tproxy ip6 to [::1]:8900 accept
        }

        chain output {
                type route hook output priority filter; policy accept;
                ip daddr { 127.0.0.0/8, 224.0.0.0/4, 255.255.255.255 } return
                meta l4proto tcp ip daddr 192.168.0.0/16 return
                ip daddr 192.168.0.0/16 udp dport != 53 return
                ip6 daddr { ::1, fe80::/10 } return
                meta l4proto tcp ip6 daddr fd00::/8 return
                ip6 daddr fd00::/8 udp dport != 53 return
                meta mark 0x000000ff return
                meta l4proto { tcp, udp } meta mark set 0x00000001 accept
        }
}

EOF
