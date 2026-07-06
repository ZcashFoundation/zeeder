# GCP deployment (Zcash Foundation seeder fleet)

Reference deployment of zeeder on Google Cloud COS container VMs. This is how the
Zcash Foundation runs the six seeders authoritative for `*.seeder.zfnd.org`; it
also serves as a worked example for running zeeder on GCP. For the generic
deployment models (Docker, systemd, DNS, firewall, monitoring), see
[../../docs/operations.md](../../docs/operations.md).

## Fleet

Six `e2-micro` COS VMs in project `ecosystem-infrastructure`, one per region,
each with a pinned static IP published as `ns{1..6}.zfnd.org`. One zeeder process
per VM serves both mainnet and testnet on a single listener.

## Why the container listens on :1053, not :53

The image runs as a non-root user (`USER 65532`), which cannot bind privileged
port 53. Each VM's startup script redirects `:53 -> :1053` with iptables (udp and
tcp), re-applied on every boot. Running the container privileged does **not** let
a non-root process bind :53, so the redirect is required.

## Usage

```bash
# Preview the rolling update without touching anything:
./deploy-seeders.sh --dry-run --roll

# Roll a new image onto all six, one at a time, gated on `dig`:
# Pin by digest in production (reproducible + cosign-verifiable):
IMAGE=docker.io/zfnd/dnsseeder@sha256:<digest> ./deploy-seeders.sh --roll

# Canary a single VM first:
./deploy-seeders.sh --roll --only ns2

# Fleet health and the delegation records:
./deploy-seeders.sh --status
./deploy-seeders.sh --dns
```

`--roll` is idempotent: re-running converges every VM to the declared container
spec (image, env, startup script), so it doubles as drift repair.

## The image

`docker.io/zfnd/dnsseeder` is built and published by
[`.github/workflows/release.yml`](../../.github/workflows/release.yml) on each
GitHub release: multi-arch, semver-tagged, with SLSA build provenance, an SBOM,
and a cosign signature. Resolve and verify the digest before a production roll:

```bash
# Resolve the digest of a tag:
docker buildx imagetools inspect docker.io/zfnd/dnsseeder:v1.4.0 --format '{{.Manifest.Digest}}'

# Verify the cosign signature (keyless, signed by the release workflow's OIDC identity):
cosign verify docker.io/zfnd/dnsseeder@sha256:<digest> \
  --certificate-identity-regexp '^https://github\.com/ZcashFoundation/zeeder/\.github/workflows/release\.yml@' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com'

# Or verify the provenance attestation via the GitHub attestation store:
gh attestation verify "oci://docker.io/zfnd/dnsseeder@sha256:<digest>" \
  --repo ZcashFoundation/zeeder \
  --signer-workflow ZcashFoundation/zeeder/.github/workflows/release.yml
```

An internal Artifact Registry image with keyless Workload Identity Federation is a
planned follow-up; until then the fleet pulls the public Docker Hub image.

## Gotcha: clear the inherited container command

`gcloud ... update-container` preserves the previous container's command/args.
VMs migrated from the old CoreDNS/dnsseeder ran `coredns -conf ...`; the script
passes `--clear-container-command --clear-container-args` so zeeder is not invoked
as `coredns` (which crash-loops with `exec: coredns: not found`).
