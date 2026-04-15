# shellcheck shell=bash
# Copied from wasabi/lab (mozilla/sccache + lab Redis). Source before cargo on self-hosted lab runners.
# See .github/workflows/lab-sccache-bench.yml
#
# If sccache is missing but RUSTC_WRAPPER/CARGO_BUILD_RUSTC_WRAPPER still reference it, we warn, unset
# those variables, and append clears to GITHUB_ENV when set so later Actions steps do not fail cargo.

_lab_sccache_redis_url() {
  printf '%s' "${SCCACHE_REDIS:-${LAB_SCCACHE_REDIS:-${LAB_SCCACHE_REDIS_DEFAULT:-}}}"
}

_lab_sccache_tcp_ok() {
  local host="$1"
  local port="$2"
  [[ -n "$host" && -n "$port" ]] || return 1
  # Clear BASH_ENV: nested non-interactive bash would otherwise re-source this file (see lab-rust-sccache-env.sh).
  BASH_ENV= timeout 1 bash -c "echo > /dev/tcp/${host}/${port}" 2>/dev/null
}

_lab_sccache_parse_redis_tcp_target() {
  local url="$1"
  local host port
  if [[ "$url" =~ ^redis://([^:/@]+):([0-9]+)(/|$) ]]; then
    host="${BASH_REMATCH[1]}"
    port="${BASH_REMATCH[2]}"
  elif [[ "$url" =~ ^redis://([^:/@]+)(/|$) ]]; then
    host="${BASH_REMATCH[1]}"
    port=6379
  else
    return 1
  fi
  printf '%s %s' "$host" "$port"
}

if command -v sccache >/dev/null 2>&1; then
  export RUSTC_WRAPPER=sccache
  export CARGO_BUILD_RUSTC_WRAPPER=sccache
  _lab_url="$(_lab_sccache_redis_url)"
  if [[ -n "$_lab_url" ]]; then
    if [[ "$_lab_url" == rediss://* ]]; then
      export SCCACHE_REDIS="$_lab_url"
    elif _lab_read="$(_lab_sccache_parse_redis_tcp_target "$_lab_url")"; then
      read -r _lab_h _lab_p <<<"$_lab_read"
      if _lab_sccache_tcp_ok "$_lab_h" "$_lab_p"; then
        export SCCACHE_REDIS="$_lab_url"
      else
        unset SCCACHE_REDIS 2>/dev/null || true
      fi
      unset _lab_h _lab_p _lab_read 2>/dev/null || true
    fi
  fi
  unset _lab_url 2>/dev/null || true
else
  _lab_sccache_broken_wrapper=0
  if [[ "${RUSTC_WRAPPER:-}" == sccache || "${CARGO_BUILD_RUSTC_WRAPPER:-}" == sccache ]]; then
    _lab_sccache_broken_wrapper=1
  elif [[ -n "${RUSTC_WRAPPER:-}" && "$(basename -- "${RUSTC_WRAPPER}")" == sccache && ! -x "${RUSTC_WRAPPER}" ]]; then
    _lab_sccache_broken_wrapper=1
  elif [[ -n "${CARGO_BUILD_RUSTC_WRAPPER:-}" && "$(basename -- "${CARGO_BUILD_RUSTC_WRAPPER}")" == sccache && ! -x "${CARGO_BUILD_RUSTC_WRAPPER}" ]]; then
    _lab_sccache_broken_wrapper=1
  fi
  if ((_lab_sccache_broken_wrapper)); then
    echo >&2 "warning: sccache is configured as the Rust compiler wrapper but sccache is not available; continuing without it (rustc directly)."
    unset RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER SCCACHE_REDIS 2>/dev/null || true
    if [[ -n "${GITHUB_ENV:-}" ]]; then
      {
        echo "RUSTC_WRAPPER="
        echo "CARGO_BUILD_RUSTC_WRAPPER="
      } >>"$GITHUB_ENV"
    fi
  fi
  unset _lab_sccache_broken_wrapper 2>/dev/null || true
fi

unset -f _lab_sccache_redis_url _lab_sccache_tcp_ok _lab_sccache_parse_redis_tcp_target 2>/dev/null || true
