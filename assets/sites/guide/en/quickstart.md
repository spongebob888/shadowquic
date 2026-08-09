# Quick Start (5 minutes)

Goal: **one server + one local machine**, routing local traffic through
the tunnel.

## Step 1: Download the binary

Grab the `shadowquic` executable for your platform from
[GitHub Releases](https://github.com/spongebob888/shadowquic/releases).
It's a single file — unzip and run.

On Linux you can also use the official one-shot install script (installs to
the system and creates a systemd service):

```bash
curl -L https://raw.githubusercontent.com/spongebob888/shadowquic/main/scripts/linux_install.sh | bash
```

> Want to build from source? See the Build section of the repository README.

## Step 2: Configure the server

On the **remote server**, create `server.yaml`:

```yaml
inbound:
  type: shadowquic
  bind-addr: "0.0.0.0:1443"        # listen on port 1443 on all interfaces
  users:
    - username: "admin"            # account the client will use
      password: "hello"
  jls-upstream:
    addr: "cloudflare.com:443"     # disguise as visiting cloudflare.com (changeable)
  alpn: ["h3"]
outbound:
  type: direct                     # server reaches sites directly
log-level: "info"
```

Start it:

```bash
./shadowquic -c server.yaml
```

> Note: open the `bind-addr` port (1443) for **UDP** in your firewall /
> cloud security group.

## Step 3: Configure the client

On your **local machine**, create `client.yaml`:

```yaml
inbound:
  type: socks
  bind-addr: "127.0.0.1:1089"      # local SOCKS5 proxy port
outbound:
  type: shadowquic
  addr: "YOUR_SERVER_IP:1443"      # change to your server's address
  username: "admin"                # must match server.yaml
  password: "hello"
  server-name: "cloudflare.com"    # must match server's jls-upstream domain
  alpn: ["h3"]
log-level: "info"
```

Start it:

```bash
./shadowquic -c client.yaml
```

## Step 4: Verify it works

From a terminal (macOS/Linux ships with `curl`):

```bash
curl --socks5-hostname 127.0.0.1:1089 https://example.com
```

You should see the page content. To confirm traffic really goes through the
tunnel, check your egress IP:

```bash
curl --socks5-hostname 127.0.0.1:1089 https://ipinfo.io/ip
```

If it shows **your server's IP** (not your local one), the tunnel works.

## Step 5: Point your browser / OS at it

- **Browser**: install an extension like SwitchyOmega, set SOCKS5
  `127.0.0.1:1089`.
- **System**: set the system proxy to `127.0.0.1:1089` (SOCKS5).
- **Phone / Clash**: ShadowQUIC is supported by ready-made clients — see the
  [Clash-rs](https://github.com/Watfaq/clash-rs) and other clients in the README.

## Stuck?

See the [FAQ](faq.md). The usual culprits are: UDP port not opened in the
firewall, or `server-name` not matching the server's `jls-upstream` domain.
