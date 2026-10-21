# Pine cloud plan

1. Preserve local capture: proxy, session layout, and TUI remain independent of cloud availability. Existing files are the durable upload source.
2. Add CLI configuration, status, disable, one-shot sync, and continuous sync. Persist checkpoints, retry with backoff, deduplicate uploads, handle growing streams and rewritten metadata. Historical sessions require explicit backfill.
3. Generate a persistent device ID and use hostname as a label. Require a revocable organization upload key remotely; defer browser login. Never use AI provider keys as cloud credentials. Upload only session files, never CA keys. Filter credential fields in structured logs; content can still contain secrets.
4. Add a portable authenticated server with organization isolation, bounded uploads, durable acknowledgements, session listing and retrieval. Start with a single persistent-volume server; object storage and PostgreSQL are the production scaling milestone.
5. Provide container deployment and AWS/GCP/other-host scripts, HTTPS setup, persistence and operational instructions. Add OS service installation after sync verification.
6. Target customer-hosted deployments first. Marketplace publication is a separate milestone requiring seller accounts, product selection, licensing/billing integration, validation and submission. Containers and scripts are not marketplace listings.
7. Later add OIDC browser login, organization membership, invitations, roles and device credential issuance. Keep identity-provider integration separate from capture.

## First implementation acceptance

CLI and portable single-node server; interrupted/retried uploads are idempotent; streams upload only complete lines; metadata is versioned; credentials in structured headers are removed even for unredacted captures; server errors do not affect local capture. Tests cover isolation, retries, validation and filtering. No existing historical capture is uploaded automatically. No deployments are executed against cloud accounts as part of development.

## Follow-on milestones

Managed object storage/PostgreSQL, web dashboard, signed release binaries, OIDC, enterprise policy and marketplace commercial integration follow the first working increment. AWS and GCP VM deployment use the same container and persistent disk. Back up the volume and key configuration together; do not run multiple server replicas against its SQLite database.
