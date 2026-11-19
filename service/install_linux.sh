set -e

if ! /usr/bin/nc -z 127.0.0.1 [SSHPORT] 2>/dev/null && ! nc -z 127.0.0.1 [SSHPORT] 2>/dev/null; then
    echo "Warning: no sshd detected on port [SSHPORT]. Pigeons won't be able to deliver connections."
    echo "  Make sure sshd is running before sending pigeons to this roost."
    echo ""
fi

cat > /etc/systemd/system/pigeons.service <<'UNIT_EOF'
[Unit]
Description=pigeons

[Service]
Type=simple
WorkingDirectory=~
Environment=RUST_LOG=info
ExecStart=/bin/bash -c '[BINARYPATH] roost --ssh-port [SSHPORT][RELAYARGS]'
Restart=on-failure
RestartSec=3s

[Install]
WantedBy=multi-user.target
UNIT_EOF

if systemctl is-active --quiet pigeons.service 2>/dev/null; then
    systemctl restart pigeons.service
else
    systemctl enable pigeons.service
    systemctl start pigeons.service
fi
