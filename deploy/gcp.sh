#!/usr/bin/env bash
# Provision an Ubuntu VM; use existing network firewall rules for 80/443 + SSH.
set -euo pipefail
: "${GCP_PROJECT:?Set GCP_PROJECT}"
: "${GCP_ZONE:?Set GCP_ZONE}"
: "${GCP_SUBNET:?Set GCP_SUBNET}"
gcloud compute instances create "${INSTANCE_NAME:-pine}" \
  --project "$GCP_PROJECT" --zone "$GCP_ZONE" --subnet "$GCP_SUBNET" \
  --machine-type e2-small --image-family ubuntu-2404-lts-amd64 \
  --image-project ubuntu-os-cloud --boot-disk-size 40GB \
  --no-boot-disk-auto-delete --no-service-account --no-scopes --tags pine
