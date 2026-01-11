#!/bin/bash

# Color definitions
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
PLAIN='\033[0m'

APP_NAME="shadowquic"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/shadowquic"
SERVICE_FILE="/etc/systemd/system/shadowquic.service"
GITHUB_REPO="spongebob888/shadowquic"

# Helper functions for logging
log_info() {
    echo -e "${YELLOW}$1${PLAIN}"
}

log_success() {
    echo -e "${GREEN}$1${PLAIN}"
}

log_error() {
    echo -e "${RED}$1${PLAIN}"
}

# Check if running as root
check_root() {
    if [ "$EUID" -ne 0 ]; then
        log_error "Please run as root"
        exit 1
    fi
}

# Check for necessary dependencies
check_dependencies() {
    log_info "Checking dependencies..."
    
    # Check for curl
    if ! command -v curl &> /dev/null; then
        log_error "Error: curl is not installed."
        log_info "Please install curl (e.g., apt install curl / yum install curl) and try again."
        exit 1
    fi

    # Check for systemd
    if ! command -v systemctl &> /dev/null; then
        log_error "Error: systemd (systemctl) is not found."
        log_info "This script requires a systemd-based system."
        exit 1
    fi
}

# Detect architecture
detect_arch() {
    local arch
    arch=$(uname -m)
    case $arch in
        x86_64)
            RELEASE_ARCH="x86_64-linux"
            ;;
        aarch64)
            RELEASE_ARCH="aarch64-linux"
            ;;
        *)
            log_error "Unsupported architecture: $arch"
            exit 1
            ;;
    esac
    log_success "Detected architecture: $arch (Release: $RELEASE_ARCH)"
}

# Download and install binary
install_binary() {
    log_info "Downloading latest release..."
    local download_url="https://github.com/$GITHUB_REPO/releases/latest/download/$APP_NAME-$RELEASE_ARCH"
    log_info "Downloading from: $download_url"

    local temp_dir
    temp_dir=$(mktemp -d)
    
    if curl -L -o "$temp_dir/$APP_NAME" "$download_url"; then
        log_success "Download successful."
    else
        log_error "Download failed."
        rm -rf "$temp_dir"
        exit 1
    fi

    chmod +x "$temp_dir/$APP_NAME"
    
    log_info "Installing $APP_NAME to $INSTALL_DIR..."
    mv "$temp_dir/$APP_NAME" "$INSTALL_DIR/"
    rm -rf "$temp_dir"
}

# Configure shadowquic
configure_system() {
    if [ ! -d "$CONFIG_DIR" ]; then
        log_info "Creating config directory: $CONFIG_DIR"
        mkdir -p "$CONFIG_DIR"
    fi

    if [ -f "$CONFIG_DIR/server.yaml" ]; then
        log_success "Config file already exists at $CONFIG_DIR/server.yaml"
        return
    fi

    log_info "Generating random credentials..."
    local username=$(tr -dc 'a-zA-Z0-9' < /dev/urandom | fold -w 8 | head -n 1)
    local password=$(tr -dc 'a-zA-Z0-9' < /dev/urandom | fold -w 16 | head -n 1)
    local bind_addr="0.0.0.0:1443"
    local jls_domain="cloudflare.com:443"

    # Export for final display
    GEN_USERNAME="$username"
    GEN_PASSWORD="$password"
    GEN_ADDR="$bind_addr"
    GEN_DOMAIN="$jls_domain"

    log_info "Creating server config..."
    cat > "$CONFIG_DIR/server.yaml" <<EOF
inbound:
    type: shadowquic
    bind-addr: "$bind_addr"
    users:
        - username: "$username"
          password: "$password"
    jls-upstream:
        addr: "$jls_domain" # domain + port, domain must be the same as client
    alpn: ["h3"]
    congestion-control: bbr
    zero-rtt: true
outbound:
    type: direct
    dns-strategy: prefer-ipv4 # or prefer-ipv6, ipv4-only, ipv6-only
log-level: "info"
EOF
    log_success "Server config created at $CONFIG_DIR/server.yaml"
}

# Setup systemd service
setup_systemd() {
    log_info "Creating systemd service..."
    cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=ShadowQuic Service
Documentation=https://github.com/spongebob888/shadowquic
After=network.target

[Service]
Type=simple
User=root
LimitNOFILE=1048576
ExecStart=$INSTALL_DIR/$APP_NAME -c $CONFIG_DIR/server.yaml
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF

    log_info "Reloading systemd..."
    systemctl daemon-reload
    systemctl enable shadowquic
}

# Stop existing service
stop_service() {
    if systemctl is-active --quiet shadowquic; then
        log_info "Stopping existing shadowquic service..."
        systemctl stop shadowquic
    fi
}

# Main function
main() {
    check_root
    check_dependencies
    detect_arch
    stop_service
    install_binary
    configure_system
    setup_systemd

    log_success "Installation complete!"
    log_success "You can start the service with: systemctl start shadowquic"
    log_success "Check status with: systemctl status shadowquic"

    if [ -n "$GEN_USERNAME" ]; then
        echo ""
        log_success "=============================="
        log_success " ShadowQuic Installed Successfully "
        log_success "=============================="
        log_info "Address:    $GEN_ADDR"
        log_info "FakeDomain: $GEN_DOMAIN"
        log_info "Username:   $GEN_USERNAME"
        log_info "Password:   $GEN_PASSWORD"
        log_success "=============================="
    fi
}

# Execute main function
main
