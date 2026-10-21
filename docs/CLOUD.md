# Cloud uploads: first implementation

Local capture works without cloud configuration. Cloud is an optional, separate process. This release provides single-node ingestion and retrieval, not a web dashboard or marketplace listing. The server stores chunks and indexes transactionally in SQLite on a persistent volume. Do not run multiple replicas or put its database on a network filesystem.

## Local trial

Install the CLI with `cargo install --path .`. Create a separate test home if you do not want to configure your normal capture directory.

```bash
mkdir -p deploy/secrets
python3 server/create_key.py my-organization --file deploy/secrets/keys.json
```

Save the printed key securely; it is shown once. The file contains hashes, not keys. The container runs as UID 10001: ensure the secrets directory is traversable and keys.json readable by that UID (for example on Linux, `sudo chown 10001:10001 deploy/secrets/keys.json`). Keep the file out of git.

```bash
docker compose -f deploy/compose.yaml up -d --build server
export PINE_UPLOAD_KEY='<printed key>'
pine cloud configure --endpoint http://127.0.0.1:8080
unset PINE_UPLOAD_KEY
pine cloud sync --once
pine cloud status
pine cloud sync
```

Configuration stores the key in a mode-0600 file under PINE_HOME. Hostname is a label; a persistent ULID identifies the device. The server maps each key to an organization. Use a distinct key per device for independent revocation. Replacing a key on a configured client is for rotation within the same organization; use a separate PINE_HOME when switching organization or server.

Sync includes all existing and new sessions by default. Already acknowledged data is skipped. `cloud sync --backfill` is accepted for compatibility. Configure displays the content upload notice. Prompts, responses, body binaries, host metadata and file paths can contain private content. Structured credential fields and header pairs are redacted for upload; arbitrary strings and binary bodies are preserved and are **not guaranteed secret-free**. The CA directory is never scanned. Local captures are never changed or deleted.

The worker scans sessions in bounded chunks, only uploads complete JSONL lines and retries with exponential backoff capped at 60 seconds. Lines larger than 4 MiB produce an error instead of being silently skipped. Metadata is uploaded as versioned snapshots. A crash before saving the local checkpoint repeats an idempotent upload. A single worker lock prevents concurrent checkpoint writers on Unix. No Windows service or concurrency support is promised in this first release. Status reports tracked files and last successful pass, not a complete backlog estimate. Errors appear on the worker's stderr.

`pine cloud disable` stops uploading on the next pass. An in-flight pass can finish; stop the worker/service for immediate shutdown. Run configure to re-enable it. Revocation is immediate for subsequent server requests: remove the corresponding SHA256 entry from keys.json. The server reloads keys per request.

## Background service

After configuring, run `python3 deploy/client-service.py install`. On macOS this creates a user LaunchAgent; on Linux a user systemd service. These run in the user's login lifecycle. Linux operation after logout requires administrator-configured user lingering. Uninstall with `python3 deploy/client-service.py uninstall`. No service is installed by the ordinary build/install script.

## AWS, GCP and other servers

`deploy/aws.sh` creates an EC2 VM using an explicitly supplied Ubuntu AMI, subnet, security group and SSH key pair. `deploy/gcp.sh` creates an Ubuntu Compute Engine VM using an explicitly supplied project, zone and subnet. Review scripts before running: they create billable resources. Both retain the boot disk on VM deletion. They do not create marketplace products, DNS, firewall rules or install Docker.

1. Set the variables described at the top of the selected script and run it. Restrict SSH to your admin network; allow public TCP 80/443. Keep 8080 private.
2. Install Docker Engine and the Compose plugin on the VM, copy this repository to it, and create the organization key file as above. These same steps work on any Linux Docker host.
3. Assign a stable IP, point your DNS name to it, then run `DOMAIN=capture.example.com ./deploy/start.sh`. Caddy obtains and renews the HTTPS certificate. Back up its volume as well as the capture volume.
4. Configure each client with `https://capture.example.com` and its upload key. Clients need no AWS/GCP credentials.
5. Check `/health`, upload a synthetic session and retrieve it before enabling real data.

Backups: stop the server briefly (`docker compose -f deploy/compose.yaml stop server`) and snapshot/copy the capture volume, including SQLite WAL files, plus the secrets directory. Restart afterward. Restore into a separate deployment and test retrieval. Keep a stable IP/DNS during VM replacement. Upgrades: back up first, then `docker compose -f deploy/compose.yaml up -d --build`. Roll back to the prior source/image if necessary; this initial schema has no migration mechanism. Storage grows indefinitely; monitor free space and arrange retention/export policies before broad rollout.

## Retrieval API

All `/v1` endpoints require `Authorization: Bearer <upload-key>` and are scoped to its organization. Keys currently grant both upload and read access; separate read/admin roles are future work.

- `GET /v1/sessions?limit=100&offset=0`: paginated devices and sessions.
- `GET /v1/files/{device}/{session}`: relative captured filenames.
- `GET /v1/chunks/{device}/{session}?path=traffic.jsonl&offset=0`: one stored chunk. Advance offset by the returned X-Source-Length, not the downloaded byte count, because filtering changes lengths. Concatenate chunks for reconstruction. A 404 means no chunk at that offset yet. For meta.json, offset zero returns the latest stored version.

An actively uploaded session can be incomplete; consumers should retry later. Health checks validate database availability but do not certify backup or disk capacity.

## Validation

```bash
cargo test
python3 -m venv /tmp/pine-test
/tmp/pine-test/bin/pip install -r server/requirements.txt httpx
(cd server && /tmp/pine-test/bin/python -m unittest -v)
```

See CLOUD_PLAN.md for the remaining managed storage, organization login, dashboard, release packaging and marketplace milestones. Cloud-provider scripts require account-level smoke tests; they are deployment helpers, not verified marketplace integrations.

Deployment command references: [AWS EC2 run-instances](https://docs.aws.amazon.com/cli/latest/reference/ec2/run-instances.html), [GCP instances create](https://docs.cloud.google.com/sdk/gcloud/reference/compute/instances/create).
