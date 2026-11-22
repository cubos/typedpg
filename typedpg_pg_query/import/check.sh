#!/bin/sh
# Link check: compile the imported sources, the hand-written overrides and the
# generated C into one shared object with --no-undefined.  Any symbol nothing
# defines (and libc/libm do not provide) fails the link; that is exactly the
# closure gen_stubs.sh is supposed to reach.
#
# Usage: import/check.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/.." && pwd)
pg=$crate/postgres

LC_ALL=C
export LC_ALL

work=$(mktemp -d "${TMPDIR:-/tmp}/typedpg-check.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

{
	while IFS= read -r f; do
		[ -n "$f" ] && printf '%s\n' "$pg/$f"
	done <"$here/FILES"
	find "$crate/csrc" -type f -name '*.c' | sort
	find "$pg/typedpg" -type f -name '*.c' | sort
} >"$work/sources"

# Symbols the crate's Rust side defines (src/stack.rs, src/lib.rs); this
# check has no Rust, so stand in for them.
printf '%s\n' '#include <stddef.h>' \
	'size_t typedpg_remaining_stack(void) { return 0; }' \
	'void typedpg_plpgsql_convert(void *state, const void *func) {}' \
	>"$work/rust_symbols.c"
printf '%s\n' "$work/rust_symbols.c" >>"$work/sources"

# Same flags the crate compiles with (see phase 1 build), no optimisation, so
# the symbol set is the one gen_stubs.sh saw.
CF="-std=gnu11 -D_GNU_SOURCE -fno-strict-aliasing -fwrapv -fPIC -w
	-I$pg/include -I$pg/src/backend/parser -I$pg/src/pl/plpgsql/src -I$crate/csrc"

echo "check.sh: linking $(wc -l <"$work/sources") files"
# shellcheck disable=SC2046,SC2086 # both lists are newline/space separated
gcc $CF -shared -o "$work/libpgparse.so" $(cat "$work/sources") \
	-Wl,--no-undefined -lm
echo "check.sh: ok; no undefined symbols"
