#!/usr/bin/env bash
set -Eeuo pipefail

CROISSANT_REPOSITORY="https://github.com/pablof7z/croissant.git"
CROISSANT_COMMIT="9c4c93e84852bd9aa6824060b74c56ab2ce812c2"
GO_IMAGE="golang:1.25-bookworm"
HARNESS_BACKEND="${NMP_NIP29_HARNESS_BACKEND:-docker}"
DEFAULT_PORT_A=19888
DEFAULT_PORT_B=19889
MARKER_NAME=".nmp-nip29-consumer-harness"

die() {
    printf 'nip29-consumer-harness: %s\n' "$*" >&2
    exit 1
}

note() {
    printf 'nip29-consumer-harness: %s\n' "$*"
}

usage() {
    cat <<'USAGE'
usage:
  harness.sh start [--port-a PORT] [--port-b PORT] RUN_DIR
  harness.sh status RUN_DIR
  harness.sh metadata-conflict RUN_DIR
  harness.sh chat-append RUN_DIR
  harness.sh follow-remove RUN_DIR
  harness.sh follow-add RUN_DIR
  harness.sh relay-down a|b RUN_DIR
  harness.sh relay-up a|b RUN_DIR
  harness.sh stop RUN_DIR

Croissant and nak are fixture infrastructure. Only NMP consumer output counts
as product evidence.
USAGE
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

require_common_commands() {
    require_command curl
    require_command git
    require_command jq
    require_command nak
    if ! command -v sha256sum >/dev/null 2>&1; then
        require_command shasum
    fi
    if ! command -v timeout >/dev/null 2>&1; then
        require_command perl
    fi
}

run_with_timeout() {
    local seconds=$1
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$seconds" "$@"
    else
        perl -e 'alarm shift; exec @ARGV' "$seconds" "$@"
    fi
}

short_hash() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -c1-12
    else
        shasum -a 256 | cut -c1-12
    fi
}

validate_port() {
    local value=$1
    [[ "$value" =~ ^[0-9]+$ ]] || die "port must be numeric: $value"
    ((value >= 1024 && value <= 65535)) || die "port is outside 1024..65535: $value"
}

canonical_new_run_dir() {
    local requested=$1
    [[ -n "$requested" ]] || die "RUN_DIR is required"
    [[ "$requested" == /* ]] || die "RUN_DIR must be an absolute path"
    [[ "$requested" != "/" ]] || die "RUN_DIR cannot be /"
    [[ ! -e "$requested" ]] || die "new RUN_DIR already exists: $requested"
    printf '%s\n' "$requested"
}

canonical_existing_run_dir() {
    local requested=$1
    [[ -n "$requested" ]] || die "RUN_DIR is required"
    [[ "$requested" == /* ]] || die "RUN_DIR must be an absolute path"
    [[ "$requested" != "/" ]] || die "RUN_DIR cannot be /"
    [[ -f "$requested/$MARKER_NAME" ]] || die "not a harness run directory: $requested"
    printf '%s\n' "$requested"
}

state_value() {
    local run_dir=$1
    local query=$2
    jq -er "$query" "$run_dir/state.json"
}

secret_value() {
    local run_dir=$1
    local identity=$2
    local path="$run_dir/secrets/$identity"
    [[ -f "$path" ]] || die "missing fixture identity: $identity"
    IFS= read -r REPLY < "$path"
    [[ -n "$REPLY" ]] || die "empty fixture identity: $identity"
    printf '%s\n' "$REPLY"
}

public_value() {
    local run_dir=$1
    local identity=$2
    jq -er --arg identity "$identity" '.identities[$identity]' "$run_dir/manifest.json"
}

next_mutation_time() {
    local run_dir=$1
    local previous now candidate temporary
    previous=$(jq -er '.last_mutation_timestamp // 0' "$run_dir/state.json")
    now=$(date +%s)
    candidate=$now
    if ((candidate <= previous)); then
        candidate=$((previous + 1))
    fi
    temporary="$run_dir/state.json.next"
    jq --argjson timestamp "$candidate" \
        '.last_mutation_timestamp = $timestamp' "$run_dir/state.json" > "$temporary"
    mv "$temporary" "$run_dir/state.json"
    printf '%s\n' "$candidate"
}

port_is_open() {
    local port=$1
    run_with_timeout 1 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" >/dev/null 2>&1
}

require_port_free() {
    local port=$1
    if port_is_open "$port"; then
        die "port is already in use: $port"
    fi
}

wait_for_relay() {
    local relay_http=$1
    local attempts=80
    local index
    for ((index = 0; index < attempts; index += 1)); do
        if curl --fail --silent --show-error \
            -H 'Accept: application/nostr+json' \
            --max-time 1 "$relay_http" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_for_group_snapshot() {
    local relay=$1
    local group_id=$2
    local attempts=80
    local index
    for ((index = 0; index < attempts; index += 1)); do
        if run_with_timeout 2 nak -q req -k 39000 -d "$group_id" "$relay" 2>/dev/null \
            | jq -e 'select(.kind == 39000)' >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

generate_identity() {
    local run_dir=$1
    local identity=$2
    local secret
    local public_key
    secret=$(nak key generate)
    public_key=$(nak key public "$secret")
    umask 077
    printf '%s\n' "$secret" > "$run_dir/secrets/$identity"
    printf '%s\n' "$public_key"
}

build_croissant() {
    local run_dir=$1
    local source_dir="$run_dir/croissant-source"
    local output_dir="$run_dir/bin"

    mkdir -p "$source_dir" "$output_dir"
    git -C "$source_dir" init --quiet
    git -C "$source_dir" remote add origin "$CROISSANT_REPOSITORY"
    git -C "$source_dir" fetch --quiet --depth 1 origin "$CROISSANT_COMMIT"
    git -C "$source_dir" checkout --quiet --detach FETCH_HEAD
    [[ "$(git -C "$source_dir" rev-parse HEAD)" == "$CROISSANT_COMMIT" ]] \
        || die "Croissant checkout did not resolve to the pinned commit"

    if [[ "$HARNESS_BACKEND" == docker ]]; then
        require_command docker
        docker run --rm \
            --user "$(id -u):$(id -g)" \
            -e GOCACHE=/tmp/go-build \
            -e GOMODCACHE=/tmp/go-mod \
            -v "$source_dir:/src:ro" \
            -v "$output_dir:/out" \
            -w /src \
            "$GO_IMAGE" \
            sh -c "go build -trimpath -ldflags='-X main.currentVersion=nmp-harness-$CROISSANT_COMMIT' -o /out/croissant ."
    elif [[ "$HARNESS_BACKEND" == host ]]; then
        if command -v go >/dev/null 2>&1; then
            (
                cd "$source_dir"
                go build -trimpath \
                    -ldflags="-X main.currentVersion=nmp-harness-$CROISSANT_COMMIT" \
                    -o "$output_dir/croissant" .
            )
        elif command -v docker >/dev/null 2>&1; then
            docker run --rm \
                --user "$(id -u):$(id -g)" \
                -e GOCACHE=/tmp/go-build \
                -e GOMODCACHE=/tmp/go-mod \
                -v "$source_dir:/src:ro" \
                -v "$output_dir:/out" \
                -w /src \
                "$GO_IMAGE" \
                sh -c "go build -trimpath -ldflags='-X main.currentVersion=nmp-harness-$CROISSANT_COMMIT' -o /out/croissant ."
        else
            die "host backend requires Go (Docker may supply the build only)"
        fi
    else
        die "unknown harness backend: $HARNESS_BACKEND (expected docker or host)"
    fi
    [[ -x "$output_dir/croissant" ]] || die "Croissant build produced no executable"
}

start_container() {
    local run_dir=$1
    local suffix=$2
    local port=$3
    local owner_public_key=$4
    local container_name
    container_name=$(state_value "$run_dir" ".containers.$suffix")

    mkdir -p "$run_dir/relay-$suffix"
    docker run --detach \
        --name "$container_name" \
        --publish "127.0.0.1:$port:9888" \
        --volume "$run_dir/relay-$suffix:/data" \
        --volume "$run_dir/bin/croissant:/usr/local/bin/croissant:ro" \
        --env PORT=9888 \
        --env HOST=0.0.0.0 \
        --env "DOMAIN=127.0.0.1:$port" \
        --env DATAPATH=/data \
        --env "OWNER_PUBLIC_KEY=$owner_public_key" \
        "$GO_IMAGE" \
        /usr/local/bin/croissant >/dev/null
}

backend_for_run() {
    state_value "$1" '.backend // "docker"'
}

host_pid_file() {
    printf '%s/relay-%s.pid\n' "$1" "$2"
}

host_process_is_owned() {
    local run_dir=$1
    local suffix=$2
    local pid_file pid command_line
    pid_file=$(host_pid_file "$run_dir" "$suffix")
    [[ -f "$pid_file" ]] || return 1
    IFS= read -r pid < "$pid_file"
    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    command_line=$(ps -p "$pid" -o command= 2>/dev/null || true)
    [[ "$command_line" == *"$run_dir/bin/croissant"* ]]
}

wait_for_host_process_ownership() {
    local run_dir=$1
    local suffix=$2
    local attempts=100
    local index
    for ((index = 0; index < attempts; index += 1)); do
        if host_process_is_owned "$run_dir" "$suffix"; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

wait_for_host_process_exit() {
    local pid=$1
    local attempts=100
    local index state
    for ((index = 0; index < attempts; index += 1)); do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        state=$(ps -p "$pid" -o stat= 2>/dev/null || true)
        if [[ "$state" == Z* ]]; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

report_host_relay_log() {
    local run_dir=$1
    local suffix=$2
    local log_file="$run_dir/witness/relay-$suffix.log"
    if [[ -s "$log_file" ]]; then
        printf 'nip29-consumer-harness: relay %s log tail:\n' "$suffix" >&2
        tail -n 40 "$log_file" >&2
    fi
}

start_host_relay() {
    local run_dir=$1
    local suffix=$2
    local port=$3
    local owner_public_key=$4
    local relay_dir="$run_dir/relay-$suffix"
    local log_file="$run_dir/witness/relay-$suffix.log"
    local pid_file
    pid_file=$(host_pid_file "$run_dir" "$suffix")
    require_command nohup
    mkdir -p "$relay_dir" "$run_dir/witness"
    nohup env \
        PORT="$port" \
        HOST=127.0.0.1 \
        DOMAIN="127.0.0.1:$port" \
        DATAPATH="$relay_dir" \
        OWNER_PUBLIC_KEY="$owner_public_key" \
        "$run_dir/bin/croissant" >> "$log_file" 2>&1 </dev/null &
    printf '%s\n' "$!" > "$pid_file"
    if ! wait_for_host_process_ownership "$run_dir" "$suffix"; then
        report_host_relay_log "$run_dir" "$suffix"
        die "host relay $suffix exited before ownership could be verified"
    fi
}

start_relay() {
    local run_dir=$1
    local suffix=$2
    local port=$3
    local owner_public_key=$4
    case "$(backend_for_run "$run_dir")" in
        docker) start_container "$run_dir" "$suffix" "$port" "$owner_public_key" ;;
        host) start_host_relay "$run_dir" "$suffix" "$port" "$owner_public_key" ;;
        *) die "run has unknown harness backend" ;;
    esac
}

sign_event() {
    local run_dir=$1
    local identity=$2
    local output=$3
    shift 3
    local secret
    secret=$(secret_value "$run_dir" "$identity")
    nak -q event --sec "$secret" "$@" > "$output"
    nak verify < "$output" >/dev/null
}

publish_signed() {
    local run_dir=$1
    local event_file=$2
    shift 2
    local relay
    for relay in "$@"; do
        if ! nak -q event "$relay" < "$event_file" \
            >> "$run_dir/seed/publish.log" 2>&1; then
            die "fixture publication failed: $(basename "$event_file") -> $relay"
        fi
    done
}

sign_and_publish() {
    local run_dir=$1
    local identity=$2
    local output=$3
    local relay_csv=$4
    shift 4
    local -a relays=()
    IFS=',' read -r -a relays <<< "$relay_csv"
    sign_event "$run_dir" "$identity" "$output" "$@"
    publish_signed "$run_dir" "$output" "${relays[@]}"
}

create_group() {
    local run_dir=$1
    local identity=$2
    local relay=$3
    local group_id=$4
    local output="$run_dir/seed/create-$group_id-$(basename "$relay").json"
    sign_and_publish "$run_dir" "$identity" "$output" "$relay" -k 9007 -h "$group_id"
    wait_for_group_snapshot "$relay" "$group_id" \
        || die "relay did not emit metadata for group $group_id at $relay"
}

seed_topology() {
    local run_dir=$1
    local relay_a relay_b
    relay_a=$(state_value "$run_dir" '.relays.a.ws')
    relay_b=$(state_value "$run_dir" '.relays.b.ws')
    local followed outsider
    followed=$(public_value "$run_dir" followed)
    outsider=$(public_value "$run_dir" outsider)

    mkdir -p "$run_dir/seed"
    : > "$run_dir/seed/publish.log"

    create_group "$run_dir" owner_a "$relay_a" solo-a
    create_group "$run_dir" owner_a "$relay_a" bitcoin
    create_group "$run_dir" owner_b "$relay_b" bitcoin
    create_group "$run_dir" owner_a "$relay_a" one-sided

    sign_and_publish "$run_dir" owner_a "$run_dir/seed/metadata-bitcoin-a.json" "$relay_a" \
        -k 9002 -h bitcoin -t 'name=Bitcoin Cash' -t 'about=relay A independent metadata'
    sign_and_publish "$run_dir" owner_b "$run_dir/seed/metadata-bitcoin-b.json" "$relay_b" \
        -k 9002 -h bitcoin -t 'name=Bitcoin (real)' -t 'about=relay B independent metadata'
    sign_and_publish "$run_dir" owner_a "$run_dir/seed/metadata-solo-a.json" "$relay_a" \
        -k 9002 -h solo-a -t 'name=Solo A'

    sign_and_publish "$run_dir" owner_a "$run_dir/seed/member-bitcoin-a.json" "$relay_a" \
        -k 9000 -h bitcoin -p "$followed"
    sign_and_publish "$run_dir" owner_b "$run_dir/seed/member-bitcoin-b.json" "$relay_b" \
        -k 9000 -h bitcoin -p "$outsider"

    sign_and_publish "$run_dir" viewer "$run_dir/seed/viewer-follows-followed.json" "$relay_a,$relay_b" \
        -k 3 -h bitcoin -p "$followed"

    sign_and_publish "$run_dir" writer "$run_dir/seed/shared-kind-9.json" "$relay_a,$relay_b" \
        --created-at 1700000100 -k 9 -h bitcoin -c 'shared chat observed at both hosts'
    sign_and_publish "$run_dir" writer "$run_dir/seed/kind-9-solo-a.json" "$relay_a" \
        --created-at 1700000099 -k 9 -h solo-a -c 'single-host group chat'
    sign_and_publish "$run_dir" writer "$run_dir/seed/kind-9-a.json" "$relay_a" \
        --created-at 1700000101 -k 9 -h bitcoin -c 'relay A chat'
    sign_and_publish "$run_dir" writer "$run_dir/seed/kind-9-b.json" "$relay_b" \
        --created-at 1700000102 -k 9 -h bitcoin -c 'relay B chat'

    sign_and_publish "$run_dir" writer "$run_dir/seed/shared-kind-30023.json" "$relay_a,$relay_b" \
        --created-at 1700000200 -k 30023 -h bitcoin -d shared-article -c 'shared long-form event'
    sign_and_publish "$run_dir" writer "$run_dir/seed/kind-30023-a.json" "$relay_a" \
        --created-at 1700000201 -k 30023 -h bitcoin -d relay-a-article -c 'relay A long-form event'
    sign_and_publish "$run_dir" outsider "$run_dir/seed/kind-30023-b.json" "$relay_b" \
        --created-at 1700000202 -k 30023 -h bitcoin -d relay-b-article -c 'relay B long-form event'

    local index relay timestamp output target
    for ((index = 0; index < 24; index += 1)); do
        timestamp=$((1700000300 + index / 4))
        if ((index % 2 == 0)); then
            relay=$relay_a
            target=a
        else
            relay=$relay_b
            target=b
        fi
        output=$(printf '%s/seed/stress-kind-9-%02d-%s.json' "$run_dir" "$index" "$target")
        sign_and_publish "$run_dir" writer "$output" "$relay" \
            --created-at "$timestamp" -k 9 -h bitcoin -c "stress chat $index from relay $target"
    done

    capture_seed_snapshot "$run_dir" a
    capture_seed_snapshot "$run_dir" b
    write_seed_summary "$run_dir"
}

capture_seed_snapshot() {
    local run_dir=$1
    local suffix=$2
    local relay
    relay=$(state_value "$run_dir" ".relays.$suffix.ws")
    run_with_timeout 10 nak -q req -k 3 -k 9 -k 30023 -k 39000 -k 39001 -k 39002 \
        "$relay" > "$run_dir/seed/relay-$suffix.jsonl"
}

write_seed_summary() {
    local run_dir=$1
    jq -n \
        --slurpfile relay_a "$run_dir/seed/relay-a.jsonl" \
        --slurpfile relay_b "$run_dir/seed/relay-b.jsonl" \
        'def in_group($id): any(.tags[]; .[0] == "h" and .[1] == $id);
        ($relay_a | map(select(.kind == 9).id)) as $a9
        | ($relay_b | map(select(.kind == 9).id)) as $b9
        | ($relay_a | map(select(.kind == 30023).id)) as $a30023
        | ($relay_b | map(select(.kind == 30023).id)) as $b30023
        | ($relay_a | map(select(.kind == 9 and in_group("bitcoin")).id)) as $a9bitcoin
        | ($relay_b | map(select(.kind == 9 and in_group("bitcoin")).id)) as $b9bitcoin
        | {
          relay_a: ($relay_a | group_by(.kind) | map({kind: .[0].kind, count: length})),
          relay_b: ($relay_b | group_by(.kind) | map({kind: .[0].kind, count: length})),
          shared_event_ids: {
            kind_9: [$a9[] | select(. as $id | $b9 | index($id))],
            kind_30023: [$a30023[] | select(. as $id | $b30023 | index($id))]
          },
          group_bitcoin_kind_9: {
            relay_a: ($a9bitcoin | length),
            relay_b: ($b9bitcoin | length),
            distinct: (($a9bitcoin + $b9bitcoin) | unique | length)
          }
        }' > "$run_dir/seed/summary.json"
    verify_seed_summary "$run_dir"
}

# The summary above is a READBACK: it is built from what each relay actually
# serves, not from what the seeding client believed it published. That is the
# only sound place to check the fixture, because a relay can answer a publish
# with `OK: false` without `nak` failing, so a silent mis-seed would otherwise
# reach the consumer intact. It would then surface as the consumer's own
# assertion failing -- one source instead of two, or fewer than 27 distinct
# rows -- and be read as an NMP defect. Every number asserted here is a number
# a consumer asserts too, so a fixture that did not seed what it claims fails
# as a fixture.
verify_seed_summary() {
    local summary="$1/seed/summary.json"
    require_seed_count "$summary" '.shared_event_ids.kind_9 | length' 1 \
        'kind 9 events present at both relays'
    require_seed_count "$summary" '.shared_event_ids.kind_30023 | length' 1 \
        'kind 30023 events present at both relays'
    require_seed_count "$summary" '.group_bitcoin_kind_9.distinct' 27 \
        'distinct kind 9 events in group bitcoin'
}

require_seed_count() {
    local summary=$1 query=$2 expected=$3 label=$4 actual
    actual=$(jq "$query" "$summary")
    [[ "$actual" == "$expected" ]] \
        || die "seed readback shows $actual $label, expected $expected"
}

capture_container_logs() {
    local run_dir=$1
    mkdir -p "$run_dir/witness"
    local suffix container_name
    for suffix in a b; do
        container_name=$(state_value "$run_dir" ".containers.$suffix")
        docker logs "$container_name" > "$run_dir/witness/relay-$suffix.log" 2>&1 || true
    done
}

stop_host_relay() {
    local run_dir=$1
    local suffix=$2
    local pid_file pid
    pid_file=$(host_pid_file "$run_dir" "$suffix")
    [[ -f "$pid_file" ]] || return 0
    IFS= read -r pid < "$pid_file"
    if kill -0 "$pid" 2>/dev/null; then
        host_process_is_owned "$run_dir" "$suffix" \
            || die "refusing to stop unowned process recorded for relay $suffix"
        kill "$pid"
        wait_for_host_process_exit "$pid" \
            || die "host relay $suffix did not exit after SIGTERM"
    fi
    rm -f "$pid_file"
}

stop_containers() {
    local run_dir=$1
    capture_container_logs "$run_dir"
    local suffix container_name
    for suffix in a b; do
        container_name=$(state_value "$run_dir" ".containers.$suffix")
        if docker container inspect "$container_name" >/dev/null 2>&1; then
            docker stop --time 5 "$container_name" >/dev/null 2>&1 || true
            docker rm "$container_name" >/dev/null 2>&1 || true
        fi
    done
}

stop_relays() {
    local run_dir=$1
    case "$(backend_for_run "$run_dir")" in
        docker) stop_containers "$run_dir" ;;
        host)
            stop_host_relay "$run_dir" a
            stop_host_relay "$run_dir" b
            ;;
        *) die "run has unknown harness backend" ;;
    esac
}

start_command() {
    local port_a=$DEFAULT_PORT_A
    local port_b=$DEFAULT_PORT_B
    while (($# > 0)); do
        case "$1" in
            --port-a)
                (($# >= 2)) || die "--port-a requires a value"
                port_a=$2
                shift 2
                ;;
            --port-b)
                (($# >= 2)) || die "--port-b requires a value"
                port_b=$2
                shift 2
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                break
                ;;
        esac
    done
    (($# == 1)) || die "start requires exactly one RUN_DIR"
    [[ "$HARNESS_BACKEND" == docker || "$HARNESS_BACKEND" == host ]] \
        || die "NMP_NIP29_HARNESS_BACKEND must be docker or host"
    validate_port "$port_a"
    validate_port "$port_b"
    [[ "$port_a" != "$port_b" ]] || die "relay ports must be distinct"
    require_port_free "$port_a"
    require_port_free "$port_b"

    local run_dir
    run_dir=$(canonical_new_run_dir "$1")
    umask 077
    mkdir -p "$run_dir/secrets"
    : > "$run_dir/$MARKER_NAME"

    local owner_a owner_b viewer followed outsider writer harness_id
    owner_a=$(generate_identity "$run_dir" owner_a)
    owner_b=$(generate_identity "$run_dir" owner_b)
    viewer=$(generate_identity "$run_dir" viewer)
    followed=$(generate_identity "$run_dir" followed)
    outsider=$(generate_identity "$run_dir" outsider)
    writer=$(generate_identity "$run_dir" writer)
    harness_id=$(printf '%s' "$run_dir" | short_hash)

    jq -n \
        --arg run_dir "$run_dir" \
        --arg commit "$CROISSANT_COMMIT" \
        --arg image "$GO_IMAGE" \
        --arg backend "$HARNESS_BACKEND" \
        --arg container_a "nmp-nip29-a-$harness_id" \
        --arg container_b "nmp-nip29-b-$harness_id" \
        --arg ws_a "ws://127.0.0.1:$port_a" \
        --arg ws_b "ws://127.0.0.1:$port_b" \
        --arg http_a "http://127.0.0.1:$port_a" \
        --arg http_b "http://127.0.0.1:$port_b" \
        --argjson port_a "$port_a" \
        --argjson port_b "$port_b" \
        '{run_dir: $run_dir, croissant_commit: $commit, go_image: $image,
          backend: $backend,
          containers: {a: $container_a, b: $container_b},
          relays: {
            a: {ws: $ws_a, http: $http_a, port: $port_a},
            b: {ws: $ws_b, http: $http_b, port: $port_b}
          }}' > "$run_dir/state.json"

    jq -n \
        --arg commit "$CROISSANT_COMMIT" \
        --arg nak_version "$(nak --version | awk '{print $3}')" \
        --arg owner_a "$owner_a" \
        --arg owner_b "$owner_b" \
        --arg viewer "$viewer" \
        --arg followed "$followed" \
        --arg outsider "$outsider" \
        --arg writer "$writer" \
        --arg relay_a "ws://127.0.0.1:$port_a" \
        --arg relay_b "ws://127.0.0.1:$port_b" \
        '{
          croissant_commit: $commit,
          nak_version: $nak_version,
          relays: {a: $relay_a, b: $relay_b},
          identities: {
            owner_a: $owner_a, owner_b: $owner_b, viewer: $viewer,
            followed: $followed, outsider: $outsider, writer: $writer
          },
          groups: {
            single_host: "solo-a", union: "bitcoin", mixed_outcome: "one-sided"
          },
          policy: {open: true, unrestricted: true, supported_kind_allowlist: null}
        }' > "$run_dir/manifest.json"

    build_croissant "$run_dir"
    start_relay "$run_dir" a "$port_a" "$owner_a"
    start_relay "$run_dir" b "$port_b" "$owner_b"
    trap 'stop_relays "$run_dir"' ERR INT TERM

    wait_for_relay "http://127.0.0.1:$port_a" || die "relay A did not become ready"
    wait_for_relay "http://127.0.0.1:$port_b" || die "relay B did not become ready"
    seed_topology "$run_dir"
    jq --argjson timestamp "$(jq -er '.created_at' "$run_dir/seed/viewer-follows-followed.json")" \
        '.last_mutation_timestamp = $timestamp' "$run_dir/state.json" \
        > "$run_dir/state.json.seeded"
    mv "$run_dir/state.json.seeded" "$run_dir/state.json"
    if [[ "$HARNESS_BACKEND" == docker ]]; then
        docker image inspect "$GO_IMAGE" --format '{{.Id}}' > "$run_dir/go-image-id.txt"
    elif command -v go >/dev/null 2>&1; then
        go version > "$run_dir/go-version.txt"
    else
        docker image inspect "$GO_IMAGE" --format '{{.Id}}' > "$run_dir/go-image-id.txt"
    fi
    trap - ERR INT TERM

    note "started and seeded unrestricted relays"
    note "manifest: $run_dir/manifest.json"
    note "relay A: ws://127.0.0.1:$port_a"
    note "relay B: ws://127.0.0.1:$port_b"
}

status_command() {
    local run_dir
    run_dir=$(canonical_existing_run_dir "$1")
    local suffix container_name
    if [[ "$(backend_for_run "$run_dir")" == docker ]]; then
        require_command docker
        for suffix in a b; do
            container_name=$(state_value "$run_dir" ".containers.$suffix")
            docker container inspect "$container_name" \
                --format "$suffix\t{{.State.Status}}\t{{.State.Running}}"
        done
    else
        for suffix in a b; do
            if host_process_is_owned "$run_dir" "$suffix"; then
                printf '%s\trunning\ttrue\n' "$suffix"
            else
                printf '%s\tstopped\tfalse\n' "$suffix"
            fi
        done
    fi
    jq '{relays, groups, policy, identities}' "$run_dir/manifest.json"
}

metadata_conflict_command() {
    local run_dir
    run_dir=$(canonical_existing_run_dir "$1")
    local relay_a relay_b now
    relay_a=$(state_value "$run_dir" '.relays.a.ws')
    relay_b=$(state_value "$run_dir" '.relays.b.ws')
    now=$(next_mutation_time "$run_dir")
    sign_and_publish "$run_dir" owner_a "$run_dir/seed/metadata-bitcoin-a-live-$now.json" "$relay_a" \
        --created-at "$now" -k 9002 -h bitcoin -t 'name=Bitcoin Cash live'
    sign_and_publish "$run_dir" owner_b "$run_dir/seed/metadata-bitcoin-b-live-$now.json" "$relay_b" \
        --created-at "$now" -k 9002 -h bitcoin -t 'name=Bitcoin (real) live'
    note "published divergent metadata changes at $now"
}

chat_append_command() {
    local run_dir
    run_dir=$(canonical_existing_run_dir "$1")
    local relay_a relay_b now
    relay_a=$(state_value "$run_dir" '.relays.a.ws')
    relay_b=$(state_value "$run_dir" '.relays.b.ws')
    now=$(next_mutation_time "$run_dir")
    sign_and_publish "$run_dir" writer "$run_dir/seed/shared-kind-9-live-$now.json" \
        "$relay_a,$relay_b" --created-at "$now" -k 9 -h bitcoin \
        -c 'shared live chat after sibling cancellation'
    note "published shared live chat mutation at $now"
}

follow_command() {
    local action=$1
    local run_dir
    run_dir=$(canonical_existing_run_dir "$2")
    local relay_a relay_b followed outsider now output
    relay_a=$(state_value "$run_dir" '.relays.a.ws')
    relay_b=$(state_value "$run_dir" '.relays.b.ws')
    followed=$(public_value "$run_dir" followed)
    outsider=$(public_value "$run_dir" outsider)
    now=$(next_mutation_time "$run_dir")
    output="$run_dir/seed/viewer-follows-$action-$now.json"
    if [[ "$action" == add ]]; then
        sign_and_publish "$run_dir" viewer "$output" "$relay_a,$relay_b" \
            --created-at "$now" -k 3 -h bitcoin -p "$followed"
    else
        sign_and_publish "$run_dir" viewer "$output" "$relay_a,$relay_b" \
            --created-at "$now" -k 3 -h bitcoin -p "$outsider"
    fi
    note "published viewer follow replacement: $action qualifying subject"
}

relay_lifecycle_command() {
    local action=$1
    local suffix=$2
    local run_dir
    [[ "$suffix" == a || "$suffix" == b ]] || die "relay must be a or b"
    run_dir=$(canonical_existing_run_dir "$3")
    local backend
    backend=$(backend_for_run "$run_dir")
    if [[ "$backend" == docker ]]; then
        require_command docker
        local container_name
        container_name=$(state_value "$run_dir" ".containers.$suffix")
        if [[ "$action" == down ]]; then
            docker stop --time 5 "$container_name" >/dev/null
        else
            docker start "$container_name" >/dev/null
        fi
    elif [[ "$action" == down ]]; then
        stop_host_relay "$run_dir" "$suffix"
    else
        local port owner_public_key
        port=$(state_value "$run_dir" ".relays.$suffix.port")
        owner_public_key=$(public_value "$run_dir" "owner_$suffix")
        start_host_relay "$run_dir" "$suffix" "$port" "$owner_public_key"
    fi
    if [[ "$action" == down ]]; then
        note "relay $suffix stopped"
    else
        local relay_http
        relay_http=$(state_value "$run_dir" ".relays.$suffix.http")
        wait_for_relay "$relay_http" || die "relay $suffix did not become ready after restart"
        note "relay $suffix restarted"
    fi
}

stop_command() {
    local run_dir
    run_dir=$(canonical_existing_run_dir "$1")
    local port_a port_b
    port_a=$(state_value "$run_dir" '.relays.a.port')
    port_b=$(state_value "$run_dir" '.relays.b.port')
    local backend
    backend=$(backend_for_run "$run_dir")
    stop_relays "$run_dir"
    if port_is_open "$port_a" || port_is_open "$port_b"; then
        die "one or more relay ports remain open after teardown"
    fi
    jq -n \
        --arg stopped_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg backend "$backend" \
        --argjson port_a "$port_a" \
        --argjson port_b "$port_b" \
        '{stopped_at: $stopped_at, backend: $backend,
          containers_removed: ($backend == "docker"), relay_processes_stopped: true,
          released_ports: [$port_a, $port_b]}' > "$run_dir/teardown.json"
    note "stopped $backend relays and released ports $port_a/$port_b"
    note "retained evidence: $run_dir"
}

main() {
    require_common_commands
    (($# >= 1)) || {
        usage
        exit 2
    }
    local command=$1
    shift
    case "$command" in
        start)
            start_command "$@"
            ;;
        status)
            (($# == 1)) || die "status requires RUN_DIR"
            status_command "$1"
            ;;
        metadata-conflict)
            (($# == 1)) || die "metadata-conflict requires RUN_DIR"
            metadata_conflict_command "$1"
            ;;
        chat-append)
            (($# == 1)) || die "chat-append requires RUN_DIR"
            chat_append_command "$1"
            ;;
        follow-remove)
            (($# == 1)) || die "follow-remove requires RUN_DIR"
            follow_command remove "$1"
            ;;
        follow-add)
            (($# == 1)) || die "follow-add requires RUN_DIR"
            follow_command add "$1"
            ;;
        relay-down)
            (($# == 2)) || die "relay-down requires a|b RUN_DIR"
            relay_lifecycle_command down "$1" "$2"
            ;;
        relay-up)
            (($# == 2)) || die "relay-up requires a|b RUN_DIR"
            relay_lifecycle_command up "$1" "$2"
            ;;
        stop)
            (($# == 1)) || die "stop requires RUN_DIR"
            stop_command "$1"
            ;;
        --help|-h|help)
            usage
            ;;
        *)
            usage
            die "unknown command: $command"
            ;;
    esac
}

main "$@"
