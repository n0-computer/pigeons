<h1 align="center">pigeons</h1>

<h3 align="center">
carrier pigeons for your SSH connections
</h3>

[![Documentation](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/iroh-pigeons/)
[![Crates.io](https://img.shields.io/crates/v/iroh-pigeons.svg?style=flat-square)](https://crates.io/crates/iroh-pigeons)
[![downloads](https://img.shields.io/crates/d/iroh-pigeons.svg?style=flat-square)](https://crates.io/crates/iroh-pigeons)
[![Chat](https://img.shields.io/discord/1161119546170687619?logo=discord&style=flat-square)](https://discord.com/invite/DpmJgtU7cW)
[![Youtube](https://img.shields.io/badge/YouTube-red?logo=youtube&logoColor=white&style=flat-square)](https://www.youtube.com/@n0computer)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE-APACHE)
[![CI](https://img.shields.io/github/actions/workflow/status/n0-computer/pigeons/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/n0-computer/pigeons/actions/workflows/ci.yml)

<div align="center">
  <h3>
    <a href="https://github.com/n0-computer/pigeons/releases">
      Releases
    </a>
    <span> | </span>
    <a href="https://docs.rs/iroh-pigeons">
      Rust Docs
    </a>
  </h3>
</div>
<br/>

## What is pigeons?

SSH to any machine without an IP address, behind a NAT or firewall, with no port
forwarding and no VPN setup.

```bash
# on the server
> pigeons roost
roost is running! id: bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330

# on the client — add a route, then ssh as normal
> pigeons add --id bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330 --name my-server
> ssh user@my-server
```

That's it. `ssh` and `sshd` need to be installed.

Under the hood pigeons dials by public key over [iroh], which establishes a
direct [QUIC] connection between the two machines, [hole-punching] whenever it
can and falling back to relay servers when it cannot.

`pigeons add` writes a `Host` entry to your SSH config whose [`ProxyCommand`]
runs `pigeons fly --stdio`, so `ssh` reaches the tunnel over that command's
stdin and stdout.

## Installation

Download a binary for your operating system from [Releases], or use the install
script:

```bash
curl -fSsL https://vorc.s3.us-east-2.amazonaws.com/pigeons-install.sh | bash
```

## Getting Started

### Server

Start a roost to accept incoming connections. Keys are persisted by default, so
the endpoint ID stays the same across restarts:

```bash
> pigeons roost
roost is running! id: bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330
```

Use `--ephemeral` for a throwaway identity, or `--ssh-port` when sshd listens
somewhere other than 22:

```bash
> pigeons roost --ephemeral --ssh-port 2222
```

### Client

Add a pigeon route, which writes an entry into your SSH config:

```bash
> pigeons add --id bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330 --name my-server
Pigeon route 'my-server' added to ~/.ssh/config
```

Then connect with standard ssh, from anywhere:

```bash
> ssh user@my-server
```

### Service Mode

Install pigeons as a system service for always-on access. Supported on Linux
(systemd), macOS (launchd), and Windows (SCM):

```bash
> pigeons service install                   # default SSH port 22
> pigeons service status                    # check if the service is running
> pigeons service log                       # view service logs
> pigeons service uninstall                 # remove the service
```

### Rust Library

The tunnel is also usable as a library. Add it with `cargo add iroh-pigeons`;
the API is documented on [docs.rs][Rust Docs]. Note that the crate is published
as `iroh-pigeons` while the CLI it installs is called `pigeons`.

## Telemetry

The first time you run a setup command (`roost`, `fly`, `add`, or `service
install`) in a terminal, pigeons asks once whether it may send anonymous
metrics to help us develop [iroh]. Both answers are recorded in the config file
that `pigeons paths` prints, so the question is asked once:

```toml
telemetry_enabled = true
```

Change that key whenever you like, or delete the file to be asked again.
Nothing is sent unless it is `true`.

What gets sent are iroh endpoint counters, sampled once a minute and pushed to
[iroh-services]: relay usage, hole-punching success, bytes moved. No hostnames,
usernames, SSH traffic, or anything about the machines you connect to. The
report travels over an iroh connection, so the sending endpoint ID is visible
to the collector.

The setting has two layers. Your own config wins, and any key it leaves unset
falls back to a machine-wide config at `/etc/pigeons/config.toml`, or
`C:\ProgramData\pigeons\config.toml` on Windows. Both paths are printed by
`pigeons paths`.

That machine-wide layer is what the service reads: a roost installed with
`pigeons service install` runs as root, so it would otherwise never see the
answer you gave as yourself. The install copies your answer there and prints
what the service will do with it. To change the service's setting later, edit
the machine-wide config and run `pigeons service restart`.

pigeons never asks when it is not attached to a terminal, when it runs as
`fly --stdio` (ssh's `ProxyCommand`), or when it runs elevated. Installing from
a root shell instead of through `sudo` leaves nothing to trace an answer back
to, so the service stays opted out until you set `telemetry_enabled` in the
machine-wide config yourself.

## Commands

```bash
# Server
> pigeons roost                             # start a roost (persistent keys by default)
> pigeons roost --ephemeral                 # throwaway identity
> pigeons roost --ssh-port 2222             # custom SSH port

# Client
> pigeons fly <ENDPOINT_ID>                 # quick connect (binds local port)
> pigeons fly --stdio <ENDPOINT_ID>         # ProxyCommand mode (used by ssh config)
> pigeons endpoint-id --key-dir <DIR>       # create/read a persistent client identity
> pigeons fly --key-dir <DIR> <ENDPOINT_ID> # reuse that client identity
> pigeons fly --relay-url <URL> --direct-address <IP:PORT> <ENDPOINT_ID>
> pigeons add --id <ID> --name <NAME>       # add SSH config entry
> pigeons list                              # list pigeon routes
> pigeons remove <NAME>                     # remove SSH config entry

# Service
> pigeons service install                   # install as system service
> pigeons service install --ssh-port 2222   # with custom SSH port
> pigeons service status                    # check service status
> pigeons service log                       # view service logs
> pigeons service restart                   # restart the running service
> pigeons service uninstall                 # remove service

# Misc
> pigeons version                           # print the version number
> pigeons paths                             # print config and data paths
```

## How It Works

```
┌──────────────┐        ┌─────────────────┐        ┌──────────────┐
│  SSH Client  │───────▶│   QUIC Tunnel   │───────▶│    pigeons   │
│              │        │  (P2P Network)  │        │     roost    │
└──────────────┘        └─────────────────┘        └──────────────┘
       │                         ▲                        │
       │                         │                        │
       ▼                         │                        ▼
┌──────────────┐        ┌─────────────────┐        ┌──────────────┐
│ ProxyCommand │        │   pigeons fly   │        │  SSH Server  │
│ pigeons fly  │───────▶│     --stdio     │        │ localhost:22 │
│    --stdio   │        │                 │        └──────────────┘
└──────────────┘        └─────────────────┘
```

1. **SSH client** invokes `pigeons fly --stdio` through SSH's `ProxyCommand`.
2. **Fly** establishes a QUIC connection over iroh's P2P network, traversing NAT
   automatically.
3. **Roost** accepts the connection and proxies it to the local SSH daemon.
4. **Authentication** stays standard SSH, end-to-end over the encrypted tunnel.

## Security Model

- **Endpoint ID access**: anyone holding the endpoint ID can reach your SSH port
- **SSH authentication**: standard SSH auth (keys, certificates, passwords) still applies
- **Persistent keys**: uses a dedicated `.ssh/pigeons_ed25519` keypair
- **QUIC encryption**: transport-layer encryption between endpoints

## License

Copyright 2025 fun with rust y2

Copyright 2026 N0, INC.

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   https://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

[QUIC]: https://en.wikipedia.org/wiki/QUIC
[hole-punching]: https://en.wikipedia.org/wiki/Hole_punching_(networking)
[iroh]: https://github.com/n0-computer/iroh
[iroh-services]: https://services.iroh.computer
[`ProxyCommand`]: https://man.openbsd.org/ssh_config#ProxyCommand
[Releases]: https://github.com/n0-computer/pigeons/releases
[Rust Docs]: https://docs.rs/iroh-pigeons
