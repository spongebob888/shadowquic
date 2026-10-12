#!/bin/bash
set -e

# 删除 iptables 跳转规则（包括重复添加的规则）
delete_jump() {
    local cmd="$1"
    local parent="$2"
    shift 2

    while "$cmd" -t mangle -C "$parent" "$@" 2>/dev/null; do
        "$cmd" -t mangle -D "$parent" "$@"
    done
}

# IPv4
delete_jump iptables PREROUTING -j SHADOWQUIC
delete_jump iptables OUTPUT -j SHADOWQUIC_MASK

# IPv6
delete_jump ip6tables PREROUTING -j SHADOWQUIC6
delete_jump ip6tables OUTPUT -j SHADOWQUIC6_MASK


# 删除自定义链
delete_chain() {
    local cmd="$1"
    local chain="$2"

    if "$cmd" -t mangle -L "$chain" -n >/dev/null 2>&1; then
        "$cmd" -t mangle -F "$chain"
        "$cmd" -t mangle -X "$chain"
    fi
}

delete_chain iptables SHADOWQUIC
delete_chain iptables SHADOWQUIC_MASK

delete_chain ip6tables SHADOWQUIC6
delete_chain ip6tables SHADOWQUIC6_MASK


# 删除 IPv4 策略路由
while ip -4 rule show | grep -Eq 'fwmark 0x1(/0xffffffff)? lookup 100([[:space:]]|$)'; do
    ip -4 rule del fwmark 1 table 100
done

ip -4 route del local 0.0.0.0/0 dev lo table 100 2>/dev/null || true

# 删除 IPv6 策略路由
while ip -6 rule show | grep -Eq 'fwmark 0x1(/0xffffffff)? lookup 106([[:space:]]|$)'; do
    ip -6 rule del fwmark 1 table 106
done

ip -6 route del local ::/0 dev lo table 106 2>/dev/null || true

echo "TPROXY cleanup completed."
