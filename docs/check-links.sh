#!/usr/bin/env bash
# Every `docs/….md` string in the tracked tree names a file that exists.
# Run from anywhere; exit 1 and one line per dangling pointer on a miss.
set -euo pipefail
cd "$(dirname "$0")/.."

missing=0
while IFS= read -r hit; do
    file=${hit%%:*}
    rest=${hit#*:}
    line=${rest%%:*}
    path=${rest#*:}
    if [[ ! -f $path ]]; then
        echo "$file:$line: $path does not exist"
        missing=1
    fi
done < <(git ls-files -z | xargs -0 grep -IonP '(?<![A-Za-z0-9_./~-])docs/[A-Za-z0-9_./-]*\.md' || true)

if (( missing )); then
    exit 1
fi
echo "check-links: every docs/…md pointer names an existing file"
