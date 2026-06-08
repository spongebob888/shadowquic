# API Subcommand

The `api` subcommand calls the SQuic user-management control-plane APIs through
the `outbound` section of a config file. The outbound must be `shadowquic` or
`sunnyquic`; `socks` and `direct` outbounds do not implement these APIs.

The configured outbound username must start with `admin`, such as `admin`,
`admin_bob`, or `admin123`. Other users can still proxy traffic, but API calls
return `PermissionDenied`.

By default, the CLI reads `config.yaml` from the current directory. Use `-c` or
`--config` to point at another client config. The flag is global, so it can be
placed before or after `api`.

```sh
shadowquic api list-users
shadowquic -c shadowquic/config_examples/client.yaml api list-users
shadowquic api --config shadowquic/config_examples/client.yaml list-users
```

When running from source, pass the subcommand after `--`:

```sh
cargo run -p shadowquic -- api list-users
```

## Subcommands

### `list-users`

List all usernames configured on the remote server.

```sh
shadowquic api list-users
```

The command prints one username per line:

```text
admin
alice
bob
```

### `add-user <username> <password>`

Add a new user to the remote server.

```sh
shadowquic api add-user alice alice-pass
```

If the username already exists, the server updates that user's password. On
success, the command prints:

```text
user added: alice
```

### `remove-user <username>`

Remove a user from the remote server.

```sh
shadowquic api remove-user alice
```

Removing a user also closes that user's active connections. On success, the
command prints:

```text
user removed: alice
```

If the user does not exist, the command fails with `NotFound`.

### `get-stats [username]`

Fetch traffic and connection statistics for one user, or every configured user
when `username` is omitted.

```sh
shadowquic api get-stats alice
```

Output fields:

```text
username: alice
conn_num: 1
tcp_conns: 1
tcp_sent: 4096
tcp_recv: 4096
udp_conns: 1
udp_sent: 777
udp_recv: 777
```

`conn_num` is the number of online QUIC connections for the user.
`tcp_conns` and `udp_conns` are active proxied TCP streams and UDP associations.
`tcp_sent` and `udp_sent` count bytes sent from the server back to the client.
`tcp_recv` and `udp_recv` count bytes received by the server from the client.

Traffic statistics require the `statistics` feature, which is enabled by
default on targets with 64-bit atomics. On targets without 64-bit atomics, such
as 32-bit MIPS, statistics are disabled and this command returns zero counters.

To fetch stats for every configured user, omit the username:

```sh
shadowquic api get-stats
```

The command prints one `get-stats`-style block per user, separated by a blank
line:

```text
username: admin
conn_num: 1
tcp_conns: 0
tcp_sent: 0
tcp_recv: 0
udp_conns: 0
udp_sent: 0
udp_recv: 0

username: alice
conn_num: 1
tcp_conns: 1
tcp_sent: 4096
tcp_recv: 4096
udp_conns: 1
udp_sent: 777
udp_recv: 777
```

### `kill-conn <username>`

Close all online QUIC connections for a user.

```sh
shadowquic api kill-conn alice
```

On success, the command prints:

```text
user connections killed: alice
```

The user remains configured and can reconnect with the same password. Use
`remove-user` if you want to delete the user as well.

## Common Errors

`PermissionDenied` means the outbound username in the client config does not
start with `admin`.

`NotFound` means the target username does not exist on the server.

`NotAvailable` means the connected server or protocol implementation does not
support the requested API.

`api requires a shadowquic or sunnyquic outbound config` means the selected
config file has a `socks` or `direct` outbound. Use a client config whose
outbound connects to a ShadowQuic or SunnyQuic server.
