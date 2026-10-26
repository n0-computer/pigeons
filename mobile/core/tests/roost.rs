//! End to end: a private sshd, an in-process roost in front of it, and the
//! mobile client dialing the roost over iroh and speaking SSH through it.
//!
//! Needs `/usr/sbin/sshd` and `ssh-keygen` (present on macOS and most Linux
//! boxes) and network access for iroh discovery. Skips itself otherwise. The
//! sshd runs unprivileged on a random port with its own host key and
//! `authorized_keys`, so nothing on the machine is touched.

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

use iroh_pigeons::{RoostConfig, Tunnel};
use pigeons_mobile::{
    Client, ConnectionState, Events, ExecOutput, RequestId, Settings, TerminalExit, TerminalId,
};
use tokio::{sync::mpsc, time};

const SSHD: &str = "/usr/sbin/sshd";

#[derive(Debug)]
enum Ev {
    State(ConnectionState, Option<String>),
    Result(RequestId, Result<String, String>),
    Output(TerminalId, Vec<u8>),
    Exit(TerminalId, TerminalExit),
}

struct TestEvents(mpsc::UnboundedSender<Ev>);

impl Events for TestEvents {
    fn state(&self, state: ConnectionState, detail: Option<&str>) {
        let _ = self.0.send(Ev::State(state, detail.map(str::to_string)));
    }
    fn result(&self, request: RequestId, result: Result<String, String>) {
        let _ = self.0.send(Ev::Result(request, result));
    }
    fn terminal_output(&self, terminal: TerminalId, bytes: &[u8]) {
        let _ = self.0.send(Ev::Output(terminal, bytes.to_vec()));
    }
    fn terminal_exit(&self, terminal: TerminalId, exit: TerminalExit) {
        let _ = self.0.send(Ev::Exit(terminal, exit));
    }
}

/// Wait up to `secs` for an event `f` accepts, returning what it produced.
/// Other events are logged and dropped.
async fn wait_for<T>(
    rx: &mut mpsc::UnboundedReceiver<Ev>,
    secs: u64,
    what: &str,
    mut f: impl FnMut(Ev) -> Option<T>,
) -> T {
    let deadline = time::Instant::now() + Duration::from_secs(secs);
    loop {
        let ev = time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("events channel closed");
        eprintln!("event: {ev:?}");
        if let Some(t) = f(ev) {
            return t;
        }
    }
}

struct Sshd {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Sshd {
    fn start(authorized_key: &str) -> Self {
        let dir = tempfile::tempdir().unwrap().keep();
        let keygen = |name: &str| {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", ""])
                .arg("-f")
                .arg(dir.join(name))
                .status()
                .unwrap();
            assert!(st.success(), "ssh-keygen failed");
        };
        keygen("host_key");
        std::fs::write(dir.join("authorized_keys"), format!("{authorized_key}\n")).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\n\
             AuthorizedKeysFile {d}/authorized_keys\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nStrictModes no\n\
             UsePAM no\nPidFile none\nLogLevel VERBOSE\n",
            d = dir.display()
        );
        std::fs::write(dir.join("sshd_config"), config).unwrap();
        let child = Command::new(SSHD)
            .arg("-D")
            .arg("-f")
            .arg(dir.join("sshd_config"))
            .arg("-E")
            .arg(dir.join("sshd.log"))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sshd");
        let sshd = Self { child, port, dir };
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return sshd;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("sshd did not start:\n{}", sshd.log());
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("sshd.log")).unwrap_or_default()
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn exec_output(payload: &str) -> ExecOutput {
    let v: serde_json::Value = serde_json::from_str(payload).expect("exec payload is JSON");
    ExecOutput {
        exit_code: v["exit_code"].as_u64().map(|c| c as u32),
        stdout: v["stdout"].as_str().unwrap_or_default().to_string(),
        stderr: v["stderr"].as_str().unwrap_or_default().to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobile_client_reaches_sshd_through_a_roost() {
    if !Path::new(SSHD).exists() {
        eprintln!("skipping: {SSHD} not found");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,pigeons_mobile=debug,iroh_pigeons=debug")
        .try_init();

    // The mobile client comes first: sshd needs its public key.
    let data_dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = Client::new(data_dir.path().to_path_buf(), Arc::new(TestEvents(tx)))
        .await
        .unwrap();
    let key = client.authorized_key().unwrap();
    assert!(key.starts_with("ssh-ed25519 "));
    assert!(
        data_dir.path().join("pigeons_ed25519").exists(),
        "device key persisted"
    );
    client.set_settings(Settings {
        tmux_mouse: false, // never touch the developer's real tmux server
        ..Settings::default()
    });

    let sshd = Sshd::start(&key);

    // A roost in front of that sshd, exactly as `pigeons roost --ssh-port` would run.
    let mut roost = Tunnel::builder_ephemeral().await.unwrap();
    roost.roost = Some(RoostConfig {
        ssh_port: sshd.port,
    });
    let roost = roost.build().await.unwrap();
    roost.endpoint().online().await;
    let roost_id = roost.endpoint().id();

    let user = std::env::var("USER").expect("USER");
    client.connect(roost_id, user.clone(), None);
    wait_for(&mut rx, 90, "connected", |ev| match ev {
        Ev::State(ConnectionState::Connected, _) => Some(()),
        Ev::State(ConnectionState::Failed, d) => {
            panic!("connect failed: {d:?}\nsshd log:\n{}", sshd.log())
        }
        _ => None,
    })
    .await;
    assert_eq!(client.state(), ConnectionState::Connected);

    // exec: the login-shell wrapper must find tmux where plain `ssh host tmux` would not.
    let id = client.exec("echo e2e-$USER; command -v tmux".to_string());
    let out = wait_for(&mut rx, 30, "exec result", |ev| match ev {
        Ev::Result(r, res) if r == id => Some(exec_output(&res.expect("exec ok"))),
        _ => None,
    })
    .await;
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert!(out.stdout.contains(&format!("e2e-{user}")), "{out:?}");
    if Path::new("/opt/homebrew/bin/tmux").exists() {
        assert!(out.stdout.contains("/tmux"), "tmux not on PATH: {out:?}");
    }

    // list_sessions: whatever the machine has, the payload must be an overview.
    let id = client.list_sessions();
    let overview = wait_for(&mut rx, 30, "session list", |ev| match ev {
        Ev::Result(r, res) if r == id => Some(res.expect("list ok")),
        _ => None,
    })
    .await;
    let v: serde_json::Value = serde_json::from_str(&overview).unwrap();
    assert!(
        v["sessions"].is_array() && v["projects"].is_array(),
        "{overview}"
    );

    // A PTY shell: type a command, see its output, exit cleanly.
    let term = client.open_shell(80, 24);
    client.terminal_write(term, b"echo term-$((20+22))\n".to_vec());
    let mut seen = Vec::new();
    wait_for(&mut rx, 30, "terminal echo", |ev| match ev {
        Ev::Output(t, bytes) if t == term => {
            seen.extend_from_slice(&bytes);
            String::from_utf8_lossy(&seen)
                .contains("term-42")
                .then_some(())
        }
        Ev::Exit(t, exit) if t == term => panic!("terminal exited early: {exit:?}"),
        _ => None,
    })
    .await;
    client.terminal_resize(term, 100, 40);
    client.terminal_write(term, b"exit\n".to_vec());
    let exit = wait_for(&mut rx, 30, "terminal exit", |ev| match ev {
        Ev::Exit(t, exit) if t == term => Some(exit),
        _ => None,
    })
    .await;
    assert_eq!(
        exit,
        TerminalExit {
            exit_code: Some(0),
            error: None
        }
    );

    // Attaching to a session that does not exist ends with tmux's error.
    let term = client.attach_session("pigeons-mobile-no-such-session".to_string(), 80, 24);
    let mut seen = Vec::new();
    let exit = wait_for(&mut rx, 30, "attach failure", |ev| match ev {
        Ev::Output(t, bytes) if t == term => {
            seen.extend_from_slice(&bytes);
            None
        }
        Ev::Exit(t, exit) if t == term => Some(exit),
        _ => None,
    })
    .await;
    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.contains("can't find session")
            || text.contains("no server running")
            || text.contains("no sessions"),
        "unexpected tmux output: {text:?}"
    );
    assert_eq!(exit.exit_code, Some(1), "{exit:?}");

    client.disconnect();
    wait_for(&mut rx, 10, "disconnected", |ev| match ev {
        Ev::State(ConnectionState::Disconnected, _) => Some(()),
        _ => None,
    })
    .await;
    roost.close().await.unwrap();
}
