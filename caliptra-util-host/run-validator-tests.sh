#!/usr/bin/env bash
# Licensed under the Apache-2.0 license
#
# Runs the caliptra-util-host validator tests locally,
# mirroring .github/workflows/caliptra-util-host-validator.yml.
#
# Usage:
#   ./caliptra-util-host/run-validator-tests.sh          # run all steps
#   ./caliptra-util-host/run-validator-tests.sh --skip-libspdm  # skip libspdm build

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UTIL_HOST_DIR="${REPO_ROOT}/caliptra-util-host"
SKIP_LIBSPDM=false

for arg in "$@"; do
  case "$arg" in
    --skip-libspdm) SKIP_LIBSPDM=true ;;
    *) echo "Unknown option: $arg"; exit 1 ;;
  esac
done

step() {
  echo ""
  echo "========================================"
  echo "  $1"
  echo "========================================"
}

# ------------------------------------------------------------------
# Step 1: Fetch cargo git dependencies (needed to locate SPDM-Utils)
# ------------------------------------------------------------------
step "Fetching cargo git dependencies"
pushd "${UTIL_HOST_DIR}" > /dev/null
cargo fetch
popd > /dev/null

# ------------------------------------------------------------------
# Step 2: Build libspdm (skip if libs already exist or --skip-libspdm)
# ------------------------------------------------------------------
export LIBSPDM_LIB_DIR="${UTIL_HOST_DIR}/target/libspdm-lib"

if [ "${SKIP_LIBSPDM}" = true ]; then
  step "Skipping libspdm build (--skip-libspdm)"
elif [ -d "${LIBSPDM_LIB_DIR}" ] && [ "$(find "${LIBSPDM_LIB_DIR}" -name '*.a' 2>/dev/null | wc -l)" -ge 19 ]; then
  step "Skipping libspdm build (${LIBSPDM_LIB_DIR} already has $(find "${LIBSPDM_LIB_DIR}" -name '*.a' | wc -l) libs)"
else
  step "Building libspdm"
  SPDM_UTILS_DIR=$(find ~/.cargo/git/checkouts -maxdepth 3 -name "Cargo.toml" -path "*spdm-utils*" -exec dirname {} \; | head -1)
  if [ -z "${SPDM_UTILS_DIR}" ]; then
    echo "ERROR: SPDM-Utils checkout not found. Run 'cd caliptra-util-host && cargo fetch' first."
    exit 1
  fi
  LIBSPDM_SRC="${SPDM_UTILS_DIR}/third-party/libspdm"
  BUILD_DIR="${UTIL_HOST_DIR}/target/libspdm-build"
  mkdir -p "${BUILD_DIR}"
  pushd "${BUILD_DIR}" > /dev/null
  cmake \
    -DARCH=x64 \
    -DTOOLCHAIN=GCC \
    -DTARGET=Debug \
    -DCRYPTO=openssl \
    -DENABLE_BINARY_BUILD=1 \
    -DCOMPILED_LIBCRYPTO_PATH=/usr/lib/ \
    -DCOMPILED_LIBSSL_PATH=/usr/lib/ \
    -DDISABLE_TESTS=1 \
    -DCMAKE_C_FLAGS="-DLIBSPDM_ENABLE_CAPABILITY_EVENT_CAP=0 -DLIBSPDM_ENABLE_CAPABILITY_MEL_CAP=0 -DLIBSPDM_HAL_PASS_SPDM_CONTEXT=1 -DLIBSPDM_ENABLE_CAPABILITY_GET_KEY_PAIR_INFO_CAP=0 -DLIBSPDM_ENABLE_CAPABILITY_SET_KEY_PAIR_INFO_CAP=0" \
    "${LIBSPDM_SRC}"
  make -j"$(nproc)"
  popd > /dev/null
  mkdir -p "${LIBSPDM_LIB_DIR}"
  find "${BUILD_DIR}/lib" -name "*.a" -exec cp {} "${LIBSPDM_LIB_DIR}/" \;
  echo "libspdm built: $(find "${LIBSPDM_LIB_DIR}" -name '*.a' | wc -l) libraries in ${LIBSPDM_LIB_DIR}"
fi

# ------------------------------------------------------------------
# Step 3: caliptra-util-host precheckin (fmt + clippy + check)
# ------------------------------------------------------------------
step "Running caliptra-util-host precheckin"
pushd "${UTIL_HOST_DIR}" > /dev/null
cargo xtask precheckin
popd > /dev/null

# ------------------------------------------------------------------
# Step 4: Build caliptra-util-host workspace
# ------------------------------------------------------------------
step "Building caliptra-util-host"
pushd "${UTIL_HOST_DIR}" > /dev/null
cargo xtask build
popd > /dev/null

# ------------------------------------------------------------------
# Step 5: Run caliptra-util-host unit tests
# ------------------------------------------------------------------
step "Running caliptra-util-host tests"
pushd "${UTIL_HOST_DIR}" > /dev/null
cargo xtask test
popd > /dev/null

# ------------------------------------------------------------------
# Step 6: Build everything (needed for integration tests)
# ------------------------------------------------------------------
step "Building all targets (cargo xtask all-build)"
pushd "${REPO_ROOT}" > /dev/null
cargo xtask all-build
popd > /dev/null

# ------------------------------------------------------------------
# Step 7: Mailbox validator integration test
# ------------------------------------------------------------------
step "Running Caliptra Util Host validator tests"
pushd "${REPO_ROOT}" > /dev/null
cargo test --package caliptra-mcu-tests-integration --lib \
  -- test::test_caliptra_util_host_validator --nocapture --include-ignored
popd > /dev/null

# ------------------------------------------------------------------
# Step 8: MCTP VDM validator integration test
# ------------------------------------------------------------------
step "Running Caliptra Util Host MCTP VDM validator tests"
pushd "${REPO_ROOT}" > /dev/null
cargo test --package caliptra-mcu-tests-integration --lib \
  -- test_mctp_vdm_validator::test::test_caliptra_util_host_mctp_vdm_validator \
  --nocapture --include-ignored
popd > /dev/null

# ------------------------------------------------------------------
# Step 9: SPDM VDM validator integration test (production mode)
# ------------------------------------------------------------------
step "Running Caliptra Util Host SPDM VDM validator tests (production mode)"
pushd "${REPO_ROOT}" > /dev/null
cargo test --package caliptra-mcu-tests-integration --lib \
  -- test_caliptra_util_host_spdm_vdm_validator::test::test_caliptra_util_host_spdm_vdm_validator \
  --nocapture --include-ignored
popd > /dev/null

# ------------------------------------------------------------------
# Step 10: SPDM VDM validator integration test (manufacturing mode)
# ------------------------------------------------------------------
step "Running Caliptra Util Host SPDM VDM validator tests (manufacturing mode)"
pushd "${REPO_ROOT}" > /dev/null
cargo test --package caliptra-mcu-tests-integration --lib \
  -- test_caliptra_util_host_spdm_vdm_validator::test::test_caliptra_util_host_spdm_vdm_validator_mfg_mode \
  --nocapture --include-ignored
popd > /dev/null

# ------------------------------------------------------------------
# Done
# ------------------------------------------------------------------
echo ""
echo "========================================"
echo "  All validator tests passed!"
echo "========================================"
