#!/usr/bin/env bash
# fetch-slt.sh — download the upstream sqllogictest corpus (a curated subset)
# into `target/slt/` for the Rustqlite test harness to consume.
#
# Source: https://github.com/cwida/sqllogictest (the original SQLite-compatible
# sqllogictest corpus, ~3 MB across 5 main test files + a 12-file `evidence/`
# subdir exercising the full SQLite surface — CREATE/INSERT/SELECT/DELETE/
# DROP/UPDATE/REINDEX/REPLACE/aggregate functions/triggers/views).
#
# This is invoked by the harness test the first time it's run, so a clean
# checkout builds and tests the SLT smoke manifest without manual setup.

set -euo pipefail

# Pin to a specific commit so the corpus is reproducible. Bump this when
# intentionally upgrading the test corpus; see TESTING.md.
SLT_REF="${SLT_REF:-master}"
SLT_REPO="https://raw.githubusercontent.com/cwida/sqllogictest/${SLT_REF}"

# Resolve the destination relative to the workspace root (this script lives
# in `crates/rustqlite/xtask/`).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
DEST="${WORKSPACE_ROOT}/target/slt"

mkdir -p "${DEST}/evidence"

# The 5 main test files (~3 MB total) + the 12-file evidence/ subdir. The
# harness reads paths from `tests/slt/manifest.txt`.
TOP_LEVEL=(
  select1.test
  select2.test
  select3.test
  select4.test
  select5.test
)
EVIDENCE=(
  in1.test
  in2.test
  slt_lang_aggfunc.test
  slt_lang_createtrigger.test
  slt_lang_createview.test
  slt_lang_dropindex.test
  slt_lang_droptable.test
  slt_lang_droptrigger.test
  slt_lang_dropview.test
  slt_lang_reindex.test
  slt_lang_replace.test
  slt_lang_update.test
)

for f in "${TOP_LEVEL[@]}"; do
  out="${DEST}/${f}"
  if [[ ! -f "${out}" ]]; then
    echo "fetch-slt: downloading ${f}"
    curl -fsSL "${SLT_REPO}/test/${f}" -o "${out}"
  fi
done

for f in "${EVIDENCE[@]}"; do
  out="${DEST}/evidence/${f}"
  if [[ ! -f "${out}" ]]; then
    echo "fetch-slt: downloading evidence/${f}"
    curl -fsSL "${SLT_REPO}/test/evidence/${f}" -o "${out}"
  fi
done

echo "fetch-slt: corpus is at ${DEST}"
