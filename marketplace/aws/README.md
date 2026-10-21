# AWS Marketplace submission preparation

Status: private AMI built and fresh-instance smoke tests passed in account
187692046617, us-west-2, using profile marketplace. A Catalog product
`prod-lfxlsiyd4wqnq` exists in **Draft** with a title and nothing else. No delivery
option, offer, price, or public listing has been created.

See [BUILD_RESULT.md](BUILD_RESULT.md) for the image ID and validation limits,
[AMI_USAGE.md](AMI_USAGE.md) for buyer instructions, and
[PUBLISH.md](PUBLISH.md) for the remaining publication steps and what still
blocks them.

## Delivery model

Proposed first product: a customer-hosted, single-node Pine server delivered
as an Ubuntu-based x86_64 AMI. The Mac client is installed separately through
the install script. This matches the current SQLite/persistent-disk architecture. Hosted
SaaS procurement would require a separate onboarding and entitlement integration.
Pricing and seller identity must be provided by the owner before submission.

## Listing draft

Name: Pine Server

Short description: Store and inspect AI tool captures in your own AWS account,
with a Mac client that captures locally and syncs using organization credentials.

Highlights:
- Local capture continues when the server is unavailable.
- Resumable uploads with server-controlled session size limits.
- Browser-based session inspection, downloads, and request-level analysis.

Description: Pine Server receives captured AI traffic from Pine clients
and stores it on a persistent server disk in the customer's AWS account. Clients
retain their original captures and upload eligible sessions using organization
keys. Administrators configure upload policy in config.yaml. The included web
interface supports browsing sessions and inspecting uploaded files. This release
is a single-node deployment; it does not provide enterprise SSO, granular user
roles, high availability, token billing estimates, or managed backup services.

Support URL, support contact, seller legal name, EULA, privacy policy, logo,
pricing, regions and supported instances: owner-supplied, not yet finalized.

## Engineering work required before submission

1. Build a pinned server image and AMI from clean source with no .env, customer
   keys, captures, uploader state, SSH authorized keys or build credentials.
2. Bake Docker and the server image into the AMI. Provide a first-run setup
   command that generates unique buyer keys, installs configuration and starts
   the server. Never ship a shared default credential.
3. Provide buyer instructions for DNS/TLS setup, restricted SSH, disk sizing,
   persistence, backup/restore, upgrades and stopping the instance. Keep port
   8080 private. Configure firewall ingress for 80/443 and restricted admin SSH.
4. Launch a clean AMI in a test account and verify first-run setup, HTTPS,
   authenticated upload, organization isolation, policy reload, restart,
   backup/restore and upgrade. Include macOS client onboarding (install script).
5. Review dependencies and image against current Marketplace AMI requirements,
   run vulnerability checks, remove build artifacts and credentials, and verify
   architecture, metadata service configuration and supported instance types.
6. Submit the AMI/version into a limited Marketplace listing, complete AWS scans
   and test buyer subscription and launch before requesting public availability.

The existing deploy/aws.sh creates a generic VM; it is not a prepared Marketplace
AMI or submission tool. Do not submit that base Ubuntu AMI as Pine.

## Account and publication steps

- Select seller account/profile and region; verify access with STS.
- Register the account as an AWS Marketplace seller. Paid offerings also require
  the applicable tax and banking onboarding.
- Use the seller portal's Server products flow to create an AMI product and
  choose its pricing/licensing model.
- Supply listing metadata, usage instructions, support details, approved license
  terms, and the tested AMI/version information.
- Complete AWS validation and limited buyer testing, then request publication.

Sources checked during preparation:
- https://docs.aws.amazon.com/marketplace/latest/userguide/ami-single-ami-products.html
- https://docs.aws.amazon.com/marketplace/latest/userguide/seller-account-registering.html
- https://docs.aws.amazon.com/marketplace/latest/userguide/product-and-ami-policies.html
- https://docs.aws.amazon.com/marketplace/latest/userguide/best-practices-for-building-your-amis.html
