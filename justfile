default:
    @just --list

toolkit_shell_cmd := "./scripts/toolkit-shell.sh"
toolkit_cmd := "./scripts/toolkit-shell.sh toolkit-xtask"
toolkit_dylint := "./scripts/toolkit-shell.sh toolkit-dylint --repo-root ."
install_dylint_cmd := "./scripts/toolkit-shell.sh toolkit-install-dylint"
fmt_cmd := "./scripts/toolkit-shell.sh toolkit-fmt"

# check workspace compiles
check:
    cargo check --workspace

# build all crates
build:
    cargo build --workspace

# run all tests
test:
    cargo test --workspace

# run clippy lints
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# format code (uses the toolkit-owned nightly rustfmt policy)
fmt:
    {{fmt_cmd}} --all

# check formatting (uses the toolkit-owned nightly rustfmt policy)
fmt-check:
    {{fmt_cmd}} --all -- --check

toolkit-show-config:
    {{toolkit_cmd}} show-config --repo-root . --config policy/toolkit.toml

proc-macro-scope:
    {{toolkit_cmd}} check proc-macro-scope --repo-root . --config policy/toolkit.toml

result-must-use:
    {{toolkit_cmd}} check result-must-use --repo-root . --config policy/toolkit.toml

crate-root-policy:
    {{toolkit_cmd}} check crate-root-policy --repo-root . --config policy/toolkit.toml

test-boundaries:
    {{toolkit_cmd}} check test-boundaries --repo-root . --config policy/toolkit.toml

docs-link-check:
    {{toolkit_cmd}} check docs-link-check --repo-root . --config policy/toolkit.toml

docs-semantic-drift:
    {{toolkit_cmd}} check docs-semantic-drift --repo-root . --config policy/toolkit.toml

ignored-result:
    {{toolkit_cmd}} check ignored-result --repo-root . --config policy/toolkit.toml

unsafe-boundary:
    {{toolkit_cmd}} check unsafe-boundary --repo-root . --config policy/toolkit.toml

bool-param:
    {{toolkit_cmd}} check bool-param --repo-root . --config policy/toolkit.toml

must-use-public-return:
    {{toolkit_cmd}} check must-use-public-return --repo-root . --config policy/toolkit.toml

assert-shape:
    {{toolkit_cmd}} check assert-shape --repo-root . --config policy/toolkit.toml

drop-side-effects:
    {{toolkit_cmd}} check drop-side-effects --repo-root . --config policy/toolkit.toml

recursion-guard:
    {{toolkit_cmd}} check recursion-guard --repo-root . --config policy/toolkit.toml

naming-units:
    {{toolkit_cmd}} check naming-units --repo-root . --config policy/toolkit.toml

limit-constant:
    {{toolkit_cmd}} check limit-constant --repo-root . --config policy/toolkit.toml

public-type-width:
    {{toolkit_cmd}} check public-type-width --repo-root . --config policy/toolkit.toml

dependency-policy:
    {{toolkit_cmd}} check dependency-policy --repo-root . --config policy/toolkit.toml

# fast environment sanity checks
ci-preflight:
    ./scripts/preflight.sh

# run all CI checks locally
ci-dry-run:
    #!/usr/bin/env bash
    set -euo pipefail
    export CARGO_INCREMENTAL=0
    export CARGO_TERM_COLOR=always
    GREEN='\033[0;32m' RED='\033[0;31m' NC='\033[0m'
    exit_code=0
    current=0
    STEPS=()
    FAILURES=()
    run_id="$(date +%Y%m%d-%H%M%S)"
    log_root="${PWD}/artifacts/ci-dry-run/${run_id}"
    mkdir -p "$log_root"

    add_step() {
        local name="$1" cmd="$2"
        STEPS+=("${name}:::${cmd}")
    }

    slugify() {
        printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-'
    }

    run_step() {
        local name="$1" cmd="$2" slug log_path start_ts end_ts duration
        current=$((current + 1))
        slug="$(slugify "$name")"
        log_path="$(printf '%s/%02d-%s.log' "$log_root" "$current" "$slug")"
        printf "[%d/%d] %s... " "$current" "$total" "$name"
        start_ts="$(date +%s)"
        if bash -lc "$cmd" >"$log_path" 2>&1; then
            end_ts="$(date +%s)"
            duration=$((end_ts - start_ts))
            echo -e "${GREEN}OK${NC} (${duration}s)"
        else
            end_ts="$(date +%s)"
            duration=$((end_ts - start_ts))
            echo -e "${RED}FAIL${NC} (${duration}s)"
            echo "  log: $log_path"
            tail -n 30 "$log_path" | sed 's/^/    /'
            FAILURES+=("$name")
            exit_code=1
        fi
    }

    add_step "Preflight"               "./scripts/preflight.sh"
    add_step "Format Check"            "{{fmt_cmd}} --all -- --check"
    add_step "Clippy"                  "nix develop --command cargo clippy --workspace --all-targets -- -D warnings"
    add_step "Tests"                   "nix develop --command cargo test --workspace"
    add_step "Docs Link Check"         "{{toolkit_cmd}} check docs-link-check --repo-root . --config policy/toolkit.toml"
    add_step "Docs Semantic Drift"     "{{toolkit_cmd}} check docs-semantic-drift --repo-root . --config policy/toolkit.toml"
    add_step "Proc Macro Scope"        "{{toolkit_cmd}} check proc-macro-scope --repo-root . --config policy/toolkit.toml"
    add_step "Result Must Use"         "{{toolkit_cmd}} check result-must-use --repo-root . --config policy/toolkit.toml"
    add_step "Test Boundaries"         "{{toolkit_cmd}} check test-boundaries --repo-root . --config policy/toolkit.toml"
    add_step "Crate Root Policy"       "{{toolkit_cmd}} check crate-root-policy --repo-root . --config policy/toolkit.toml"
    add_step "Ignored Result"          "{{toolkit_cmd}} check ignored-result --repo-root . --config policy/toolkit.toml"
    add_step "Unsafe Boundary"         "{{toolkit_cmd}} check unsafe-boundary --repo-root . --config policy/toolkit.toml"
    add_step "Bool Param"              "{{toolkit_cmd}} check bool-param --repo-root . --config policy/toolkit.toml"
    add_step "Must Use Public Return"  "{{toolkit_cmd}} check must-use-public-return --repo-root . --config policy/toolkit.toml"
    add_step "Assert Shape"            "{{toolkit_cmd}} check assert-shape --repo-root . --config policy/toolkit.toml"
    add_step "Drop Side Effects"       "{{toolkit_cmd}} check drop-side-effects --repo-root . --config policy/toolkit.toml"
    add_step "Recursion Guard"         "{{toolkit_cmd}} check recursion-guard --repo-root . --config policy/toolkit.toml"
    add_step "Naming Units"            "{{toolkit_cmd}} check naming-units --repo-root . --config policy/toolkit.toml"
    add_step "Limit Constant"          "{{toolkit_cmd}} check limit-constant --repo-root . --config policy/toolkit.toml"
    add_step "Public Type Width"       "{{toolkit_cmd}} check public-type-width --repo-root . --config policy/toolkit.toml"
    add_step "Dependency Policy"       "{{toolkit_cmd}} check dependency-policy --repo-root . --config policy/toolkit.toml"
    add_step "Install cargo-dylint"    "{{install_dylint_cmd}}"
    add_step "Dylint Trait Purity"     "env CARGO_INCREMENTAL=0 {{toolkit_dylint}} --toolkit-lint trait_purity --all -- --all-targets"

    total=${#STEPS[@]}
    echo "CI Dry Run"
    echo "=========="
    echo "Logs: $log_root"
    echo ""

    for step in "${STEPS[@]}"; do
        name="${step%%:::*}"
        cmd="${step#*:::}"
        run_step "$name" "$cmd"
    done

    echo ""
    if [ $exit_code -eq 0 ]; then
        echo -e "${GREEN}All CI checks passed${NC}"
    else
        echo "Failed:"
        for failure in "${FAILURES[@]}"; do
            echo "  - $failure"
        done
        echo -e "${RED}Some CI checks failed${NC}"
        exit 1
    fi

# enter the pinned toolkit shell for nightly formatter and dylint commands
toolkit-shell:
    {{toolkit_shell_cmd}} bash -lc 'exec "${SHELL:-bash}" -l'

# backwards-compatible alias for the toolkit shell
nightly-shell: toolkit-shell

install-dylint:
    {{install_dylint_cmd}}

dylint-trait-purity:
    env CARGO_INCREMENTAL=0 {{toolkit_dylint}} --toolkit-lint trait_purity --all -- --all-targets
