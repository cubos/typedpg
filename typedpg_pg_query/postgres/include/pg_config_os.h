/*
 * src/include/pg_config_os.h for typedpg's imported tree.
 *
 * PostgreSQL's configure writes this file with the contents of the import
 * machine's src/include/port/<platform>.h (linux.h here).  The imported tree
 * must build for several platforms, so import.sh replaces that copy with this
 * dispatcher; the three files it includes are imported verbatim from the
 * release (import/EXTRA_HEADERS).
 *
 * c.h includes this file before every system header, which is why the
 * per-platform file must come first: port/win32.h sets _WIN32_WINNT, includes
 * <crtdefs.h> before its errcode() can collide with PostgreSQL's, and defines
 * WIN32, which the rest of the tree tests.
 */

#if defined(_WIN32)
#include "port/win32.h"

/*
 * typedpg compiles PostgreSQL's sources into a static library inside the
 * Rust binary, not into a DLL, so PG's own declarations must not become
 * __declspec(dllimport): a dllimport variable would be referenced through an
 * __imp_* slot that no import library provides, and the stubs would not link.
 * libpg_query's pg_config_os.h does the same for its Windows build.
 */
#undef PGDLLIMPORT
#undef PGDLLEXPORT

#elif defined(__APPLE__)
#include "port/darwin.h"

#else
#include "port/linux.h"
#endif
