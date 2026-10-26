# pigeons mobile

An iOS app for working from a phone the way `~/work.sh` works from a laptop:
dial one pigeons roost, see its tmux sessions grouped by project, jump in and
out of them, and open worktrees through `work.sh` itself.

```
┌─────────────────────┐   C ABI (include/pigeons_mobile.h)   ┌──────────────────────────┐
│ ios/  SwiftUI app   │ ───────────────────────────────────▶ │ core/  Rust               │
│  SwiftTerm terminal │ ◀─── callbacks: state, results,  ─── │  iroh endpoint (device    │
│  key bar, lists     │      terminal bytes, log             │  key = SSH key)          │
└─────────────────────┘                                      │  russh over              │
                                                             │  Tunnel::open_stream     │
                                                             └────────────┬─────────────┘
                                                                          │ iroh / QUIC
                                                             ┌────────────▼─────────────┐
                                                             │ roost: pigeons roost      │
                                                             │  sshd → tmux, work.sh     │
                                                             └──────────────────────────┘
```

## Layout

- `core/` — Rust crate `pigeons-mobile`. Its own workspace root, so russh and
  friends stay out of the main crate's lockfile and CI. Depends on
  `iroh-pigeons` by path for the tunnel.
  - `src/client.rs` — one iroh endpoint, one SSH connection, exec channels for
    commands and PTY channels for terminals.
  - `src/tmux.rs` — the `work.sh` vocabulary: `tmux list-sessions` parsing,
    `project/tree` session names, `git worktree list` parsing, attach / kill /
    `work.sh` command builders. Pure functions, unit tested.
  - `src/shell.rs` — quoting, and the login-shell wrapper that gives remote
    commands the user's `PATH` (plain `ssh host tmux` fails on a Mac because
    Homebrew is not on sshd's `PATH`).
  - `src/identity.rs` — the iroh secret key re-expressed as an OpenSSH key.
  - `src/ffi.rs` + `include/pigeons_mobile.h` — the hand-written C ABI.
  - `tests/roost.rs` — end to end: private sshd on a random port, in-process
    roost, the client dialing it over iroh.
- `ios/` — XcodeGen spec (`project.yml`) and Swift sources. The Xcode project
  and the xcframework are generated, not committed.
  - `Pigeons/Core/PigeonsCore.swift` — `RoostClient`, the Swift face of the ABI.
  - `Pigeons/Terminal/` — SwiftTerm subclass, controller, key bar, screen.
  - `Pigeons/Sessions/` — session list and the `work.sh` sheet.
  - `Pigeons/Settings/` — roost, user, paths, prefix, device key.
- `scripts/build-core.sh` — Rust → `ios/PigeonsCore.xcframework`.
- `scripts/archive.sh` — archive and upload to TestFlight.

## Roost setup

On the machine you want to reach, once:

```bash
pigeons roost                       # or: pigeons service install
# note the id it prints
```

tmux and `~/work.sh` must be on that machine, and `~/code` laid out the way
`work.sh` expects (`<project>/.bare` plus one directory per worktree). Sessions
`work.sh` has not created still show up; they land in an "other" group.

Then let the phone in. The app shows its public key under Settings → This
device; append it to `~/.ssh/authorized_keys` for your user. Or type your
password once under Settings → First connection and the app appends the key
itself. The same ed25519 key is the phone's iroh identity, so there is exactly
one key to know about.

## Building

Prerequisites on the Mac that builds:

- Xcode 26 or newer (SwiftTerm 1.99 needs swift-tools 6.2) with the iOS SDK,
  `xcode-select -s /Applications/Xcode.app`.
- Rust with the iOS targets: `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`
  (`build-core.sh` adds them when missing).
- XcodeGen: `brew install xcodegen`.
- An Apple Developer team for signing. Its id goes in `PIGEONS_TEAM_ID`.

```bash
mobile/scripts/build-core.sh                       # Rust → ios/PigeonsCore.xcframework
cd mobile/ios && PIGEONS_TEAM_ID=ABCDE12345 xcodegen generate
open Pigeons.xcodeproj                             # run on a device or the simulator
```

Rebuild the xcframework whenever `core/` changes; Xcode links it statically.
The simulator slice is arm64 only; on an Intel Mac add `x86_64-apple-ios` to
the script and lipo the two simulator libraries.

Core tests run on the Mac without Xcode:

```bash
cd mobile/core && cargo test        # unit tests + the sshd/roost end-to-end test
```

The end-to-end test starts an unprivileged `sshd` on a random port with its
own host key and `authorized_keys`, so it touches nothing on the machine. It
needs network for iroh discovery.

## TestFlight

1. In App Store Connect, create the app with bundle id `computer.n0.pigeons`
   (change it in `ios/project.yml` if you want another).
2. Bump `CURRENT_PROJECT_VERSION` in `ios/project.yml`; each upload needs a new
   build number.
3. `PIGEONS_TEAM_ID=ABCDE12345 mobile/scripts/archive.sh` builds the core,
   regenerates the project, archives, and uploads with the Apple ID signed in
   to Xcode. For an API key instead, export `ASC_KEY_ID`, `ASC_ISSUER_ID`,
   `ASC_KEY_PATH`.
4. Processing takes a few minutes; then add yourself as an internal tester.
   App Store Connect asks about export compliance on the first build: the app
   uses standard encryption (QUIC/TLS and SSH) and no proprietary algorithms.

## How the terminal maps to tmux

- **Attach** runs `exec tmux set-option -g mouse on \; attach-session -t =NAME`
  through the user's login shell. `exec` makes tmux the PTY's process, so the
  channel closes exactly when tmux detaches, and the app returns to the list.
- **Scrolling.** With mouse mode on, SwiftTerm turns a one-finger drag into
  mouse wheel reports, and tmux answers wheel-up in a pane by entering copy
  mode and scrolling its history, leaving again at the bottom. Two fingers
  scroll SwiftTerm's own buffer, which is empty while tmux has the screen. Turn
  mouse mode off in Settings if you would rather keep your own tmux config.
- **Key bar.** Top row sends prefixed tmux keys (`d` detach, `[` copy mode,
  `x` kill pane, `&` kill window, window navigation, bare prefix). Bottom row
  has Esc, Tab, a Ctrl toggle for the next typed key, `^C ^D ^Z ^L ^U ^R`,
  arrows, and the punctuation the soft keyboard hides. The prefix is a setting
  (`C-b` default, tmux notation).
- **Kill session** is in the terminal's menu and as a swipe on the list; it
  runs `tmux kill-session -t =NAME` on an exec channel and asks first.
- **New session** runs `work.sh <project> <tree>` in the PTY, so creating a
  worktree from a branch name, starting its session, and attaching all behave
  exactly as from a laptop. Worktrees that already have a session are marked.

## Background and reconnecting

iOS suspends the app in the background and its sockets die. The SSH keepalive
notices on resume and the core reports Disconnected; the app dials again when
it becomes active. tmux keeps everything running on the roost in between,
which is the point of tmux.

## Troubleshooting

- *authentication rejected*: the key is not in `authorized_keys` for that
  user, or sshd has `PubkeyAuthentication no`. Use the password flow once.
- *tmux: command not found* in the terminal: `PATH` is set only in an
  interactive rc file. The wrapper appends `/opt/homebrew/bin` and
  `/usr/local/bin`; for other locations, set `PATH` in `~/.zprofile` (or
  `~/.profile`) on the roost.
- *cannot find type `in6_ifreq` in crate `libc`* while building `netdev` for
  iOS: netdev 0.46.x uses a macOS-only libc type. `core/Cargo.toml` patches
  netdev to the upstream fix commit (still versioned 0.46.3, which netwatch
  accepts); if the error reappears, make sure the `[patch.crates-io]` entry is
  intact and run `cargo update -p netdev@0.46.3`. Drop the patch once iroh's
  netwatch requires netdev 0.47 or later.
- *unresolved imports `libc::RTA_IFP`, `libc::RTAX_DST`, ... / cannot find
  value `RTM_VERSION`* while building `netwatch` for iOS: libc 0.2.190 made
  those constants macOS-only. `core/Cargo.toml` pins `libc = "=0.2.189"`; if
  something pulls a newer libc in, run `cargo update -p libc --precise
  0.2.189`. Lift the pin when netwatch no longer relies on them on iOS.
- *Linker errors about `_res_9_*` or `_SCDynamicStore*`* when building the app:
  the Rust static library needs `libresolv` and SystemConfiguration, which
  `project.yml` links. Regenerate the project after editing it.
- Settings → Debug → Core log shows what the Rust side did, including the
  iroh dial and the exact commands sent.
