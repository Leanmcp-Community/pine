## Optional cloud uploads

Basic cloud sync is implemented and tested. You can run your own server on your
laptop or a remote machine. Local capture continues independently of cloud
availability. Browser login, a web dashboard, managed storage, and marketplace
publishing remain follow-on work.

### Run your own server locally

Run these commands from the repository root. You need Rust/Cargo, Python 3, and
Docker Desktop (or Docker Engine with the Compose plugin).

1. Start Docker Desktop, then build and install the client:

   ```bash
   cargo install --path .
   ```

2. Create your organization upload key:

   ```bash
   mkdir -p deploy/secrets
   python3 server/create_key.py my-organization \
     --file deploy/secrets/keys.json
   ```

   Save the key printed by this command; it is shown once. The server key file
   contains its hash. The container runs as UID 10001, so that user must be able
   to read the file. On Linux, you can set ownership with
   `sudo chown 10001:10001 deploy/secrets/keys.json`.

3. Start the local server and check its health:

   ```bash
   docker compose -f deploy/compose.yaml up -d --build server
   curl http://127.0.0.1:8080/health
   ```

   The health check should return `{"status":"ok"}`. Captured data persists in a
   Docker volume. This setup exposes the server only on your own computer.

4. Connect the client:

   ```bash
   export PINE_UPLOAD_KEY='paste-your-key-here'
   pine cloud configure --endpoint http://127.0.0.1:8080
   unset PINE_UPLOAD_KEY
   ```

   Sync includes **all existing and new sessions** by default and skips already
   acknowledged data. `cloud sync --backfill` is also accepted. Uploads include captured prompts,
   responses, and machine metadata. Structured credentials are filtered, but
   arbitrary content and binary bodies can still contain secrets.

5. Run the uploader in a separate terminal:

   ```bash
   pine cloud sync
   ```

   Continue using `pine` normally in another terminal. Check upload status
   with:

   ```bash
   pine cloud status
   ```

   For a single upload pass instead of a continuous worker, use
   `pine cloud sync --once`.

6. Optionally install the uploader as a background service:

   Stop the manually running uploader with Ctrl-C first, then run:

   ```bash
   python3 deploy/client-service.py install
   ```

   This installs a per-user macOS LaunchAgent or Linux systemd service. Remove
   it with:

   ```bash
   python3 deploy/client-service.py uninstall
   ```

### Stop uploads or the local server

Disable uploads while keeping local capture working:

```bash
pine cloud disable
```

An in-flight upload pass can finish. Stop the worker or uninstall its service
for immediate shutdown. Run `cloud configure` again to re-enable uploads.

Stop the local server without deleting its stored data:

```bash
docker compose -f deploy/compose.yaml stop server
```

### Deploy a remote server

Use `deploy/aws.sh` or `deploy/gcp.sh` to provision a VM. These scripts require
cloud credentials and the environment variables listed at the top of each
script; running them creates billable resources. They do not publish a
marketplace listing.

Follow [cloud setup and deployment](docs/CLOUD.md) to install Docker on the VM,
create its organization key file, configure DNS and firewall access, and start
it with HTTPS:

```bash
DOMAIN=capture.example.com ./deploy/start.sh
```

The same container setup works on other Linux Docker hosts. Configure clients
with the server's HTTPS URL and their upload key; clients do not need AWS or GCP
credentials. See the deployment guide for persistence, backup, retrieval API,
and operational limitations, and the [seven-part roadmap](docs/CLOUD_PLAN.md)
for remaining work.

### Validation status

The Rust build/test and four server and client/server integration tests passed.
Shell syntax and Docker Compose configuration were checked. Container runtime
validation was not completed because Docker's daemon was stopped during
implementation. AWS/GCP deployment scripts have not been exercised against a
cloud account.

Run `pine cloud --help` for the available uploader commands.

### Local management and analysis UI

After starting the server, open **http://127.0.0.1:8080/** and connect with your
organization upload key. The key is held only in the browser tab's memory.
The dashboard provides organization totals, paginated/filterable sessions,
file inspection, chunk downloads, request/error counts and mean request duration
for the displayed traffic chunk, and a server health check. These are captured
request metrics, not billing or token estimates. Refresh to see new uploads.
The UI cannot control the uploader process on another computer; use the CLI or
service installer for that. Browser identity login, user roles and key management
remain future work.

To apply UI updates to an already running local server:

```bash
docker compose -f deploy/compose.yaml up -d --build server
```

For automatic syncing, configure your key once as above, then run:

```bash
python3 deploy/client-service.py install
pine cloud status
```

The user service automatically starts the uploader during your login lifecycle.
On macOS, inspect it with `launchctl print gui/$(id -u)/com.pine.sync`.

### Sync progress

`pine cloud sync` shows a progress bar for each session with new uploads,
including the current file and acknowledged source bytes. Interactive terminals
update the current line; redirected output uses plain log lines. Percentages
refer to the files observed at the beginning of that scan, not the eventual
size of a live conversation. Filtering may change the actual network byte count.
Large files continue on subsequent scans; incomplete JSONL lines wait for more
data. When idle, the worker reports that it is watching for new data. Restart an
already running uploader after installing an updated CLI to see these messages.

### Session size limit and stopping sync

Only sessions **smaller than 200 MiB** (209,715,200 logical file bytes) are
eligible for upload. The worker counts all regular files recursively without
following symlinks, and skips sessions at or above the limit. Size is checked
on each scan; local data and previously uploaded chunks are preserved.

Stop an existing foreground or background uploader with:

```bash
pine cloud stop
```

This requests a clean stop after the current request (up to its 30-second
timeout). It preserves checkpoints. The installed service does not restart a
successfully stopped worker. Once it has exited, run `pine cloud sync` for
foreground progress, or `python3 deploy/client-service.py install` to restart
the background service. A new uploader clears the previous stop request.

### Administrator configuration and server files

Edit `deploy/config/config.yaml` on the server host:

```yaml
sync_enabled: true
max_session_mib: 200
max_chunk_mib: 8
```

Clients fetch the authenticated `/v1/config` policy before every scan. The server
also rejects uploads when paused or when accumulated session source bytes reach
the limit. Invalid/missing configuration pauses uploads rather than silently
using defaults. Changes apply on the next scan/request; no rebuild is needed.
Size settings use MiB; the protocol currently caps chunks at 8 MiB. Client
credentials, endpoint, local paths and OS service settings remain local; the
server policy controls upload eligibility and limits. Existing uploaded data is
not deleted when a limit is lowered.

- `server/app.py`: ingestion API and dashboard routes.
- `server/index.html`: web UI.
- `server/Dockerfile`: server image.
- `deploy/compose.yaml`: Docker services and persistent mounts.
- `deploy/config/config.yaml`: administrator policy, mounted read-only at `/etc/pine/config.yaml`.
- `deploy/secrets/keys.json`: organization key hashes, mounted at `/run/secrets/keys.json`.
- Docker volume `deploy_capture`: uploaded data and indexes, in `/data/capture.sqlite`
  inside the container (including SQLite WAL files while running).

On Docker Desktop, that volume lives inside Docker's Linux VM, not directly in
your macOS repository. Inspect it with `docker volume inspect deploy_capture`.
Do not delete the volume or run `docker compose down -v` unless you intend to
remove the uploaded data. Local original captures remain under `~/.pine/sessions/`.
