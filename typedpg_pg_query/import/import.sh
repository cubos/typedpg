#!/bin/sh
# Import PostgreSQL into typedpg_pg_query/postgres.
#
# Downloads the release named by ../POSTGRES_VERSION, verifies it against
# ../POSTGRES_SHA256, configures it, generates the headers and parsers with
# bison/flex, copies the sources listed in FILES whole (with the private and
# generated headers next to them), generates the built-in catalog tables, the
# stubs for symbols the crate does not carry and the Rust bindings of the node
# structs, and trims postgres/include/ to the headers the compiled sources
# actually use.
#
# Usage: import/import.sh   (no arguments; see README.md for prerequisites)
#
# The work directory lives outside the repository.  The output tree is
# replaced entirely, so removing a file from FILES removes it here too.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/.." && pwd)

version=$(cat "$crate/POSTGRES_VERSION")
expected=$(cat "$crate/POSTGRES_SHA256" | tr -d '[:space:]')
case "$version" in
'' | *[!0-9A-Za-z._-]*) echo "import.sh: bad POSTGRES_VERSION: $version" >&2; exit 1 ;;
esac
case "$expected" in
????????????????????????????????????????????????????????????????) ;;
*) echo "import.sh: POSTGRES_SHA256 is not a sha256: $expected" >&2; exit 1 ;;
esac

work=${TMPDIR:-/tmp}/typedpg-import-$version
src=$work/postgresql-$version
out=$crate/postgres
tarball=postgresql-$version.tar.bz2
url=https://ftp.postgresql.org/pub/source/v$version/$tarball

# Reproducible tool output (sort order, linker messages).
LC_ALL=C
export LC_ALL

say() { printf 'import.sh: %s\n' "$*"; }
fail() {
	printf 'import.sh: %s\n' "$*" >&2
	exit 1
}

for tool in bison flex perl python3 gcc make tar curl sha256sum bindgen clang; do
	command -v "$tool" >/dev/null 2>&1 || fail "missing tool: $tool"
done

# --- download and verify ---------------------------------------------------

mkdir -p "$work"
if [ -f "$work/$tarball" ] &&
	[ "$(sha256sum "$work/$tarball" | cut -d' ' -f1)" != "$expected" ]; then
	say "cached $tarball does not match POSTGRES_SHA256; downloading again"
	rm -f "$work/$tarball"
fi
if [ ! -f "$work/$tarball" ]; then
	say "downloading $url"
	curl -sSfL -o "$work/$tarball.part" "$url" || fail "downloading $url"
	mv "$work/$tarball.part" "$work/$tarball"
fi
actual=$(sha256sum "$work/$tarball" | cut -d' ' -f1)
[ "$actual" = "$expected" ] ||
	fail "sha256 mismatch for $tarball: expected $expected, got $actual"

# --- configure and generate ------------------------------------------------

say "extracting $tarball"
rm -rf "$src"
tar -xjf "$work/$tarball" -C "$work"
[ -d "$src" ] || fail "archive did not create $src"

say "configuring (no ICU, readline, compression or SSL)"
(
	cd "$src"
	./configure -q \
		--without-icu --without-readline --without-zlib --without-zstd \
		--without-lz4 --without-openssl >/dev/null
)

say "generating headers and parsers"
make -s -C "$src/src/backend" generated-headers >/dev/null
make -s -C "$src/src/backend/parser" gram.c scan.c >/dev/null
make -s -C "$src/src/common" kwlist_d.h >/dev/null
make -s -C "$src/src/pl/plpgsql/src" \
	pl_gram.c pl_gram.h plerrcodes.h \
	pl_reserved_kwlist_d.h pl_unreserved_kwlist_d.h >/dev/null

# --- check the input lists before deleting anything ------------------------

while IFS= read -r f; do
	[ -n "$f" ] || continue
	[ -f "$src/$f" ] || fail "FILES entry not found in the release: $f"
done <"$here/FILES"

# --- copy the sources ------------------------------------------------------

say "copying sources to ${out#"$crate"/}"
rm -rf "$out"
mkdir -p "$out/typedpg"
printf '%s\n' "$version" >"$out/VERSION"
cp "$src/COPYRIGHT" "$out/COPYRIGHT"

while IFS= read -r f; do
	[ -n "$f" ] || continue
	mkdir -p "$out/$(dirname "$f")"
	cp "$src/$f" "$out/$f"
done <"$here/FILES"

# Private headers (generated ones included) next to the copied sources:
# gram.h, kwlist_d.h, the nodes *.funcs.c / *.switch.c, ...
sed 's|/[^/]*$||' "$here/FILES" | sort -u >"$work/dirs"
while IFS= read -r d; do
	mkdir -p "$out/$d"
	for f in "$src/$d"/*.h "$src/$d"/*.funcs.c "$src/$d"/*.switch.c; do
		[ -f "$f" ] || continue
		cp "$f" "$out/$d/"
	done
done <"$work/dirs"

# C files the copied sources #include (levenshtein.c, ...).
while IFS= read -r f; do
	[ -n "$f" ] || continue
	d=$(dirname "$f")
	for i in $(sed -n 's/^#include "\([^"/]*\.c\)"/\1/p' "$src/$f"); do
		[ -f "$src/$d/$i" ] || fail "$f includes $i, which does not exist"
		cp "$src/$d/$i" "$out/$d/"
	done
done <"$here/FILES"

# The whole include tree for now; it is trimmed once the dependency lists are
# known (after the generated C exists).
cp -RL "$src/src/include" "$out/include"

# --- generated C -----------------------------------------------------------

say "generating built-in catalog tables"
python3 -I "$here/gen_catalog.py" "$src/src/include/catalog/postgres.bki" \
	>"$out/typedpg/builtin_catalog.c"

say "generating stubs"
sh "$here/gen_stubs.sh" >/dev/null

# --- trim include/ to the headers actually used ----------------------------

say "trimming include/"
{
	while IFS= read -r f; do
		[ -n "$f" ] && printf 'postgres/%s\n' "$f"
	done <"$here/FILES"
	find csrc/pg -type f -name '*.c' | sort
	find postgres/typedpg -type f -name '*.c' | sort
} >"$work/compile-files"

: >"$work/deps"
(
	cd "$crate"
	while IFS= read -r f; do
		gcc -MM -std=gnu11 -D_GNU_SOURCE \
			-Ipostgres/include \
			-Ipostgres/src/backend/parser \
			-Ipostgres/src/pl/plpgsql/src \
			"$f" >>"$work/deps" || fail "gcc -MM failed on $f"
	done <"$work/compile-files"
)

tr '\\' ' ' <"$work/deps" | tr ' ' '\n' |
	sed -n 's|^postgres/include/||p' | grep -v '^$' | sort -u >"$work/needed"
cat "$here/EXTRA_HEADERS" >>"$work/needed"
grep -v '^$' "$work/needed" | sort -u >"$work/needed.sorted"
mv "$work/needed.sorted" "$work/needed"

full=$work/include-full
rm -rf "$full"
mv "$out/include" "$full"
mkdir -p "$out/include"
while IFS= read -r h; do
	[ -f "$full/$h" ] || fail "header not found in the release: $h"
	mkdir -p "$out/include/$(dirname "$h")"
	cp "$full/$h" "$out/include/$h"
done <"$work/needed"
rm -rf "$full"

# --- Rust bindings of the node structs -------------------------------------

say "generating Rust bindings of the node structs"
cat >"$work/bindings.h" <<'EOF'
#include "postgres.h"
#include "nodes/parsenodes.h"
#include "nodes/primnodes.h"
#include "nodes/value.h"
#include "nodes/pg_list.h"
#include "nodes/bitmapset.h"
#include "nodes/nodes.h"
EOF

# Every node tag's C type, plus the types the tags do not name (the list and
# bitmapset plumbing, and the base Node).  Bindgen pulls in what those
# reference — the enums used by node fields, the typedef aliases — by itself.
{
	sed -n 's/^[[:space:]]*T_\([A-Za-z0-9_]*\)[[:space:]]*=[[:space:]]*[0-9]*,[[:space:]]*$/\1/p' \
		"$out/include/nodes/nodetags.h"
	printf '%s\n' Node NodeTag List ListCell Bitmapset \
		Integer Float Boolean String BitString A_Const
} | LC_ALL=C sort -u >"$work/bindings-types"
types=$(paste -sd'|' "$work/bindings-types")

bindgen "$work/bindings.h" \
	--allowlist-type "^($types)$" \
	--no-layout-tests \
	--no-doc-comments \
	--ctypes-prefix ::core::ffi \
	--use-core \
	--wrap-unsafe-ops \
	--default-enum-style consts \
	-o "$work/bindings.rs" \
	-- -I "$out/include"

# Bindgen's first line names its own version; drop it (and the blank line after
# it) so the output does not change when the tool is upgraded.
{
	printf '// @generated by import/import.sh from PostgreSQL %s headers.\n' "$version"
	printf '// Do not edit; run `import/import.sh`.\n\n'
	awk 'NR == 1 { next } NR == 2 && $0 == "" { next } { print }' "$work/bindings.rs"
} >"$out/typedpg/bindings.rs"

# --- sanity: no build-machine paths may leak into the imported tree --------

if grep -rlE '(/tmp/|/home/|/nix/store/)' "$out" >"$work/absolute"; then
	cat "$work/absolute" >&2
	fail "absolute build paths leaked into postgres/"
fi

say "done: $out ($(find "$out" -type f | wc -l) files)"
