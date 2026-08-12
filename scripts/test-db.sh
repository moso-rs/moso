#!/usr/bin/env bash
#
# test-db.sh - manage the throwaway Postgres and Redis that moso's integration
# tests use.
#
#   ./scripts/test-db.sh up         # start both (idempotent) and wait until ready
#   ./scripts/test-db.sh down       # stop and delete both, data included
#   ./scripts/test-db.sh reset      # recreate the moso_test database, flush Redis
#   ./scripts/test-db.sh psql       # interactive shell; extra args are passed on
#   ./scripts/test-db.sh redis-cli  # interactive shell; extra args are passed on
#   ./scripts/test-db.sh url        # print DATABASE_URL and exit
#   ./scripts/test-db.sh redis-url  # print REDIS_URL and exit
#   ./scripts/test-db.sh env        # print both as `export` lines, for eval
#   ./scripts/test-db.sh status     # report reachability, version, privileges
#
# Then:
#   eval "$(./scripts/test-db.sh env)"
#   cargo test --workspace --all-features
#
# Both stores matter. Tests that need Postgres gate on DATABASE_URL and tests
# that need Redis gate on REDIS_URL, and both skip cleanly when their variable
# is unset — so the suite still passes on a machine without Docker. The cost of
# that design is that exporting only one of the two produces a green run which
# never exercised the other, which is why `env` prints both together and `up`
# starts both.
#
# This script only ever touches the two containers named below. It never
# enumerates, stops, or prunes anything else on the machine.

set -euo pipefail

readonly CONTAINER="moso-test-pg"
readonly IMAGE="postgres:17-alpine"
readonly HOST_PORT="55433"
readonly PG_USER="moso"
readonly PG_PASSWORD="moso"
readonly PG_DB="moso_test"
readonly DATABASE_URL="postgres://${PG_USER}:${PG_PASSWORD}@localhost:${HOST_PORT}/${PG_DB}"

readonly REDIS_CONTAINER="moso-test-redis"
readonly REDIS_IMAGE="redis:7-alpine"
readonly REDIS_HOST_PORT="56379"
readonly REDIS_URL="redis://localhost:${REDIS_HOST_PORT}"

readonly READY_TIMEOUT_SECS=60

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly COMPOSE_FILE="${REPO_ROOT}/compose.test.yaml"

die() {
  printf 'test-db: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'test-db: %s\n' "$*" >&2
}

require_docker() {
  command -v docker >/dev/null 2>&1 \
    || die "docker not found on PATH. Install Docker, or point DATABASE_URL and REDIS_URL at your own servers."
  docker info >/dev/null 2>&1 \
    || die "the docker daemon is not responding. Start Docker Desktop and retry."
}

# True when the compose plugin is usable, so `up` can go through
# compose.test.yaml (the declarative path CI uses).
have_compose() {
  [[ -f "${COMPOSE_FILE}" ]] && docker compose version >/dev/null 2>&1
}

# Prints the named container's state word ("running", "exited", ...) or nothing
# at all when no container by that name exists.
container_state() {
  docker inspect -f '{{.State.Status}}' "$1" 2>/dev/null || true
}

# Runs psql inside the container, so no host psql install is required. SQL comes
# from the caller's flags (-c / -tAc), never from stdin.
#
# Deliberately no `docker exec -i`: with -i, docker drains this process's stdin,
# which silently swallows the rest of a `while read` loop's input when pg is
# called from inside one (see cmd_reset). stdin is closed for the same reason.
pg() {
  docker exec -e PGPASSWORD="${PG_PASSWORD}" "${CONTAINER}" \
    psql -v ON_ERROR_STOP=1 -U "${PG_USER}" "$@" </dev/null
}

# The same shape for Redis: no host redis-cli required, stdin closed.
redis() {
  docker exec "${REDIS_CONTAINER}" redis-cli "$@" </dev/null
}

wait_pg_ready() {
  local waited=0
  while (( waited < READY_TIMEOUT_SECS )); do
    if docker exec "${CONTAINER}" pg_isready -U "${PG_USER}" -d "${PG_DB}" >/dev/null 2>&1; then
      # pg_isready can succeed during the init phase, while the entrypoint is
      # still about to restart the server. Require a real query to round-trip.
      if pg -d "${PG_DB}" -tAc 'select 1' >/dev/null 2>&1; then
        log "postgres ready after ${waited}s"
        return 0
      fi
    fi
    sleep 1
    waited=$(( waited + 1 ))
  done
  log "container logs (tail):"
  docker logs --tail 40 "${CONTAINER}" >&2 2>&1 || true
  die "Postgres was not ready within ${READY_TIMEOUT_SECS}s."
}

wait_redis_ready() {
  local waited=0
  while (( waited < READY_TIMEOUT_SECS )); do
    if [[ "$(redis ping 2>/dev/null || true)" == "PONG" ]]; then
      log "redis ready after ${waited}s"
      return 0
    fi
    sleep 1
    waited=$(( waited + 1 ))
  done
  log "container logs (tail):"
  docker logs --tail 40 "${REDIS_CONTAINER}" >&2 2>&1 || true
  die "Redis was not ready within ${READY_TIMEOUT_SECS}s."
}

# Brings one container up without compose. Used when the compose plugin is
# missing, and when only one of the pair is absent (compose would recreate both).
run_postgres() {
  docker run -d \
    --name "${CONTAINER}" \
    -p "${HOST_PORT}:5432" \
    -e POSTGRES_USER="${PG_USER}" \
    -e POSTGRES_PASSWORD="${PG_PASSWORD}" \
    -e POSTGRES_DB="${PG_DB}" \
    --tmpfs /var/lib/postgresql/data \
    "${IMAGE}" \
    postgres -c fsync=off -c synchronous_commit=off \
             -c full_page_writes=off -c max_connections=200 >/dev/null
}

run_redis() {
  docker run -d \
    --name "${REDIS_CONTAINER}" \
    -p "${REDIS_HOST_PORT}:6379" \
    "${REDIS_IMAGE}" \
    redis-server --save '' --appendonly no \
                 --maxmemory 256mb --maxmemory-policy allkeys-lru >/dev/null
}

# Starts one container, reusing it when it already exists. `$1` is the container
# name, `$2` the function that creates it from scratch.
start_one() {
  local name="$1" create="$2" state
  state="$(container_state "${name}")"

  case "${state}" in
    running)
      log "container '${name}' already running; reusing it"
      ;;
    exited|created|paused)
      log "container '${name}' exists (${state}); starting it"
      docker start "${name}" >/dev/null
      ;;
    "")
      log "creating '${name}'"
      "${create}"
      ;;
    *)
      die "container '${name}' is in unexpected state '${state}'. Run '$0 down' and retry."
      ;;
  esac
}

cmd_up() {
  require_docker

  # The compose path is the declarative one and creates both services in one
  # call, so prefer it when neither container exists yet. Once either exists,
  # `docker compose up` would still be correct but the per-container path gives
  # a clearer log and never recreates a container a developer is mid-debug on.
  if have_compose \
    && [[ -z "$(container_state "${CONTAINER}")" ]] \
    && [[ -z "$(container_state "${REDIS_CONTAINER}")" ]]; then
    log "creating both services via ${COMPOSE_FILE##*/}"
    docker compose -f "${COMPOSE_FILE}" up -d
  else
    start_one "${CONTAINER}" run_postgres
    start_one "${REDIS_CONTAINER}" run_redis
  fi

  wait_pg_ready
  wait_redis_ready
  cmd_env
}

cmd_down() {
  require_docker

  if have_compose; then
    # -v also removes any named volume an earlier revision of compose.test.yaml
    # used. Note that this exits 0 as a no-op when a container was created by
    # `docker run` rather than by compose, so success here proves nothing:
    # re-check each state and fall through to `docker rm`.
    docker compose -f "${COMPOSE_FILE}" down -v >/dev/null 2>&1 || true
  fi

  local removed=0 name
  for name in "${CONTAINER}" "${REDIS_CONTAINER}"; do
    if [[ -n "$(container_state "${name}")" ]]; then
      docker rm -f -v "${name}" >/dev/null
      log "removed '${name}'"
      removed=1
    fi
  done

  if (( removed == 0 )); then
    log "nothing to remove"
  fi
}

# Drops and recreates ${PG_DB}, plus any leftover per-test databases this
# harness creates (they are all prefixed moso_test_), and empties Redis. Faster
# than down+up and it keeps both connection URLs stable.
cmd_reset() {
  require_docker
  [[ "$(container_state "${CONTAINER}")" == "running" \
     && "$(container_state "${REDIS_CONTAINER}")" == "running" ]] || cmd_up >/dev/null

  local leftovers
  leftovers="$(pg -d postgres -tAc \
    "select datname from pg_database where datname like 'moso_test\\_%'")"

  if [[ -n "${leftovers}" ]]; then
    while IFS= read -r db; do
      [[ -n "${db}" ]] || continue
      log "dropping leftover database ${db}"
      pg -d postgres -c "drop database if exists \"${db}\" with (force)" >/dev/null
    done <<< "${leftovers}"
  fi

  log "recreating database ${PG_DB}"
  pg -d postgres -c "drop database if exists \"${PG_DB}\" with (force)" >/dev/null
  pg -d postgres -c "create database \"${PG_DB}\" owner \"${PG_USER}\"" >/dev/null

  # FLUSHALL, not FLUSHDB: the KV conformance suite and the jobs queue both use
  # numbered databases, and clearing only db 0 leaves the others behind.
  log "flushing redis"
  redis flushall >/dev/null

  log "reset complete"
  cmd_env
}

cmd_psql() {
  require_docker
  [[ "$(container_state "${CONTAINER}")" == "running" ]] \
    || die "container '${CONTAINER}' is not running. Run '$0 up' first."

  if [[ $# -gt 0 ]]; then
    docker exec -e PGPASSWORD="${PG_PASSWORD}" -i "${CONTAINER}" \
      psql -U "${PG_USER}" -d "${PG_DB}" "$@"
  else
    # -t gives an interactive terminal; only valid without piped SQL.
    docker exec -e PGPASSWORD="${PG_PASSWORD}" -it "${CONTAINER}" \
      psql -U "${PG_USER}" -d "${PG_DB}"
  fi
}

cmd_redis_cli() {
  require_docker
  [[ "$(container_state "${REDIS_CONTAINER}")" == "running" ]] \
    || die "container '${REDIS_CONTAINER}' is not running. Run '$0 up' first."

  if [[ $# -gt 0 ]]; then
    docker exec -i "${REDIS_CONTAINER}" redis-cli "$@"
  else
    docker exec -it "${REDIS_CONTAINER}" redis-cli
  fi
}

cmd_url() {
  printf '%s\n' "${DATABASE_URL}"
}

cmd_redis_url() {
  printf '%s\n' "${REDIS_URL}"
}

# Both variables in one `eval`-able block. Exporting only one of the two is the
# mistake this subcommand exists to make hard: the other suite would skip and
# still report success.
cmd_env() {
  printf 'export DATABASE_URL=%s\n' "${DATABASE_URL}"
  printf 'export REDIS_URL=%s\n' "${REDIS_URL}"
}

cmd_status() {
  require_docker

  local pg_state redis_state failed=0
  pg_state="$(container_state "${CONTAINER}")"
  redis_state="$(container_state "${REDIS_CONTAINER}")"

  if [[ "${pg_state}" == "running" ]]; then
    printf 'postgres:      running\n'
    printf '  url:         %s\n' "${DATABASE_URL}"
    printf '  version:     %s\n' "$(pg -d "${PG_DB}" -tAc 'select version()')"
    printf '  createdb:    %s\n' \
      "$(pg -d "${PG_DB}" -tAc "select rolcreatedb from pg_roles where rolname = '${PG_USER}'")"
  else
    printf 'postgres:      %s  (run "%s up")\n' "${pg_state:-absent}" "$0"
    failed=1
  fi

  if [[ "${redis_state}" == "running" ]]; then
    printf 'redis:         running\n'
    printf '  url:         %s\n' "${REDIS_URL}"
    printf '  version:     %s\n' "$(redis --no-raw info server | tr -d '\r' | sed -n 's/^redis_version://p')"
    printf '  keys:        %s\n' "$(redis dbsize | tr -d '\r')"
  else
    printf 'redis:         %s  (run "%s up")\n' "${redis_state:-absent}" "$0"
    failed=1
  fi

  return "${failed}"
}

usage() {
  cat >&2 <<EOF
usage: $0 <up|down|reset|psql|redis-cli|url|redis-url|env|status> [args...]

  up         start ${CONTAINER} and ${REDIS_CONTAINER} (idempotent), wait, print both URLs
  down       stop and remove both containers and their data
  reset      recreate ${PG_DB} and any leftover moso_test_* databases, and flush Redis
  psql       open psql against ${PG_DB}; extra args are forwarded to psql
  redis-cli  open redis-cli against ${REDIS_CONTAINER}; extra args are forwarded
  url        print the DATABASE_URL tests expect
  redis-url  print the REDIS_URL tests expect
  env        print both as \`export\` lines: eval "\$($0 env)"
  status     print reachability, versions and CREATE DATABASE privilege

DATABASE_URL=${DATABASE_URL}
REDIS_URL=${REDIS_URL}
EOF
}

main() {
  local subcommand="${1:-}"
  [[ $# -gt 0 ]] && shift

  case "${subcommand}" in
    up)        cmd_up "$@" ;;
    down)      cmd_down "$@" ;;
    reset)     cmd_reset "$@" ;;
    psql)      cmd_psql "$@" ;;
    redis-cli) cmd_redis_cli "$@" ;;
    url)       cmd_url "$@" ;;
    redis-url) cmd_redis_url "$@" ;;
    env)       cmd_env "$@" ;;
    status)    cmd_status "$@" ;;
    -h|--help|help) usage ;;
    "")        usage; exit 2 ;;
    *)         printf 'test-db: unknown subcommand %q\n\n' "${subcommand}" >&2; usage; exit 2 ;;
  esac
}

main "$@"
