#!/bin/sh
# Fetches the Eg-walker paper's editing traces from diamond-types, pinned
# to one commit so they never change underneath us, and checks each
# against its SHA-256. The traces aren't committed; run this to get them.
#
#   S1-S3: one person editing, sequentially
#   C1-C2: two people editing concurrently, live
#   A1-A2: asynchronous: long branches, merged later
set -eu
cd "$(dirname "$0")"
COMMIT=89ae3a0ab8d941a2885e6ec89d094bc9ce9d2922
BASE="https://raw.githubusercontent.com/josephg/diamond-types/$COMMIT/benchmark_data"
while read -r sum name; do
    if [ ! -f "$name" ]; then
        curl --fail --silent --show-error --location --output "$name.part" "$BASE/$name"
        mv "$name.part" "$name"
    fi
    echo "$sum  $name" | shasum -a 256 --check --status || {
        echo "checksum mismatch for $name" >&2
        exit 1
    }
done <<SUMS
152b5c73abf9cab85530db9f2efaac909219bd6c73d33a4fd5ab555922e253fd  S1.dt
28b09f1effc69d73a1d728919a6efa4245ecd2173bbbb414fdfe37e19c61f1ca  S2.dt
35cdba18ebd235fc426aed8f24dcce094e974864cecaa5f41beffb850542416a  S3.dt
c6b395f81e384bed9344ed43a6e283dbf1594071c9ecb32922b71183b9d375e8  C1.dt
8e1ff96afe03aa402218aca96744aeed7dc4b038098f0cd011621e7b22fbe09b  C2.dt
b0fab32a27d43b2ecdeb0f2f133c39bdc78c0fce496911323f5b4ec87d4814be  A1.dt
0dc39a102ed1c2fe653a57457dcf0e62f537594fa984e338c7441dcbff749b70  A2.dt
SUMS
echo "traces ready"
