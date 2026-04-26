#!/bin/bash
set -ex

# Go to workspace root
cd "$(dirname "$0")/.."

mkdir -p target/debug

cat << 'EOF' > target/debug/tproxy_config.yaml
inbound:
  type: tproxy
  bind-addr: "0.0.0.0:1089"
outbound:
  type: direct
log-level: trace
EOF

cat << 'EOF' > target/debug/run_test.sh
#!/bin/bash
set -ex

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y iproute2 iptables curl netcat-openbsd python3 socat

cd /app

cargo build --features tproxy

# Create a group for shadowquic
groupadd shadowquic

# Set up iptables for tproxy
ip rule add fwmark 1 table 100
ip route add local 0.0.0.0/0 dev lo table 100

iptables -t mangle -N DIVERT
iptables -t mangle -A DIVERT -j MARK --set-mark 1
iptables -t mangle -A DIVERT -j ACCEPT

iptables -t mangle -A PREROUTING -p tcp -m socket -j DIVERT
iptables -t mangle -A PREROUTING -p udp -m socket -j DIVERT

iptables -t mangle -A PREROUTING -p tcp -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089
iptables -t mangle -A PREROUTING -p udp -j TPROXY --tproxy-mark 0x1/0x1 --on-port 1089

# Local traffic (excluding shadowquic group)
iptables -t mangle -A OUTPUT -m owner --gid-owner shadowquic -j RETURN
# Redirect 10.255.255.1 traffic for testing
iptables -t mangle -A OUTPUT -p tcp -d 10.255.255.1 --dport 8080 -j MARK --set-mark 1
iptables -t mangle -A OUTPUT -p udp -d 10.255.255.1 --dport 8080 -j MARK --set-mark 1

# Redirect shadowquic's outbound connection to the local server
iptables -t nat -A OUTPUT -m owner --gid-owner shadowquic -d 10.255.255.1 -p tcp --dport 8080 -j DNAT --to-destination 127.0.0.1:8080
iptables -t nat -A OUTPUT -m owner --gid-owner shadowquic -d 10.255.255.1 -p udp --dport 8080 -j DNAT --to-destination 127.0.0.1:8080

# Start HTTP server
python3 -m http.server 8080 --bind 127.0.0.1 &

# Start UDP echo server
socat UDP-LISTEN:8080,fork,bind=127.0.0.1 EXEC:cat &

# Run shadowquic as shadowquic group
sg shadowquic -c "target/debug/shadowquic -c target/debug/tproxy_config.yaml &"
# Wait a bit for startup
sleep 3

# Test TCP
TCP_RES=$(curl -s --connect-timeout 5 10.255.255.1:8080 || true)
if echo "$TCP_RES" | grep -q "Directory listing"; then
    echo "TCP Tproxy Success"
else
    echo "TCP Tproxy Failed"
    exit 1
fi

# Test UDP
UDP_RES=$(echo "hello_tproxy" | nc -u -w 2 10.255.255.1 8080 || true)
if echo "$UDP_RES" | grep -q "hello_tproxy"; then
    echo "UDP Tproxy Success"
else
    echo "UDP Tproxy Failed"
    exit 1
fi

EOF

chmod +x target/debug/run_test.sh

# Run docker container
docker run --rm --cap-add=NET_ADMIN \
    -v $(pwd):/app \
    rust:latest /app/target/debug/run_test.sh

echo "Docker test completed successfully!"
