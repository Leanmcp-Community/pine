# Pine server AMI

Build account: **187692046617**. Region: **us-west-2**. CLI profile: **marketplace**.
All build/test commands explicitly use this profile and region and assert the
account identity. Publication is not performed by these scripts.

This AMI runs a native Python service, not the development Docker deployment.

## Buyer startup

Launch with your own EC2 SSH key pair. Allow SSH only from your admin IP.
The initial server binds to localhost:8080. It creates a unique organization
key on first boot; it ships without a preconfigured credential or capture data.

SSH as `ubuntu`, then read your generated key:

```bash
sudo cat /root/pine-upload-key
```

For a private trial, forward the UI to your computer:

```bash
ssh -i your-key.pem -L 8080:127.0.0.1:8080 ubuntu@INSTANCE_IP
```

Open http://127.0.0.1:8080 and enter the key. Keep this tunnel running to use
localhost as the client endpoint. Choose another local port if already occupied.

For a shared server, assign a stable public IP, point DNS to it, allow TCP 80/443,
then run on the instance:

```bash
sudo pine-https capture.example.com
```

Configure Mac clients with the HTTPS endpoint and upload key using the Homebrew
instructions. A trusted HTTPS certificate requires working public DNS and
certificate-authority access to the instance. Do not expose port 8080 directly.

## Paths and administration

- Code and Python environment: `/opt/pine`
- Admin policy: `/etc/pine/config.yaml`
- Key hashes: `/etc/pine/keys.json`
- Initial raw key: `/root/pine-upload-key` (root-readable only)
- Upload database: `/var/lib/pine/capture.sqlite` and SQLite WAL files
- Service: `sudo systemctl status pine`
- Logs: `sudo journalctl -u pine`
- TLS configuration: `/etc/caddy/Caddyfile`

Upload keys currently grant organization-wide read and write access. Browser
SSO and individual permissions are not included. Size policy reloads per request.
Capture content may contain confidential information; structured header filtering
is not arbitrary-content secret removal.

## Backup and upgrades

Stop pine, take an EBS snapshot, and restart it. Retain the instance root
volume or take a backup before termination; the default root volume is deleted
on termination. Restore backups to a separate instance and verify authenticated
retrieval before replacing the original. The snapshot contains credentials and
customer data; keep it private. For upgrades, test a new AMI separately and plan
a stopped-service transfer of `/etc/pine` and `/var/lib/pine`, preserving
ownership and permissions. Schema compatibility must be verified per release.

## Build and test

```bash
python3 marketplace/aws/build_ami.py
python3 marketplace/aws/test_ami.py
```

These create billable temporary EC2 resources. Build state and resource IDs are
recorded in `/tmp/pine-ami-build/resources.json`. The validation script removes
temporary instances, key pair and security group after success, retaining the
private AMI and its EBS snapshot. On failure, inspect recorded IDs and clean up
only those resources. Do not rerun a build over existing state.

AMI preparation does not constitute AWS Marketplace approval. Before publishing,
complete AWS AMI security scans, seller listing metadata, support/EULA/pricing,
clean buyer launch checks and HTTPS/DNS validation. No public offer is created.
