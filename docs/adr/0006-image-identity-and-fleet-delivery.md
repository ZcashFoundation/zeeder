# ADR 0006: Image Identity and Fleet Delivery

## Status

Accepted

## Context

The Zcash Foundation runs Zeeder as six authoritative nameservers,
`ns1.zfnd.org` through `ns6.zfnd.org`, in the `ecosystem-infrastructure` GCP
project. Each is a Container-Optimized OS VM in its own region. Those hosts back
the `mainnet.seeder.zfnd.org` and `testnet.seeder.zfnd.org` delegations, so a
botched roll can stop peer discovery for a whole network.

This fleet is Zeeder's only production consumer, and it deploys by pinned image
digest. It inherited GCP VMs that previously ran CoreDNS through the
`gce-container-declaration` metadata mechanism. Google is retiring that
mechanism: workflows that use it stop working on 2026-07-31, and support ends on
2027-07-31. Startup scripts and cloud-init are unaffected.

We need a stable image identity for the fleet to pin, a delivery mechanism that
rolls nameservers without dropping a delegation, and a host substrate that
survives the container-declaration retirement.

## Decision

### Image identity

Zeeder publishes one container image, `docker.io/zfnd/dnsseeder`, the name
inherited from the predecessor seeder. Only `.github/workflows/release.yml`
publishes it, and only when a GitHub release is published. Each image is
Cosign-signed keyless and carries SLSA build provenance and an SBOM.

There is no edge or per-commit channel. The only production consumer is the
first-party fleet, which deploys by pinned digest. Restricting publication to
releases keeps `id-token` and signing permissions off every non-release run and
gives a one-to-one release-to-artifact audit trail.

### Fleet delivery

The fleet is managed by policy files in git plus one imperative mechanism
script, all under `deploy/gcp`:

- `fleet.conf` is the inventory. Its array order is the rolling order: the canary
  is first and `ns1` is last.
- `IMAGE` is a single line holding the full image reference pinned by `sha256`
  digest. Bumping it in a pull request is the deploy event, and its git log is
  the deploy audit trail.
- `startup-script.sh.tmpl` is the per-boot host script. It stops
  `systemd-resolved`, applies an iptables `REDIRECT` from `:53` to `:1053` for
  UDP and TCP, and `docker run`s the pinned digest.
- `seeders.sh` is mechanism only: `roll`, `create`, `status`, `audit`, and
  `dns`, each with a dry-run.

Rolls proceed one VM at a time. Each step is dig-gated on both zones and aborts
on the first failure, so a bad image stops at the canary while the other five
nameservers keep answering. `cosign verify` against the release workflow's OIDC
identity gates every roll and fails closed, so only a release-signed digest ever
reaches production.

Desired state is git; actual state is the GCP API. `seeders.sh --audit` reports
the drift between them. Rollback is a `git revert` of `IMAGE` followed by a roll.

### Host substrate

The fleet runs a full `docker run` from the startup script. The
`gce-container-declaration` metadata mechanism is prohibited, along with
`gcloud compute instances update-container` and `create-with-container`.

The startup script re-applies iptables on every boot, so the `:53` redirect
survives reboots without manual repair. It also removes the
inherited-container-command failure class: the fleet's earlier CoreDNS
containers left command arguments that `update-container` preserved, which
crash-looped any replacement image. A full `docker run` in the startup script
inherits nothing.

## Rationale

- Release-only publication scopes signing and `id-token` permissions to the one
  workflow that needs them, and every production digest maps to exactly one
  published release.
- Splitting policy from mechanism makes a deploy a reviewable pull request whose
  diff is a single digest and whose revert is a one-line rollback, while
  `seeders.sh` stays free of desired state.
- DNS serves UDP, so the roll gate digs both the UDP and TCP paths on both zones.
  A gate that watched only TCP would miss the path clients actually use.
- Fail-closed Cosign verification means an unsigned or mismatched digest cannot
  roll, even by operator error, so the signature is load-bearing at deploy time
  rather than advisory.
- The startup-script substrate outlives the container-declaration retirement,
  re-establishes the `:53` redirect on every boot, and carries no inherited
  container command, which is what crash-looped the previous nameservers.

## Consequences

- A bad image halts at the canary; the remaining five nameservers keep answering
  throughout a roll, so the delegation stays live.
- Non-release CI runs never hold signing or `id-token` permissions.
- Every deploy is auditable from the `IMAGE` git history, and rollback needs no
  tooling beyond git and a roll.
- The fleet depends on Docker Hub reachability at boot until the Artifact
  Registry remote lands (see Deferred).
- Reboots re-apply the `:53` redirect automatically, so no host carries manual
  iptables state.
- The container-declaration retirement does not affect this fleet; its 2026-07-31 cutoff applies only to hosts still using that mechanism.

## Deferred

- An Artifact Registry remote repository, a pull-through cache of `docker.io`, as
  the fleet's pull path. It closes the Docker-Hub-outage-at-boot gap. Digests are
  content-addressed, so release signatures stay valid through the cache.
- A scheduled read-only audit workflow. It needs a read-only Workload Identity
  Federation identity first.
- A version-bearing health endpoint, so the roll gate can assert the running
  image reference rather than infer it.
- A `workflow_dispatch` deploy wrapper, added only when a second regular operator
  needs push-button deploys.
- Renaming the image to `zfnd/zeeder` and retiring `zfnd/dnsseeder`, tracked in
  issue #55. Docker Hub cannot alias names, so the cutover is deliberate rather
  than incidental.

## Alternatives Considered

- Terraform or OpenTofu for this fleet: rejected. Terraform has no health-gated,
  one-VM-at-a-time rolling primitive for standalone instances, so the roll stays
  an external script under any design. A state backend and a public-repo CI
  identity able to reset production nameservers add standing risk. The
  organization's live compute automation is gcloud-based, while its one Terraform
  compute repository is abandoned and its Terraform succeeds only for Cloudflare
  DNS records. Revisit if the fleet grows past roughly ten hosts, a second
  resource type appears, or a second regular operator needs push-button deploys.
- Managed instance groups: rejected. One regional MIG cannot span the six
  regions. Six size-one zonal MIGs buy only TCP/53 autohealing, which cannot
  observe the UDP path DNS serves, at triple the resource count.
