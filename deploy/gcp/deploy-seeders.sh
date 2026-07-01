#!/bin/bash
#
# Deploy and roll the zeeder DNS seeder fleet on Google Cloud (COS container VMs).
#
# This is the Zcash Foundation's production deployment of zeeder: six e2-micro
# COS VMs (`zfnd-seeder-*` in project `ecosystem-infrastructure`) authoritative
# for `*.seeder.zfnd.org`. It is also a worked reference for anyone running
# zeeder on GCP container VMs. For the generic deployment models (Docker,
# systemd, DNS, firewall, monitoring) see ../../docs/operations.md.
#
# Modes
# -----
#   ./deploy-seeders.sh --roll              # roll the current image onto all six (1-at-a-time, gated)
#   ./deploy-seeders.sh --roll --only NAME  # roll a single VM (canary)
#   ./deploy-seeders.sh --create            # create any missing VMs (provision)
#   ./deploy-seeders.sh --status            # dig every NS, both zones
#   ./deploy-seeders.sh --dns               # print the A/NS records for the parent zone
#   ./deploy-seeders.sh --dry-run --roll    # preview without executing (works with any mode)
#
# GCP / COS deployment facts (hard-won — do not drop any of these):
#   1. zeeder runs NON-ROOT (USER 65532) and cannot bind :53. It listens on
#      :1053 and the host redirects :53 -> :1053 via iptables in the startup
#      script (re-applied every boot). `--container-privileged` does NOT help a
#      non-root process bind a privileged port.
#   2. `gcloud ... update-container` PRESERVES the prior container command/args.
#      VMs migrated from the old CoreDNS/dnsseeder ran `coredns -conf ...`;
#      without --clear-container-command / --clear-container-args, zeeder is
#      invoked as `coredns` and crash-loops ("exec: coredns: not found"). Always
#      clear them.
#   3. After update-container we `reset` the VM so the startup-script re-runs
#      (re-applying the iptables redirect) and the container relaunches.
#   4. Roll ONE VM at a time, gating on `dig` between each, so the other five NS
#      keep answering. Roll the oldest VM (zfnd-seeder-6d3a819 / ns1) LAST.
#
# Prerequisites: gcloud (authenticated, Compute Admin on $PROJECT), dig.
#
set -euo pipefail

# =============================================================================
# Configuration
# =============================================================================

PROJECT="${PROJECT:-ecosystem-infrastructure}"
MACHINE_TYPE="${MACHINE_TYPE:-e2-micro}"
DISK_SIZE="${DISK_SIZE:-10}"
IMAGE_FAMILY="cos-stable"
IMAGE_PROJECT="cos-cloud"
NETWORK_TAG="seeder"
SERVICE_ACCOUNT="instance-service-account@ecosystem-infrastructure.iam.gserviceaccount.com"

# Container image. Temporary: the zeeder build published to Docker Hub as
# zfnd/dnsseeder:v1.3.0 (that image IS zeeder). Override with IMAGE=... once the
# Artifact Registry pipeline exists — prefer pinning by digest (…/zeeder@sha256:…).
CONTAINER_IMAGE="${IMAGE:-docker.io/zfnd/dnsseeder:v1.3.0}"

# zeeder zone/listener/limits. Mirror the values validated in production.
ZEEDER_LISTEN="0.0.0.0:1053"
MAINNET_DOMAIN="mainnet.seeder.zfnd.org"
TESTNET_DOMAIN="testnet.seeder.zfnd.org"
MAINNET_TTL="600"
TESTNET_TTL="300"
METRICS_ADDR="127.0.0.1:9999"   # loopback: internal only, never public
HEALTH_ADDR="127.0.0.1:8080"    # loopback: /ready and /health
RATE_QPS="50"
RATE_BURST="100"

# VM definitions: name|zone|region|ns
# Rolling order matters: canary first, oldest VM (ns1) LAST.
declare -a SEEDERS=(
    "zfnd-seeder-2|us-east1-b|us-east1|ns2"
    "zfnd-seeder-3|us-west1-a|us-west1|ns3"
    "zfnd-seeder-4|europe-west1-b|europe-west1|ns4"
    "zfnd-seeder-5|europe-west3-a|europe-west3|ns5"
    "zfnd-seeder-6|europe-north1-a|europe-north1|ns6"
    "zfnd-seeder-6d3a819|us-central1-a|us-central1|ns1"
)

# Gate: how long to wait for a rolled VM to start serving before moving on.
GATE_TRIES="${GATE_TRIES:-15}"
GATE_SLEEP="${GATE_SLEEP:-12}"

# =============================================================================
# Host startup script (prepares the COS host for the non-root zeeder container)
# =============================================================================

read -r -d '' STARTUP_SCRIPT << 'STARTUP_EOF' || true
#!/bin/bash
systemctl stop systemd-resolved
echo "nameserver 8.8.8.8" > /etc/resolv.conf
echo "nameserver 8.8.4.4" >> /etc/resolv.conf
# zeeder runs non-root and binds :1053; redirect privileged :53 -> :1053 (udp+tcp)
iptables -t nat -A PREROUTING -p udp --dport 53 -j REDIRECT --to-ports 1053
iptables -t nat -A PREROUTING -p tcp --dport 53 -j REDIRECT --to-ports 1053
STARTUP_EOF

# =============================================================================
# Helpers
# =============================================================================

DRY_RUN=false
MODE=""
ONLY=""

log_info()  { echo -e "\033[0;32m[INFO]\033[0m $*"; }
log_error() { echo -e "\033[0;31m[ERROR]\033[0m $*"; }

run_cmd() {
    if [ "$DRY_RUN" = true ]; then
        echo -e "\033[0;36m[DRY-RUN]\033[0m $*"
    else
        "$@"
    fi
}

# zeeder container-env list for a given nameserver (NAMESERVER must be out-of-zone).
zeeder_env() {
    local ns="$1"
    cat <<ENV | paste -sd, -
ZEEDER__DNS__LISTEN_ADDR=${ZEEDER_LISTEN}
ZEEDER__ZONES__MAINNET__DOMAIN=${MAINNET_DOMAIN}
ZEEDER__ZONES__MAINNET__NAMESERVER=${ns}.zfnd.org
ZEEDER__ZONES__MAINNET__TTL=${MAINNET_TTL}
ZEEDER__ZONES__TESTNET__DOMAIN=${TESTNET_DOMAIN}
ZEEDER__ZONES__TESTNET__NAMESERVER=${ns}.zfnd.org
ZEEDER__ZONES__TESTNET__TTL=${TESTNET_TTL}
ZEEDER__METRICS__ENDPOINT_ADDR=${METRICS_ADDR}
ZEEDER__HEALTH__ENDPOINT_ADDR=${HEALTH_ADDR}
ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND=${RATE_QPS}
ZEEDER__RATE_LIMIT__BURST_SIZE=${RATE_BURST}
ENV
}

# One describe returns both fields: prints "IP<TAB>STATUS" (empty if the VM is absent).
vm_ip_status() {
    gcloud compute instances describe "$1" --project="$PROJECT" --zone="$2" \
        --format='value(networkInterfaces[0].accessConfigs[0].natIP,status)' 2>/dev/null
}

# Block until a VM serves both zones over udp+tcp, or the gate times out.
gate_serving() {
    local ip="$1" n=0 m mt t
    [ "$DRY_RUN" = true ] && { log_info "  (dry-run) would gate on dig @$ip"; return 0; }
    while [ "$n" -lt "$GATE_TRIES" ]; do
        m=$(dig +short +time=3 +tries=1 @"$ip" "$MAINNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
        mt=$(dig +short +tcp +time=3 +tries=1 @"$ip" "$MAINNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
        t=$(dig +short +time=3 +tries=1 @"$ip" "$TESTNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
        if [ "$m" -ge 1 ] && [ "$mt" -ge 1 ] && [ "$t" -ge 1 ]; then
            log_info "  ✓ serving (mainnet udp=$m tcp=$mt | testnet udp=$t)"
            return 0
        fi
        n=$((n+1)); sleep "$GATE_SLEEP"
    done
    log_error "  ✗ did not serve within $((GATE_TRIES*GATE_SLEEP))s"
    return 1
}

# =============================================================================
# Roll (update-container swap to CONTAINER_IMAGE) — the day-2 deploy operation
# =============================================================================

roll_one() {
    local name="$1" zone="$2" ns="$3"
    local env; env="$(zeeder_env "$ns")"

    # Fetch IP + status once up front; the static IP is stable across the reset.
    local ip st; IFS=$'\t' read -r ip st < <(vm_ip_status "$name" "$zone")
    if [ -z "$st" ]; then
        log_error "$name not found in $zone — run --create first"; return 1
    fi

    log_info "Rolling $name ($zone, $ns) -> $CONTAINER_IMAGE"
    local startup_file; startup_file=$(mktemp); echo "$STARTUP_SCRIPT" > "$startup_file"

    # 1) ensure the iptables-redirect startup-script is in place (re-runs on reset)
    run_cmd gcloud compute instances add-metadata "$name" --project="$PROJECT" --zone="$zone" \
        --metadata-from-file="startup-script=$startup_file"
    rm -f "$startup_file"

    # 2) swap the image + zeeder env; clear the inherited coredns command/args;
    #    non-privileged (non-root binds :1053, not :53).
    run_cmd gcloud compute instances update-container "$name" --project="$PROJECT" --zone="$zone" \
        --container-image="$CONTAINER_IMAGE" \
        --container-env="$env" \
        --clear-container-command --clear-container-args \
        --no-container-privileged

    # 3) reset so the startup-script re-applies iptables and the container relaunches
    run_cmd gcloud compute instances reset "$name" --project="$PROJECT" --zone="$zone"

    # 4) gate on serving before the caller moves to the next VM
    gate_serving "$ip"
}

roll_all() {
    log_info "Rolling ${CONTAINER_IMAGE} onto the fleet (1-at-a-time, gated)"
    local rolled=0
    for seeder in "${SEEDERS[@]}"; do
        IFS='|' read -r name zone _ ns <<< "$seeder"
        [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && [ "$ONLY" != "$ns" ] && continue
        echo "----------------------------------------------------------------------"
        roll_one "$name" "$zone" "$ns" || { log_error "halting roll at $name"; return 1; }
        rolled=$((rolled+1))
    done
    [ "$rolled" -eq 0 ] && { log_error "no VM matched --only '$ONLY'"; return 1; }
    log_info "Rolled $rolled VM(s)."
}

# =============================================================================
# Create (provision a missing VM with the zeeder container)
# =============================================================================

create_vm() {
    local name="$1" zone="$2" region="$3" ns="$4"
    local st; IFS=$'\t' read -r _ st < <(vm_ip_status "$name" "$zone")
    if [ -n "$st" ]; then
        log_info "VM '$name' already exists in $zone (use --roll to update it)"; return 0
    fi
    local ip_address; ip_address=$(gcloud compute addresses describe "$name" \
        --project="$PROJECT" --region="$region" --format='value(address)' 2>/dev/null || echo "")
    if [ -z "$ip_address" ]; then
        log_error "Static IP '$name' not found in $region. Reserve it first:"
        log_error "  gcloud compute addresses create $name --project=$PROJECT --region=$region --network-tier=PREMIUM"
        return 1
    fi
    log_info "Creating VM '$name' in $zone ($ns) with IP $ip_address..."
    # create-with-container builds the container declaration natively from
    # --container-env (the same zeeder_env used by --roll), so there is no
    # hand-rolled YAML to keep in sync.
    local startup_file; startup_file=$(mktemp); echo "$STARTUP_SCRIPT" > "$startup_file"
    run_cmd gcloud compute instances create-with-container "$name" \
        --project="$PROJECT" --zone="$zone" --machine-type="$MACHINE_TYPE" \
        --network-interface="network-tier=PREMIUM,address=$ip_address,stack-type=IPV4_ONLY" \
        --container-image="$CONTAINER_IMAGE" \
        --container-env="$(zeeder_env "$ns")" \
        --container-restart-policy=always \
        --no-container-privileged \
        --metadata-from-file="startup-script=$startup_file" \
        --metadata="google-logging-enabled=true" \
        --maintenance-policy=MIGRATE --provisioning-model=STANDARD \
        --service-account="$SERVICE_ACCOUNT" \
        --scopes="https://www.googleapis.com/auth/cloud-platform" \
        --tags="$NETWORK_TAG" \
        --create-disk="auto-delete=yes,boot=yes,device-name=$name,image-family=$IMAGE_FAMILY,image-project=$IMAGE_PROJECT,mode=rw,size=$DISK_SIZE,type=pd-balanced" \
        --labels="container-vm=$name" \
        --deletion-protection \
        --no-shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring
    rm -f "$startup_file"
}

create_all() {
    for seeder in "${SEEDERS[@]}"; do
        IFS='|' read -r name zone region ns <<< "$seeder"
        [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && [ "$ONLY" != "$ns" ] && continue
        create_vm "$name" "$zone" "$region" "$ns" || true
    done
}

# =============================================================================
# Status / DNS
# =============================================================================

show_status() {
    printf "\n%-22s %-16s %-9s %-10s %-10s\n" "VM (ns)" "IP" "STATUS" "main(u/t)" "test(u)"
    printf "%-22s %-16s %-9s %-10s %-10s\n" "-------" "--" "------" "---------" "-------"
    for seeder in "${SEEDERS[@]}"; do
        IFS='|' read -r name zone _ ns <<< "$seeder"
        local ip st m mt t
        IFS=$'\t' read -r ip st < <(vm_ip_status "$name" "$zone")
        [ -z "$st" ] && st="NOT_FOUND"
        if [ -n "$ip" ]; then
            m=$(dig +short +time=3 +tries=1 @"$ip" "$MAINNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
            mt=$(dig +short +tcp +time=3 +tries=1 @"$ip" "$MAINNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
            t=$(dig +short +time=3 +tries=1 @"$ip" "$TESTNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')
        else m="-"; mt="-"; t="-"; fi
        printf "%-22s %-16s %-9s %-10s %-10s\n" "$name ($ns)" "${ip:-N/A}" "$st" "$m/$mt" "$t"
    done
    echo
    echo "End-to-end (delegation): mainnet=$(dig +short "$MAINNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]') testnet=$(dig +short "$TESTNET_DOMAIN" A 2>/dev/null | grep -cE '^[0-9]')"
}

output_dns_config() {
    echo "; A records (parent zone, e.g. Cloudflare zfnd.org). Static IPs survive recreation."
    for seeder in "${SEEDERS[@]}"; do
        IFS='|' read -r name zone _ ns <<< "$seeder"
        local ip; IFS=$'\t' read -r ip _ < <(vm_ip_status "$name" "$zone")
        echo "${ns}.zfnd.org.    IN    A    ${ip}"
    done
    echo "; NS delegation: {mainnet,testnet}.seeder.zfnd.org -> ns1..6.zfnd.org"
}

# =============================================================================
# Main
# =============================================================================

usage() {
    cat >&2 <<'USAGE'
Deploy and roll the zeeder DNS seeder fleet on Google Cloud.

  --roll [--only NAME]    roll CONTAINER_IMAGE onto all six VMs (1-at-a-time, gated)
  --create [--only NAME]  create any missing VMs (provision)
  --status                dig every NS, both zones
  --dns                   print the A/NS records for the parent zone
  --dry-run               preview without executing (combine with any mode)

Env overrides: IMAGE, PROJECT, GATE_TRIES, GATE_SLEEP.
USAGE
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --roll)    MODE="roll"; shift ;;
        --create)  MODE="create"; shift ;;
        --status)  MODE="status"; shift ;;
        --dns)     MODE="dns"; shift ;;
        --only)    ONLY="$2"; shift 2 ;;
        --dry-run) DRY_RUN=true; shift ;;
        --help|-h) usage 0 ;;
        *) log_error "Unknown option: $1"; usage 1 ;;
    esac
done

command -v gcloud >/dev/null || { log_error "gcloud not found"; exit 1; }
command -v dig >/dev/null || { log_error "dig not found"; exit 1; }

case "$MODE" in
    roll)   roll_all ;;
    create) create_all; show_status ;;
    status) show_status ;;
    dns)    output_dns_config ;;
    *)      log_error "No mode given."; usage 1 ;;
esac
