#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Fleet inventory is never committed. Locally it lives at deploy/gcp/fleet.conf;
# CI points FLEET_CONF_FILE at a runner-temp file materialized from a repository
# variable, so the same inventory can be injected without a plaintext file in the
# checkout.
FLEET_CONF_FILE="${FLEET_CONF_FILE:-${SCRIPT_DIR}/fleet.conf}"
if [ ! -f "${FLEET_CONF_FILE}" ]; then
  printf 'error: %s\n' "fleet inventory not found at ${FLEET_CONF_FILE}; copy deploy/gcp/fleet.conf.example to deploy/gcp/fleet.conf, or set FLEET_CONF_FILE" >&2
  exit 1
fi
# shellcheck disable=SC1090,SC1091
source "${FLEET_CONF_FILE}"

IMAGE_REF="$(<"${SCRIPT_DIR}/IMAGE")"
TEMPLATE="${SCRIPT_DIR}/startup-script.sh.tmpl"
DRY_RUN=false
MODE=""
ONLY=""
AUDIT_SCOPE=""

usage() {
  cat <<'USAGE'
Usage: seeders.sh MODE [OPTIONS]

Modes:
  --roll [--only NAME_OR_NS] [--dry-run]
  --create [--only NAME_OR_NS] [--dry-run]
  --status [--dry-run]
  --audit [--only NAME_OR_NS] [--dry-run]
  --dns [--dry-run]

Options:
  --only NAME_OR_NS  Limit VM actions to a VM name or ns label.
  --dry-run          Print gcloud commands instead of executing them.
  -h, --help         Show this help.
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "$1 not found on PATH"
}

print_cmd() {
  printf '[dry-run]'
  printf ' %q' "$@"
  printf '\n'
}

run_cmd() {
  if "${DRY_RUN}"; then
    print_cmd "$@"
    return 0
  fi
  "$@"
}

get_cmd() {
  if "${DRY_RUN}"; then
    print_cmd "$@" >&2
    return 0
  fi
  "$@" || true
}

image_digest() {
  [[ "${IMAGE_REF}" =~ @sha256:([0-9a-f]{64})$ ]] || die "deploy/gcp/IMAGE must pin a sha256 digest"
  printf '%s\n' "${BASH_REMATCH[1]}"
}

digest_short() {
  image_digest | cut -c1-12
}

selected() {
  [ -z "${ONLY}" ] || [ "${ONLY}" = "$1" ] || [ "${ONLY}" = "$2" ]
}

each_selected() {
  local callback="$1" count=0 row name zone region ns
  for row in "${SEEDERS[@]}"; do
    IFS='|' read -r name zone region ns <<< "${row}"
    selected "${name}" "${ns}" || continue
    "${callback}" "${name}" "${zone}" "${region}" "${ns}"
    count=$((count + 1))
  done
  [ "${count}" -gt 0 ] || die "no VM matched --only ${ONLY}"
}

render_startup_script() {
  local rendered
  rendered="$(mktemp)"
  awk -v image_ref="${IMAGE_REF}" -v nameserver="$1" \
    '{ gsub(/__IMAGE_REF__/, image_ref); gsub(/__NAMESERVER__/, nameserver); print }' \
    "${TEMPLATE}" > "${rendered}"
  printf '%s\n' "${rendered}"
}

addr() {
  get_cmd gcloud compute addresses describe "$1" \
    --project="${PROJECT}" --region="$2" --format='value(address)'
}

vm() {
  get_cmd gcloud compute instances describe "$1" \
    --project="${PROJECT}" --zone="$2" --format="$3"
}

metadata() {
  get_cmd gcloud compute instances describe "$1" \
    --project="${PROJECT}" --zone="$2" \
    --flatten='metadata.items[]' \
    --filter="metadata.items.key=$3" \
    --format='value(metadata.items.value)'
}

count_a() {
  local output
  output="$(dig +short +time=3 +tries=1 "$@" A 2>/dev/null || true)"
  grep -cE '^[0-9]' <<< "${output}" || true
}

dig_count() {
  local ip="$1" domain="$2" transport="${3:-udp}"
  if [ -z "${ip}" ] || "${DRY_RUN}"; then
    printf '%s\n' "-"
    return 0
  fi
  if [ "${transport}" = tcp ]; then
    count_a +tcp @"${ip}" "${domain}"
  else
    count_a @"${ip}" "${domain}"
  fi
}

delegation_count() {
  if "${DRY_RUN}"; then
    printf '%s\n' "-"
    return 0
  fi
  count_a "$1"
}

gate() {
  local ip="$1" try main_udp=0 main_tcp=0 test_udp=0 test_tcp=0
  if "${DRY_RUN}"; then
    printf 'dry-run: skipped dig gate for %s\n' "${ip:-reserved IP}"
    return 0
  fi
  for ((try = 1; try <= GATE_TRIES; try++)); do
    main_udp="$(dig_count "${ip}" "${MAINNET_DOMAIN}" udp)"
    main_tcp="$(dig_count "${ip}" "${MAINNET_DOMAIN}" tcp)"
    test_udp="$(dig_count "${ip}" "${TESTNET_DOMAIN}" udp)"
    test_tcp="$(dig_count "${ip}" "${TESTNET_DOMAIN}" tcp)"
    if [ "${main_udp}" -ge 1 ] && [ "${main_tcp}" -ge 1 ] && [ "${test_udp}" -ge 1 ] && [ "${test_tcp}" -ge 1 ]; then
      printf 'gate passed for %s: mainnet udp=%s tcp=%s, testnet udp=%s tcp=%s\n' "${ip}" "${main_udp}" "${main_tcp}" "${test_udp}" "${test_tcp}"
      return 0
    fi
    sleep "${GATE_SLEEP}"
  done
  # Mainnet is hard-gated: a node that is not serving mainnet over both UDP and TCP aborts the roll.
  if [ "${main_udp}" -lt 1 ] || [ "${main_tcp}" -lt 1 ]; then
    die "gate failed for ${ip}: mainnet udp=${main_udp} tcp=${main_tcp} (testnet udp=${test_udp} tcp=${test_tcp})"
  fi
  # Testnet is soft-gated: the NU6.3 protocol floor (#51) makes testnet servability
  # network-dependent, so a freshly reset crawler can sit at servable=0 for a while while
  # mainnet is healthy. Warn and continue instead of aborting a node that serves mainnet fine.
  warn "gate soft-pass for ${ip}: mainnet udp=${main_udp} tcp=${main_tcp} healthy; testnet udp=${test_udp} tcp=${test_tcp} not yet serving — continuing (testnet crawler may still be warming up)"
  return 0
}

verify_image() {
  if "${DRY_RUN}"; then
    printf 'dry-run: would cosign verify %s\n' "${IMAGE_REF}"
    return 0
  fi
  require_cmd cosign
  cosign verify "${IMAGE_REF}" \
    --certificate-identity-regexp '^https://github\.com/ZcashFoundation/zeeder/\.github/workflows/release\.yml@' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
}

firewall_ok() {
  local rule
  rule="$(get_cmd gcloud compute firewall-rules list \
    --project="${PROJECT}" \
    --filter="direction=INGRESS AND disabled=false AND sourceRanges:0.0.0.0/0 AND targetTags:${NETWORK_TAG} AND allowed[].IPProtocol=tcp AND allowed[].ports=53 AND allowed[].IPProtocol=udp AND allowed[].ports=53" \
    --format='value(name)' | head -n1 || true)"
  [ -n "${rule}" ] || "${DRY_RUN}"
}

audit_vm() {
  local name="$1" zone="$2" region="$3" ns="$4" want status ip reserved sa deletion label konlet
  # Semicolon-separated, not tab: `read` with an IFS-whitespace delimiter (tab)
  # collapses consecutive separators, so an empty middle field (e.g. a missing
  # NAT IP) would shift every later field. `;` never appears in these values.
  IFS=';' read -r status ip sa deletion label < <(vm "${name}" "${zone}" \
    "csv[no-heading,separator=';'](status, networkInterfaces[0].accessConfigs[0].natIP, serviceAccounts[0].email, deletionProtection, labels.zeeder-digest)") || true
  reserved="$(addr "${name}" "${region}")"
  if [ "${AUDIT_SCOPE}" = full ]; then
    want="$(digest_short)"
    konlet="$(metadata "${name}" "${zone}" gce-container-declaration)"
  fi

  "${DRY_RUN}" && return 0
  [ "${status}" = RUNNING ] || die "${name} (${ns}) is not RUNNING"
  [ -n "${reserved}" ] || die "${name} has no reserved regional address in ${region}"
  [ "${ip}" = "${reserved}" ] || die "${name} NAT IP ${ip:-missing} does not match reserved ${reserved}"
  [ "${sa}" = "${SERVICE_ACCOUNT}" ] || die "${name} service account ${sa:-missing} does not match ${SERVICE_ACCOUNT}"
  [ "${deletion}" = True ] || [ "${deletion}" = true ] || die "${name} deletion protection is not enabled"
  [ "${AUDIT_SCOPE}" = full ] || return 0
  [ -z "${konlet}" ] || die "${name} still has gce-container-declaration metadata"
  [ "${label}" = "${want}" ] || die "${name} zeeder-digest label ${label:-missing} does not match ${want}"
}

audit_all() {
  AUDIT_SCOPE="$1"
  each_selected audit_vm
  firewall_ok || die "no firewall rule allows udp+tcp :53 from 0.0.0.0/0 to tag ${NETWORK_TAG}"
}

roll_vm() {
  local name="$1" zone="$2" region="$3" ns="$4" rendered ip digest
  rendered="$(render_startup_script "${ns}.zfnd.org")"
  ip="$(addr "${name}" "${region}")"
  digest="$(digest_short)"
  printf 'rolling %s (%s.zfnd.org)\n' "${name}" "${ns}"
  run_cmd gcloud compute instances add-metadata "${name}" \
    --project="${PROJECT}" --zone="${zone}" \
    --metadata-from-file="startup-script=${rendered}"
  # Remove legacy konlet metadata when present.
  run_cmd gcloud compute instances remove-metadata "${name}" \
    --project="${PROJECT}" --zone="${zone}" \
    --keys=gce-container-declaration || true
  run_cmd gcloud compute instances add-labels "${name}" \
    --project="${PROJECT}" --zone="${zone}" \
    --labels="zeeder-digest=${digest}"
  run_cmd gcloud compute instances reset "${name}" \
    --project="${PROJECT}" --zone="${zone}"
  rm -f "${rendered}"
  gate "${ip}"
}

create_vm() {
  local name="$1" zone="$2" region="$3" ns="$4" status ip rendered
  status="$(vm "${name}" "${zone}" 'value(status)')"
  if [ -n "${status}" ] && ! "${DRY_RUN}"; then
    printf '%s already exists in %s\n' "${name}" "${zone}"
    return 0
  fi
  ip="$(addr "${name}" "${region}")"
  if [ -z "${ip}" ] && ! "${DRY_RUN}"; then
    die "missing static IP ${name} in ${region}; reserve it with: gcloud compute addresses create ${name} --project=${PROJECT} --region=${region} --network-tier=PREMIUM"
  fi
  rendered="$(render_startup_script "${ns}.zfnd.org")"
  run_cmd gcloud compute instances create "${name}" \
    --project="${PROJECT}" --zone="${zone}" --machine-type="${MACHINE_TYPE}" \
    --image-family="${IMAGE_FAMILY}" --image-project="${IMAGE_PROJECT}" \
    --network-interface="network-tier=PREMIUM,address=${ip:-<reserved-ip>},stack-type=IPV4_ONLY" \
    --metadata-from-file="startup-script=${rendered}" \
    --metadata=google-logging-enabled=true \
    --tags="${NETWORK_TAG}" \
    --service-account="${SERVICE_ACCOUNT}" \
    --scopes=https://www.googleapis.com/auth/cloud-platform \
    --boot-disk-type=pd-balanced --boot-disk-size="${DISK_SIZE}GB" \
    --deletion-protection \
    --maintenance-policy=MIGRATE --provisioning-model=STANDARD \
    --shielded-vtpm --shielded-integrity-monitoring \
    --labels="zeeder-digest=$(digest_short)"
  rm -f "${rendered}"
}

show_status() {
  local want row name zone region ns ip status label marker main_udp main_tcp test_udp test_tcp
  want="$(digest_short)"
  printf '%-24s %-15s %-10s %-26s %-12s %-12s\n' "name(ns)" "IP" "VM" "digest" "main u/t" "test u/t"
  for row in "${SEEDERS[@]}"; do
    IFS='|' read -r name zone region ns <<< "${row}"
    # Semicolon-separated, not tab: a tab (IFS-whitespace) delimiter drops empty
    # middle fields under `read`, misaligning the row when a VM has no NAT IP.
    IFS=';' read -r ip status label < <(vm "${name}" "${zone}" \
      "csv[no-heading,separator=';'](networkInterfaces[0].accessConfigs[0].natIP, status, labels.zeeder-digest)") || true
    [ "${label}" = "${want}" ] && marker=match || marker=miss
    main_udp="$(dig_count "${ip}" "${MAINNET_DOMAIN}" udp)"
    main_tcp="$(dig_count "${ip}" "${MAINNET_DOMAIN}" tcp)"
    test_udp="$(dig_count "${ip}" "${TESTNET_DOMAIN}" udp)"
    test_tcp="$(dig_count "${ip}" "${TESTNET_DOMAIN}" tcp)"
    printf '%-24s %-15s %-10s %-26s %-12s %-12s\n' "${name}(${ns})" "${ip:-N/A}" "${status:-N/A}" "${label:-none}/${want} ${marker}" "${main_udp}/${main_tcp}" "${test_udp}/${test_tcp}"
  done
  printf 'delegation: mainnet=%s testnet=%s\n' "$(delegation_count "${MAINNET_DOMAIN}")" "$(delegation_count "${TESTNET_DOMAIN}")"
}

print_dns() {
  local row name zone region ns ip
  for row in "${SEEDERS[@]}"; do
    IFS='|' read -r name zone region ns <<< "${row}"
    ip="$(addr "${name}" "${region}")"
    printf '%s.zfnd.org. IN A %s\n' "${ns}" "${ip:-<reserved-ip>}"
  done
  printf '; Delegate %s and %s to ns1.zfnd.org through ns6.zfnd.org.\n' "${MAINNET_DOMAIN}" "${TESTNET_DOMAIN}"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --roll | --create | --status | --audit | --dns)
        [ -z "${MODE}" ] || die "only one mode may be provided"
        MODE="${1#--}"; shift ;;
      --only)
        [ "$#" -ge 2 ] || die "--only requires a value"
        ONLY="$2"; shift 2 ;;
      --dry-run)
        DRY_RUN=true; shift ;;
      -h | --help)
        usage; exit 0 ;;
      *)
        die "unknown argument: $1" ;;
    esac
  done
  [ -n "${MODE}" ] || die "no mode provided"
}

main() {
  parse_args "$@"
  require_cmd gcloud
  require_cmd dig
  case "${MODE}" in
    roll)
      audit_all infra
      verify_image
      # Canary order is array order, with ns1 intentionally last.
      each_selected roll_vm ;;
    create) each_selected create_vm ;;
    status) show_status ;;
    audit) audit_all full ;;
    dns) print_dns ;;
    *) die "unsupported mode ${MODE}" ;;
  esac
}

main "$@"
