# Pine Cloud: MVP plan

**Goal:** a 3-person team installs `pine`, logs in, and sees each other's
sessions live in a browser. They can name checkpoints and pull a teammate's
session or checkpoint to their own machine. Recording always works locally,
logged in or not.

---

## 1. Architecture

```
 laptop (each teammate)                         Vercel
 ┌──────────────────────────┐                  ┌───────────────────────────────┐
 │ pn / pine (Rust)         │                  │ web/  (Next.js, Bun)          │
 │  proxy → ~/.pine/sessions│── HTTPS ───────▶ │  /api/cli/*   device login     │
 │  sync worker (tails files│   bearer token   │  /api/v1/*    chunks, sessions,│
 │   uploads new bytes ~2s) │                  │               checkpoints      │
 │  ~/.pine/auth.json       │◀── pull ──────── │  pages: sessions, viewer,      │
 └──────────────────────────┘                  │   checkpoints, team, /cli      │
                                               └───────┬──────────────┬────────┘
   browser: app.usepine.dev ── GitHub login ─▶         │              │
   (GitHub App: repo access = view)                  ▼              ▼
                                              Neon Postgres     Vercel Blob
                                              (metadata)        (session bytes)
```

- **Local first.** The proxy always writes to `~/.pine/sessions/<id>/`, as it does now. Sync is a separate worker that reads those files. If the network is down or you're logged out, nothing is lost: the worker catches up later from saved byte offsets. The current `cloud.rs` already works this way.
- **One web app** in `web/` in this repo, deployed as its own Vercel project at `app.usepine.dev`. The landing page stays at `usepine.dev`.
- It replaces the self-hosted `server/app.py` (FastAPI + SQLite + Docker) for the hosted product. See open question 1.

## 2. Stack choices

| Need | Pick | Why |
|---|---|---|
| Login | **Native GitHub login through a GitHub App** (plus Google later), with **Better Auth** (open source; users and sessions live in *our* Postgres) | No vendor lock-in, and every permission GitHub grants is ours to use. See §2a. |
| Teams and access | **GitHub itself** | The GitHub org is the team. Repo access decides who can see a session. No invite system to build. |
| Metadata | **Neon Postgres** (Vercel Marketplace) | Relational data: orgs → users → devices → sessions → checkpoints. Team lists filtered by time and name are a single SQL query. |
| Session bytes | **Vercel Blob** | One object per uploaded chunk. Cheap, no AWS account needed. |
| Web + API | **Next.js on Vercel, Bun** | One deploy for the dashboard and the API the CLI talks to. |

**Why not DynamoDB?**
- **Item size:** each item is limited to 400 KB, and captured request and response bodies are often larger. We'd need S3 for the bytes anyway.
- **Queries:** the queries we need ("sessions in repos I can read, by date", "checkpoints named X", "who's online") fit SQL naturally.
- **Setup:** it brings in AWS accounts and IAM for no MVP benefit.

DynamoDB plus S3 is a fine scale-out path later. For now the blob keys are plain paths, so moving them to S3 or R2 is a copy.

**Vercel limit to respect:** a function request body can be at most **4.5 MB**. So the CLI sends raw bytes (`application/octet-stream`, with metadata in headers) in chunks of **at most 3 MB**. Today it sends 4 MB; that has to come down.

## 2a. Access = GitHub repo access

**Rule:** a session belongs to the repo it was recorded in. You can read the
session only if you can read that repo on GitHub right now. Example: an org
has 100 people and a repo is readable by 3 of them. Those 3 see its sessions,
and the other 97 don't even see that they exist.

**How it works:**
1. **One GitHub App ("Pine"),** installed by an org admin on all or selected repos. A GitHub App beats a plain OAuth app here:
   - **Narrower access:** it asks only for repository *Metadata: read* and *Members: read*. It never reads code.
   - **Tokens:** they're short-lived.
   - **Events:** it receives webhooks when access changes.
   - **Login:** it also signs users in.
2. **Tagging a session:** when a session starts, the CLI records the repo from `cwd`, using `git remote get-url origin` to get `owner/repo`, plus the branch and commit. These ride along with the first chunk.
3. **Upload check:** the server checks that the uploader can actually access that repo. Otherwise anyone could label a session with someone else's repo.
4. **Read check:** done at read time, every time, never by copying a member list.
   - **The call:** `GET /repos/{owner}/{repo}` with the *user's* GitHub-App token. A 200 means they can read the repo.
   - **Caching:** results are cached per user and repo for 10 minutes in Postgres.
   - **Clearing the cache early:** these webhooks clear it: `member`, `membership`, `team_add`, `organization` and `installation_repositories`.
   - **Effect:** removing someone from the repo removes their access to its sessions within minutes.
5. **Not in a GitHub repo:** if the folder isn't a git repo, or the remote isn't on GitHub, the session is **private to its owner**. They can share it explicitly later. This is the safe default.

**Caveats:**
- **Loose boundary:** a session can contain more than the repo, such as files read from elsewhere, pasted secrets and other projects. Repo access is a good boundary, not a perfect one.
- **Pulled copies:** sessions already pulled to a laptop can't be taken back. Revocation only stops future reads.
- **Google sign-in:** a Google-only user has no GitHub identity, so they can see only their own sessions until they link GitHub. That's why the MVP is GitHub-only.

**CLI login without a web page:** GitHub Apps support the **device flow**, the same one `gh auth login` uses.
1. `pn login` prints a code and opens `github.com/login/device`.
2. The CLI receives a GitHub user token.
3. It exchanges that token at `/api/cli/exchange` for a Pine token tied to the device.
4. The GitHub token stays on the server, which refreshes it there.

## 3. Data model (Postgres)

```
users(id, github_id, github_login, email, name, avatar_url)        -- + Better Auth's own tables
github_tokens(user_id, access_token_enc, refresh_token_enc, expires_at)  -- encrypted at rest
installations(id, github_installation_id, account_login, account_type)  -- one per GitHub org/user
devices(id, user_id, hostname, os, arch, pine_version, last_seen_at)
cli_tokens(id, device_id, token_hash, created_at, revoked_at)
repo_access(user_id, repo_full_name, can_read, checked_at)          -- 10-minute cache
sessions(id ulid, user_id, device_id, repo_full_name NULL, branch, commit,
         target, title, started_at, last_chunk_at, bytes, status)  -- NULL repo = owner-only
chunks(session_id, file, offset, length, sha256, blob_url, created_at)
       PRIMARY KEY (session_id, file, offset)           -- makes re-uploads idempotent
checkpoints(id, session_id, user_id, name, note,
            offsets jsonb, created_at)                  -- {file: byte_offset} at that moment
```

## 4. CLI (`pine`, with the alias `pn`)

| Command | Behaviour |
|---|---|
| `pn login` | GitHub device flow: the CLI prints a code and opens `github.com/login/device`. You enter the code and approve the Pine app. The CLI exchanges the GitHub token for a Pine token and saves it to `~/.pine/auth.json` (mode 0600). Sync turns on. |
| `pn logout` | Revokes the token on the server and deletes `auth.json`. Recording continues locally. |
| `pn whoami` | Shows GitHub login, email, installed orgs, device and sync state (live, n chunks pending). Logged out: *"Not logged in. Recording locally to ~/.pine/sessions. Run `pn login` to share with your team."* Offline: cached identity plus "offline, n chunks queued". |
| `pn` (run Claude etc.) | Unchanged. When logged in, it also starts the sync worker inside the same process, so the session streams up **in real time** (new bytes about every 2s). A detached background sync stays optional. |
| `pn sync` | Runs the sync worker in the foreground, catching up on everything. `pn sync --once` does one pass. `pn sync status` shows progress. `pn sync pause` / `pn sync resume` toggle uploads. |
| `pn checkpoint "name" [-m note] [--session ID]` | Names the session at its current state: the byte offset of every file **right now**. The default session is the current or most recent one. Saved locally and synced. |
| `pn checkpoints [--mine]` | Lists the team's checkpoints: name, who, when, which session. |
| `pn sessions [--team]` | Lists local sessions, or the team's sessions with live markers. |
| `pn pull <checkpoint-name or session-id>` | Downloads it to `~/.pine/sessions/<id>/`, marked as pulled and read-only. A checkpoint downloads only up to its offsets. |
| `pn open [ID]` | Opens the session in the web viewer. |
| `pn -i` | Interactive menu, described below. |

**`pn -i` for the MVP:** a keyboard menu with these entries:
- Who am I / log in
- Sync on/off + status
- Browse team checkpoints → pull / open
- Browse sessions
- Settings: hosts, sync

Built with a small prompt crate (`inquire` or `dialoguer`) inside the Rust binary, so there's nothing extra to install. A full-screen `ratatui` session viewer comes after the MVP. The existing Python Textual viewer in `packages/tui` can't ship inside the binary.

The existing `pine cloud configure/sync/status`, with its shared org upload key, is replaced by per-user login and `pn sync`.

## 5. Real-time sync and checkpoints

- **Uploading:** the worker scans `~/.pine/sessions/*/`. For each file it uploads the bytes from its last acknowledged offset up to the current end, in chunks of at most 3 MB. The request is `POST /api/v1/chunks` with these headers: `session`, `file`, `offset`, `sha256`, and session metadata on the first chunk.
- **Server side:** the server writes the blob, then inserts the `chunks` row. An offset it has already seen returns 200, so retries are safe. Secret filtering (`sanitize`) stays on the client, before upload.
- **Speed:** while a session is active, polling runs every 2s. When idle, it backs off to 30s.
- **Live viewer:** the web page polls `GET /api/v1/sessions/:id?after=<cursor>` every 3s. That's enough to feel live for the MVP; SSE can come later.
- **Checkpoints:** a checkpoint is just `{file: offset}` plus a name, which is cheap and exact. Pulling or viewing a checkpoint reassembles each file's chunks up to its offset.

## 6. Security and privacy (must be said in onboarding)

- **Visibility:** everything captured is visible to everyone who can read the session's GitHub repo: prompts, code, tool output. Credentials are filtered, but arbitrary text can still contain secrets.
- **Opting out:** `pn sync pause` stops uploads. Per-session "private" comes after the MVP.
- **Tokens:** stored hashed on the server, revocable per device, scoped to one user and device. GitHub tokens are stored encrypted and never sent to the CLI.
- **Clean-up before launch:** rotate the upload key that's committed in `HASH`, and delete that file.

## 7. Distribution (no Homebrew)

- **Where the binaries live: GitHub Releases**, not Actions artifacts. Artifacts expire after 90 days and need a GitHub login to download. Releases are public, permanent links: `github.com/Leanmcp-Community/pine/releases/latest/download/pine-<target>.tar.gz` (+ `.sha256`). `release-edge.yml` already uploads to Releases. It has **never succeeded**, for two reasons:
  - `VERSION` is set but not `export`ed, so the packaging step fails with `KeyError: 'VERSION'`.
  - The `macos-13` (Intel) runner no longer exists, so that job hangs for 24 hours. Instead, build `x86_64-apple-darwin` on `macos-14` with `--target`.
- **Multiple sources:**
  1. GitHub Releases is the primary source.
  2. Mirror the same files to a bucket at `dl.usepine.dev` (Cloudflare R2 or Vercel Blob) from the same CI job. `install.sh` tries the mirror first and falls back to GitHub.
  3. `usepine.dev/install` serves `scripts/install.sh` (a Vercel rewrite), so the install command is `curl -fsSL https://usepine.dev/install | bash`.
  4. npm and `cargo install` remain as extra channels.
- **Remove Homebrew:** delete `Formula/` and `docs/HOMEBREW.md`, the README sections, and the Homebrew guard in `update.rs`.

## 8. Cross-compiling

| Target | How |
|---|---|
| macOS arm64 and x86_64 | Both on one `macos-14` runner: `rustup target add x86_64-apple-darwin`, then `cargo build --target …`. Optionally `lipo` the two into one universal binary. macOS targets can't realistically be built from Linux (SDK licence). |
| Linux x86_64 and arm64 | Native `ubuntu-22.04` and `ubuntu-22.04-arm` runners, as today. Better: build **musl** (`*-unknown-linux-musl`) for fully static binaries. That drops the glibc 2.35 requirement, so they run on any distro; rustls + ring work fine on musl. |
| Linux from your Mac (local testing) | `cargo zigbuild --target x86_64-unknown-linux-musl` (cargo-zigbuild uses Zig as the linker) or `cross` (Docker). |
| Windows (later) | A `windows-latest` runner with `x86_64-pc-windows-msvc`, or `cargo-xwin` from Linux. The proxy's Unix-only parts (`libc`) need a check first. |

## 9. Build order for tonight → demo tomorrow

| # | Task | Est. |
|---|---|---|
| 0 | Fix the release workflow (export VERSION, macOS runner), push, and confirm `install.sh` downloads a binary | 30m |
| 1 | `web/`: Next.js + Better Auth (GitHub) + Neon + Blob; create the GitHub App. Deploy to Vercel at `app.usepine.dev` | 2h |
| 2 | API: GitHub device-flow exchange, `/me`, chunk upload (+ repo check), sessions list/detail filtered by repo access, checkpoints, webhooks | 3h |
| 3 | CLI: `login`, `logout`, `whoami` (offline-safe); switch sync to bearer token + octet-stream ≤3 MB; in-process live sync | 2h |
| 4 | CLI: `checkpoint`, `checkpoints`, `pull`, `open` | 1h |
| 5 | Web: sessions list (live dot), session viewer (conversation rendered from the logs), checkpoints page | 1.5h |
| 6 | `pn -i` menu | 45m |
| 7 | Test end to end with 2 machines, 2 GitHub users, 1 repo each can and can't read; write a 5-line onboarding note | 45m |

**Total:** about 11.5 hours, which is tight for tomorrow morning.

**If time runs short, cut in this order:**
1. `pn -i` (whoami and checkpoints already cover it from the CLI).
2. The live web polling (a refresh button works).
3. The mirror (GitHub alone is fine for 3 people).

## 10. Open questions

1. **Self-hosted server:** keep the self-hosted `server/app.py` + AWS Marketplace AMI path, or retire it in favour of the hosted app?
2. **Resuming a pulled session:** should a teammate be able to **resume** a pulled checkpoint in their own Claude Code? It's possible: the last captured request already contains the whole message history. It's post-MVP, but it decides how we store checkpoints.
3. **Default visibility:** repo-scoped (decided). Non-GitHub sessions are owner-only; is an explicit "share with user X" needed for the MVP?
4. **Domain:** `app.usepine.dev` for the app, OK?
