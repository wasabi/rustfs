#!/usr/bin/env bash
# Set CARGO_BUILD_JOBS from CPU count and memory: up to half of logical cores,
# capped when RAM per concurrent rustc job would be too low.
#
# Usage:
#   compute-cargo-build-jobs.sh           # log to stderr; append to GITHUB_ENV if set
#   compute-cargo-build-jobs.sh --value-only   # print job count on stdout only (for Make)
#
# Override knobs (optional):
#   CARGO_JOBS_RESERVE_MB   — MiB reserved for OS / link peaks (default 4096)
#   CARGO_JOBS_PER_RUSTC_MB — budget MiB per parallel rustc job (default 2048)

set -euo pipefail

VALUE_ONLY=0
if [[ "${1:-}" == "--value-only" ]]; then
  VALUE_ONLY=1
fi

readonly RESERVE_MB="${CARGO_JOBS_RESERVE_MB:-4096}"
readonly PER_JOB_MB="${CARGO_JOBS_PER_RUSTC_MB:-2048}"

detect_resources() {
  local os
  os="$(uname -s 2>/dev/null || echo Unknown)"

  case "$os" in
    Linux)
      cores="$(nproc 2>/dev/null || echo 2)"
      if [[ -r /proc/meminfo ]]; then
        local mem_kb
        mem_kb="$(grep -E '^MemAvailable:' /proc/meminfo 2>/dev/null | awk '{print $2}' || true)"
        if [[ -z "${mem_kb:-}" || "${mem_kb:-0}" -eq 0 ]]; then
          mem_kb="$(grep -E '^MemTotal:' /proc/meminfo 2>/dev/null | awk '{print $2}' || echo 8388608)"
        fi
        mem_mb=$((mem_kb / 1024))
      else
        mem_mb=8192
      fi
      ;;
    Darwin)
      cores="$(sysctl -n hw.ncpu 2>/dev/null || echo 2)"
      local mem_bytes
      mem_bytes="$(sysctl -n hw.memsize 2>/dev/null || echo 8589934592)"
      mem_mb=$((mem_bytes / 1024 / 1024))
      # No MemAvailable equivalent; assume a large fraction is usable on CI runners.
      mem_mb=$((mem_mb * 8 / 10))
      ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT)
      cores="${NUMBER_OF_PROCESSORS:-2}"
      if command -v powershell.exe >/dev/null 2>&1; then
        # FreePhysicalMemory is KiB per WMI.
        local free_kb
        free_kb="$(powershell.exe -NoProfile -Command \
          "[int]((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory)" 2>/dev/null || echo 8388608)"
        mem_mb=$((free_kb / 1024))
      else
        mem_mb=8192
      fi
      ;;
    *)
      cores=2
      mem_mb=8192
      ;;
  esac
}

detect_resources

half=$((cores / 2))
[[ "$half" -lt 1 ]] && half=1

if ((mem_mb > RESERVE_MB)); then
  usable_mb=$((mem_mb - RESERVE_MB))
else
  usable_mb=$((mem_mb * 3 / 4))
fi
[[ "$usable_mb" -lt 256 ]] && usable_mb=256

max_by_mem=$((usable_mb / PER_JOB_MB))
[[ "$max_by_mem" -lt 1 ]] && max_by_mem=1

if [[ "$half" -le "$max_by_mem" ]]; then
  jobs=$half
  detail="half of ${cores} cores (memory sufficient)"
else
  jobs=$max_by_mem
  detail="capped by memory (~${mem_mb} MiB reported, ~${usable_mb} MiB budget, ${PER_JOB_MB} MiB/job)"
fi

if [[ "$VALUE_ONLY" -eq 1 ]]; then
  printf '%s\n' "$jobs"
else
  echo "Cargo parallelism: CARGO_BUILD_JOBS=${jobs} (${detail})" >&2
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    {
      echo "CARGO_BUILD_JOBS=${jobs}"
    } >>"$GITHUB_ENV"
  fi
fi
