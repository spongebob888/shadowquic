#!/bin/bash
set -ex

# Go to workspace root
cd "$(dirname "$0")/../.."

mkdir -p target/debug

cat << 'SCRIPT_EOF' > target/debug/run_tproxy_echo_test_inner.sh
#!/bin/bash
set -ex

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y iproute2 iptables curl netcat-openbsd python3 socat

cd /app

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

# Local traffic (redirect 10.255.255.1 traffic to TPROXY)
iptables -t mangle -A OUTPUT -p tcp -d 10.255.255.1 --dport 8080 -j MARK --set-mark 1
iptables -t mangle -A OUTPUT -p udp -d 10.255.255.1 --dport 8080 -j MARK --set-mark 1

# Run the rust test
cargo test --features tproxy --test tproxy_echo -- --nocapture

SCRIPT_EOF

chmod +x target/debug/run_tproxy_echo_test_inner.sh

# Run docker container
docker run --rm --cap-add=NET_ADMIN \
    -v $(pwd):/app \
    rust:latest /app/target/debug/run_tproxy_echo_test_inner.sh

echo "Docker test completed successfully!"
