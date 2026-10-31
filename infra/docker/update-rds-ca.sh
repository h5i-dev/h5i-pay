#!/usr/bin/env bash
# Refresh the vendored Amazon RDS global CA bundle that the API image ships
# at /etc/h5i-pay/rds-global-bundle.pem (DB_CA_FILE). Review the diff and
# commit it; the image build itself never downloads trust anchors.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -sSfL --proto '=https' --tlsv1.2 https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem -o "$tmp"
n=$(grep -c 'BEGIN CERTIFICATE' "$tmp")
[ "$n" -gt 0 ] || { echo "downloaded bundle has no certificates" >&2; exit 1; }
openssl crl2pkcs7 -nocrl -certfile "$tmp" | openssl pkcs7 -print_certs -noout >/dev/null
mv "$tmp" "$here/rds-global-bundle.pem"
trap - EXIT
echo "$n certificates; sha256 $(sha256sum "$here/rds-global-bundle.pem" | cut -d' ' -f1)"
