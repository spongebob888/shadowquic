# API Subcommand

The `api` subcommand calls SQuic user-management APIs through the `outbound`
section of the config file. The outbound must be `shadowquic` or `sunnyquic`.

The configured client username must start with `admin`, such as `admin`,
`admin_bob`, or `admin123`. Other users receive `PermissionDenied`.

## Usage

Use the default `config.yaml`:

```sh
shadowquic api list-users
shadowquic api add-user alice alice-pass
shadowquic api remove-user alice
shadowquic api get-stats alice
shadowquic api kill-conn alice
```

Use another config file:

```sh
shadowquic -c shadowquic/config_examples/client.yaml api list-users
shadowquic api -c shadowquic/config_examples/client.yaml add-user alice alice-pass
shadowquic api remove-user alice -c shadowquic/config_examples/client.yaml
```

When running from source:

```sh
cargo run -p shadowquic -- api list-users
cargo run -p shadowquic -- api add-user alice alice-pass
cargo run -p shadowquic -- api remove-user alice
cargo run -p shadowquic -- api get-stats alice
cargo run -p shadowquic -- api kill-conn alice
```

`add-user` updates the password if the username already exists.

