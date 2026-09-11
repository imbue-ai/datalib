#!/usr/bin/env bash
# End-to-end check of `latchkey-curl-dispatch`'s exec path, which the
# Rust unit tests cannot reach: that a marked invocation execs the
# `latchkey-curl-impersonate` sibling with the impersonation flags in
# front and our headers stripped, and that an unmarked one execs the
# `curl` on PATH untouched. Both targets are fakes that print their argv.
set -euo pipefail

dispatch_src="$1"
dir="${TEST_TMPDIR:?}/bin"
mkdir -p "$dir"
cp "$dispatch_src" "$dir/latchkey-curl-dispatch"

# The dispatch resolves its impersonator as a sibling of its own
# canonical path, so the fake has to sit next to the copied binary.
cat > "$dir/latchkey-curl-impersonate" <<'FAKE'
#!/bin/sh
echo "impersonator"
printf '%s\n' "$@"
FAKE
cat > "$dir/curl" <<'FAKE'
#!/bin/sh
echo "system-curl"
printf '%s\n' "$@"
FAKE
chmod +x "$dir"/*

fail() {
    echo "FAIL: $1" >&2
    echo "--- got ---" >&2
    printf '%s\n' "$2" >&2
    echo "--- expected ---" >&2
    printf '%s\n' "$3" >&2
    exit 1
}

got="$(DATALIB_IMPERSONATE_PROFILE=chrome131 "$dir/latchkey-curl-dispatch" \
    -sS -H 'User-Agent: curl/8.7.1' -H 'X-Imbue-Impersonate: 1' -H 'Accept: */*' https://example.com/)"
expected="$(printf '%s\n' impersonator --compressed --noproxy '*' --impersonate chrome131 -sS -H 'Accept: */*' https://example.com/)"
[[ "$got" == "$expected" ]] || fail "marked invocation, explicit profile" "$got" "$expected"

got="$(env -u DATALIB_IMPERSONATE_PROFILE "$dir/latchkey-curl-dispatch" -H 'X-Imbue-Impersonate;' https://example.com/)"
expected="$(printf '%s\n' impersonator --compressed --noproxy '*' --impersonate chrome150 https://example.com/)"
[[ "$got" == "$expected" ]] || fail "marked invocation, default profile" "$got" "$expected"

got="$(PATH="$dir:$PATH" "$dir/latchkey-curl-dispatch" -sS -H 'User-Agent: curl/8.7.1' https://example.com/)"
expected="$(printf '%s\n' system-curl -sS -H 'User-Agent: curl/8.7.1' https://example.com/)"
[[ "$got" == "$expected" ]] || fail "unmarked invocation" "$got" "$expected"

echo "PASS"
