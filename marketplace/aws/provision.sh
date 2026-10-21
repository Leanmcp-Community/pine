#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get upgrade -y
apt-get install -y python3-venv caddy
id pine >/dev/null 2>&1 || useradd --system --home /var/lib/pine --shell /usr/sbin/nologin pine
python3 -m venv /opt/pine/venv
/opt/pine/venv/bin/pip install -r /opt/pine/requirements.txt
install -d -m 750 -o pine -g pine /var/lib/pine
install -d -m 750 -o root -g pine /etc/pine
cp /opt/pine/config.yaml /etc/pine/config.yaml
cat > /usr/local/sbin/pine-initialize <<'PY'
#!/opt/pine/venv/bin/python
import hashlib,json,secrets,os,pwd
from pathlib import Path
path=Path('/etc/pine/keys.json')
if not path.exists():
    os.umask(0o077)
    key=secrets.token_urlsafe(32)
    Path('/root/pine-upload-key').write_text(key+'\n')
    path.write_text(json.dumps({hashlib.sha256(key.encode()).hexdigest():'default'}))
    os.chown(path,0,pwd.getpwnam('pine').pw_gid)
    path.chmod(0o640)
PY
chmod 755 /usr/local/sbin/pine-initialize
cat > /etc/systemd/system/pine-initialize.service <<'UNIT'
[Unit]
Description=Initialize unique Pine credentials
Before=pine.service
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/pine-initialize
RemainAfterExit=yes
UNIT
cat > /etc/systemd/system/pine.service <<'UNIT'
[Unit]
Description=Pine capture server
After=network.target pine-initialize.service
Requires=pine-initialize.service
[Service]
User=pine
Group=pine
WorkingDirectory=/opt/pine
Environment=PINE_DATA=/var/lib/pine
Environment=PINE_KEYS_FILE=/etc/pine/keys.json
Environment=PINE_CONFIG=/etc/pine/config.yaml
ExecStart=/opt/pine/venv/bin/uvicorn app:app --host 127.0.0.1 --port 8080 --no-access-log
Restart=on-failure
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/pine
[Install]
WantedBy=multi-user.target
UNIT
cat > /usr/local/sbin/pine-https <<'PY'
#!/usr/bin/python3
import re,sys,subprocess
from pathlib import Path
if len(sys.argv)!=2 or not re.fullmatch(r'[a-zA-Z0-9](?:[a-zA-Z0-9.-]*[a-zA-Z0-9])?',sys.argv[1]):
    sys.exit('Usage: sudo pine-https capture.example.com')
Path('/etc/caddy/Caddyfile').write_text(sys.argv[1]+' {\n reverse_proxy 127.0.0.1:8080\n}\n')
subprocess.run(['systemctl','enable','--now','caddy'],check=True)
subprocess.run(['systemctl','reload','caddy'],check=True)
PY
chmod 755 /usr/local/sbin/pine-https
systemctl disable --now caddy
systemctl daemon-reload
systemctl enable pine
printf 'PasswordAuthentication no\nPermitRootLogin no\n' > /etc/ssh/sshd_config.d/00-pine.conf
# Do not initialize credentials on the builder. Each buyer generates unique keys.
touch /opt/pine/build-ready
