#!/usr/bin/env bash
# Check an independent consumer: workspace lint dependencies deliberately use HTTP.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
mkdir -p "$tmp/src"
cat > "$tmp/Cargo.toml" <<EOF
[package]
name = "sof-independent-consumer"
version = "0.0.0"
edition = "2024"

[workspace]

[features]
parquet = ["octofhir-sof/parquet"]

[dependencies]
octofhir-sof = { path = "$root/crates/octofhir-sof", default-features = false }
serde_json = "1.0.154"
EOF
cat > "$tmp/src/main.rs" <<'EOF'
use octofhir_sof::{SqlGenerator, ViewDefinition, execute_blocking};
use serde_json::json;

fn main() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": [{"column": [{"name": "id", "path": "id"}]}]
    })).unwrap();
    let result = execute_blocking(&view, &[json!({"resourceType": "Patient", "id": "p1"})]).unwrap();
    assert_eq!(result.data, vec![vec![json!("p1")]]);
    assert_eq!(result.row_count, 1);
    assert_eq!(SqlGenerator::new().generate(&view).unwrap().columns.len(), 1);
}
EOF
# Keep this smoke build on the same resolved versions as the workspace.
cp "$root/Cargo.lock" "$tmp/Cargo.lock"
for mode in default parquet; do
    flags=(--no-default-features)
    if [[ "$mode" == parquet ]]; then
        flags+=(--features parquet)
    fi
    cargo run --manifest-path "$tmp/Cargo.toml" "${flags[@]}"
    cargo tree --manifest-path "$tmp/Cargo.toml" --locked "${flags[@]}" \
        --edges normal,build --prefix none --format '{p}' > "$tmp/$mode.tree"
    if grep -Eq '^reqwest v' "$tmp/$mode.tree"; then
        echo "ERROR: independent $mode consumer pulls in reqwest" >&2
        cat "$tmp/$mode.tree" >&2
        exit 1
    fi
    echo "Independent $mode consumer builds and runs without reqwest."
done
