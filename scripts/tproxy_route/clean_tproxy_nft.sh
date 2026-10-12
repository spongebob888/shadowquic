MARK=1

# Remove nftables rules
nft delete table inet shadowquic 2>/dev/null || true

# Remove IPv4 policy routing
ip -4 rule del fwmark $MARK table 100 2>/dev/null || true
ip -4 route flush table 100

# Remove IPv6 policy routing
ip -6 rule del fwmark $MARK table 106 2>/dev/null || true
ip -6 route flush table 106

echo "clean tproxy"