#!/bin/bash
# Test using a disposable keychain and CA, never the user's login keychain.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname)" == Darwin ]] || { echo 'This check requires macOS'; exit 1; }
mkdir -p target/device-qa
swiftc crates/sm-server/src/bin/device/device_keychain.swift -o target/device-qa/keychain-helper
qa_dir=$(mktemp -d /tmp/sm1726-keychain.XXXXXX)
qa_keychain="$qa_dir/test.keychain-db"
trap 'security delete-keychain "$qa_keychain" >/dev/null 2>&1 || true; rm -rf "$qa_dir"' EXIT
security create-keychain -p sm-test-only "$qa_keychain"
security unlock-keychain -p sm-test-only "$qa_keychain"
# Exercise the real permission-repair functions without modifying a user's key
# or requiring an authorization dialog in an unattended test.
sed '/^do { try run() } catch {/,$d' crates/sm-server/src/bin/device/device_keychain.swift > "$qa_dir/access-repair.swift"
cat scripts/test-device-key-access-repair.swift >> "$qa_dir/access-repair.swift"
swift "$qa_dir/access-repair.swift"
target/device-qa/keychain-helper prepare qa-device "$qa_keychain" > "$qa_dir/device.csr"
swift scripts/check-device-key-access.swift "$qa_keychain"
target/device-qa/keychain-helper repair qa-device "$qa_keychain"
openssl req -in "$qa_dir/device.csr" -verify -noout
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$qa_dir/ca.key" -out "$qa_dir/ca.pem" -subj /CN=sm-test-ca -days 1 >/dev/null 2>&1
openssl x509 -req -in "$qa_dir/device.csr" -CA "$qa_dir/ca.pem" -CAkey "$qa_dir/ca.key" -CAcreateserial -out "$qa_dir/device.pem" -days 1
target/device-qa/keychain-helper import qa-device "$qa_keychain" < "$qa_dir/device.pem"
echo 'Mac non-exportable key, CSR signature, and certificate identity passed.'
