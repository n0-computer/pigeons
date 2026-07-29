# pigeons

[![Crates.io](https://img.shields.io/crates/v/pigeons.svg)](https://crates.io/crates/pigeons)
[![Documentation](https://docs.rs/pigeons/badge.svg)](https://docs.rs/pigeons)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

**SSH to any machine without an IP address, behind a NAT/firewall without port forwarding or VPN setup.**

```bash
# on the server
> pigeons roost
roost is running! id: bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330

# on the client — add a route, then ssh as normal
> pigeons add --id bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330 --name my-server
> ssh user@my-server
```

**That's it.** (requires ssh/sshd to be installed)

---

## Installation

Download the binary for your operating system from [GitHub Releases](https://github.com/n0-computer/pigeons/releases), or use our bash one-liner:

```bash
curl -fSsL https://vorc.s3.us-east-2.amazonaws.com/pigeons-install.sh | bash
```

---

## Quick Start

### Server (roost)

Start a roost to accept incoming connections. Keys are persisted by default so the endpoint ID stays the same across restarts:

```bash
> pigeons roost
roost is running! id: bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330
```

Use `--ephemeral` for a throwaway identity, or `--ssh-port` if sshd is on a non-standard port:

```bash
> pigeons roost --ephemeral --ssh-port 2222
```

### Client 

The easiest way to connect is to add a pigeon route, which creates an SSH config entry:

```bash
> pigeons add --id bb8e1a5661a6dfa9ae2dd978922f30f524f6fd8c99b3de021c53f292aae74330 --name my-server
Pigeon route 'my-server' added to ~/.ssh/config
```

Then connect with standard ssh:

```bash
> ssh user@my-server
```

Works through any firewall, NAT, or private network. No configuration needed.

---

## Service Mode

Install pigeons as a system service for always-on access:

```bash
> pigeons service install                   # default SSH port 22
> pigeons service status                    # check if the service is running
> pigeons service log                       # view service logs
> pigeons service uninstall                 # remove the service
```

Supported on Linux (systemd), macOS (launchd), and Windows (SCM).

---

## Route Management

```bash
# Add a route
> pigeons add --id <ENDPOINT_ID> --name my-server

# List configured routes
> pigeons list

# Remove a route
> pigeons remove my-server
```

---

## How It Works

```
┌─────────────┐          ┌─────────────────┐          ┌─────────────┐
│     SSH     │─────────▶│  QUIC Tunnel    │─────────▶│   pigeons   │
│   Client    │          │  (P2P Network)  │          │    roost    │
└─────────────┘          └─────────────────┘          └─────────────┘
      │                           ▲                            │
      │                           │                            │
      ▼                           │                            ▼
┌─────────────┐          ┌─────────────────┐          ┌──────────────────┐
│ ProxyCommand│          │   pigeons fly   │          │   SSH Server     │
│ pigeons fly │─────────▶│    --stdio      │          │  localhost:22    │
│   --stdio   │          │                 │          └──────────────────┘
└─────────────┘          └─────────────────┘
```

1. **SSH Client**: Invokes `pigeons fly --stdio` via SSH's ProxyCommand
2. **Fly**: Establishes QUIC connection through Iroh's P2P network (automatic NAT traversal)
3. **Roost**: Accepts connection and proxies to local SSH daemon (port 22)
4. **Authentication**: Standard SSH authentication end-to-end over encrypted QUIC tunnel

## Commands

```bash
# Server
> pigeons roost                             # start a roost (persistent keys by default)
> pigeons roost --ephemeral                 # throwaway identity
> pigeons roost --ssh-port 2222             # custom SSH port

# Client
> pigeons fly <ENDPOINT_ID>                 # quick connect (binds local port)
> pigeons fly --stdio <ENDPOINT_ID>         # ProxyCommand mode (used by ssh config)
> pigeons add --id <ID> --name <NAME>       # add SSH config entry
> pigeons list                              # list pigeon routes
> pigeons remove <NAME>                     # remove SSH config entry

# Service
> pigeons service install                   # install as system service
> pigeons service install --ssh-port 2222   # with custom SSH port
> pigeons service status                    # check service status
> pigeons service log                       # view service logs
> pigeons service uninstall                 # remove service
```

## Security Model

- **Endpoint ID access**: Anyone with the Endpoint ID can reach your SSH port
- **SSH authentication**: Standard SSH auth (keys, certificates, passwords) applies
- **Persistent keys**: Uses dedicated `.ssh/pigeons_ed25519` keypair
- **QUIC encryption**: Transport layer encryption between endpoints

## License

Licensed under either of Apache License 2.0 or MIT license at your option.
