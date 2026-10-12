# 设置策略路由 v4
ip rule add fwmark 1 table 100
ip route add local 0.0.0.0/0 dev lo table 100
#
# # 设置策略路由 v6
ip -6 rule add fwmark 1 table 106
ip -6 route add local ::/0 dev lo table 106



# 代理局域网设备 v4
iptables -t mangle -N SHADOWQUIC
iptables -t mangle -A SHADOWQUIC -d 127.0.0.1/32 -j RETURN
iptables -t mangle -A SHADOWQUIC -d 224.0.0.0/4 -j RETURN
iptables -t mangle -A SHADOWQUIC -d 255.255.255.255/32 -j RETURN
iptables -t mangle -A SHADOWQUIC -d 192.168.0.0/16 -p tcp -j RETURN
iptables -t mangle -A SHADOWQUIC -d 192.168.0.0/16 -p udp ! --dport 53 -j RETURN
iptables -t mangle -A SHADOWQUIC -j RETURN -m mark --mark 0xff
iptables -t mangle -A SHADOWQUIC -p udp -j TPROXY --on-ip 127.0.0.1 --on-port 8900 --tproxy-mark 1
iptables -t mangle -A SHADOWQUIC -p tcp -j TPROXY --on-ip 127.0.0.1 --on-port 8900 --tproxy-mark 1
iptables -t mangle -A PREROUTING -j SHADOWQUIC

# 代理局域网设备 v6
ip6tables -t mangle -N SHADOWQUIC6
ip6tables -t mangle -A SHADOWQUIC6 -d ::1/128 -j RETURN
ip6tables -t mangle -A SHADOWQUIC6 -d fe80::/10 -j RETURN
ip6tables -t mangle -A SHADOWQUIC6 -d fd00::/8 -p tcp -j RETURN
#ip6tables -t mangle -A SHADOWQUIC6 -d fd00::/8 -p udp -j RETURN
ip6tables -t mangle -A SHADOWQUIC6 -d fd00::/8 -p udp ! --dport 53 -j RETURN
ip6tables -t mangle -A SHADOWQUIC6 -j RETURN -m mark --mark 0xff
ip6tables -t mangle -A SHADOWQUIC6 -p udp -j TPROXY --on-ip ::1 --on-port 8900 --tproxy-mark 1
ip6tables -t mangle -A SHADOWQUIC6 -p tcp -j TPROXY --on-ip ::1 --on-port 8900 --tproxy-mark 1
ip6tables -t mangle -A PREROUTING -j SHADOWQUIC6

# 代理网关本机 v4
iptables -t mangle -N SHADOWQUIC_MASK
iptables -t mangle -A SHADOWQUIC_MASK -d 224.0.0.0/4 -j RETURN
iptables -t mangle -A SHADOWQUIC_MASK -d 255.255.255.255/32 -j RETURN
iptables -t mangle -A SHADOWQUIC_MASK -d 192.168.0.0/16 -p tcp -j RETURN
iptables -t mangle -A SHADOWQUIC_MASK -d 192.168.0.0/16 -p udp ! --dport 53 -j RETURN
iptables -t mangle -A SHADOWQUIC_MASK -j RETURN -m mark --mark 0xff
iptables -t mangle -A SHADOWQUIC_MASK -p udp -j MARK --set-mark 1
iptables -t mangle -A SHADOWQUIC_MASK -p tcp -j MARK --set-mark 1
iptables -t mangle -A OUTPUT -j SHADOWQUIC_MASK

# 代理网关本机 v6
ip6tables -t mangle -N SHADOWQUIC6_MASK
ip6tables -t mangle -A SHADOWQUIC6_MASK -d fe80::/10 -j RETURN
ip6tables -t mangle -A SHADOWQUIC6_MASK -d fd00::/8 -p tcp -j RETURN
ip6tables -t mangle -A SHADOWQUIC6_MASK -d fd00::/8 -p udp ! --dport 53 -j RETURN
ip6tables -t mangle -A SHADOWQUIC6_MASK -j RETURN -m mark --mark 0xff
ip6tables -t mangle -A SHADOWQUIC6_MASK -p udp -j MARK --set-mark 1
ip6tables -t mangle -A SHADOWQUIC6_MASK -p tcp -j MARK --set-mark 1
ip6tables -t mangle -A OUTPUT -j SHADOWQUIC6_MASK


