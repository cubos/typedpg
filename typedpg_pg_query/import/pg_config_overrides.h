/*
 * Portable overrides for postgres/include/pg_config.h.
 *
 * import.sh appends this file to the pg_config.h that PostgreSQL's configure
 * wrote on the import machine (x86_64 Linux, glibc): that file describes the
 * import machine, while the imported tree must compile for every target
 * typedpg ships (glibc and musl Linux on x86_64 and aarch64, macOS on x86_64
 * and aarch64, and MSVC on Windows x64).  Each block below restores what
 * PostgreSQL's own configure/meson would have found on that target; the
 * reference for the answers is PostgreSQL 19.0's meson.build (the checks are
 * the same ones configure runs) and libpg_query's hand-maintained
 * pg_config.h, whose builds already cover these platforms.
 *
 * Only defines the compiled sources and the imported headers consult are
 * listed.  Server features the crate never builds (replication, SSL, ICU,
 * NLS, ...) keep whatever configure found: no compiled file reads them.
 *
 * The file is compiled by every target, so a block that does not apply must
 * leave the earlier value alone.  Nothing here is specific to the machine
 * that ran the import.
 */

/* -------------------------------------------------------------------------
 * libc functions
 * ------------------------------------------------------------------------- */

/*
 * The tests below ask whether the libc is glibc, and glibc only declares
 * __GLIBC__/__GLIBC_MINOR__ in <features.h>, which the first system header
 * includes: c.h includes this file before any system header.  Include
 * <stdlib.h> (as libpg_query's overrides do) so the versions are visible; it
 * only brings in a header c.h includes anyway and changes no define above.
 */
#include <stdlib.h>

/*
 * strlcpy/strlcat: glibc only gained them in 2.38, so the glibc 2.35 CI
 * target lacks them; musl and macOS always had them, Windows never.
 * src/port/strlcpy.c (import/FILES) supplies strlcpy where the libc does not
 * declare it, exactly as PostgreSQL builds that file only when the function
 * is missing.  Same test as libpg_query's pg_config_overrides.h.
 *
 * HAVE_STRLCPY is configure's "the function exists" (it decides whether
 * src/port/strlcpy.c is built); HAVE_DECL_STRLCPY is "it is declared in
 * <string.h>" (port.h declares its own prototype when it is not).
 */
#undef HAVE_STRLCPY
#undef HAVE_STRLCAT
#undef HAVE_DECL_STRLCPY
#undef HAVE_DECL_STRLCAT
#if !defined(_WIN32) && \
	(defined(__APPLE__) || !defined(__GLIBC__) || \
	 (__GLIBC__ == 2 && __GLIBC_MINOR__ >= 38) || __GLIBC__ > 2)
#define HAVE_STRLCPY 1
#define HAVE_STRLCAT 1
#define HAVE_DECL_STRLCPY 1
#define HAVE_DECL_STRLCAT 1
#else
#define HAVE_DECL_STRLCPY 0
#define HAVE_DECL_STRLCAT 0
#endif

/*
 * strchrnul: glibc 2.38+, FreeBSD and NetBSD declare it.  macOS 15.4 has it
 * too, but its declaration only compiles for a high enough deployment
 * target, so use the local fallback in src/port/snprintf.c there (which
 * renames itself and is safe even when <string.h> declares the name).  musl
 * has it, but the fallback is equally correct; libpg_query keeps the same
 * conservative test.
 */
#undef HAVE_DECL_STRCHRNUL
#if defined(__FreeBSD__) || defined(__NetBSD__) || \
	(defined(__GLIBC__) && \
	 ((__GLIBC__ == 2 && __GLIBC_MINOR__ >= 38) || __GLIBC__ > 2))
#define HAVE_DECL_STRCHRNUL 1
#else
#define HAVE_DECL_STRCHRNUL 0
#endif

/*
 * strerror_r: POSIX (returns int) on macOS and on Linux without glibc (musl,
 * i.e. the linux-*-musl targets); GNU (returns char *) on glibc.  Windows
 * has no strerror_r, so src/port/strerror.c falls back to strerror() plus
 * its own copy (tl;dr: strlcpy).  Getting this wrong is a compile error with
 * clang and a wrong error message with gcc, as the first CI run showed.
 */
#undef HAVE_STRERROR_R
#undef STRERROR_R_INT
#if !defined(_WIN32)
#define HAVE_STRERROR_R 1
#if defined(__APPLE__) || (defined(__linux__) && !defined(__GLIBC__))
#define STRERROR_R_INT 1
#endif
#endif

/*
 * execinfo/backtrace: glibc and macOS have them; musl does not ship
 * execinfo.h (it lives in the separate libexecinfo package) and Windows
 * never had it.  assert.c and elog.c include it only with HAVE_EXECINFO_H
 * and use backtrace_symbols only with HAVE_BACKTRACE_SYMBOLS.
 */
#undef HAVE_EXECINFO_H
#undef HAVE_BACKTRACE_SYMBOLS
#if defined(__GLIBC__) || defined(__APPLE__)
#define HAVE_EXECINFO_H 1
#define HAVE_BACKTRACE_SYMBOLS 1
#endif

/*
 * Headers that do not exist on every target.  MSVC has no <strings.h>,
 * <syslog.h>, <termios.h> or <unistd.h>; PostgreSQL ships stand-ins for the
 * few unistd symbols its code needs under port/win32_msvc/, which build.rs
 * puts on the include path.  <xlocale.h> and <copyfile.h> are macOS-only;
 * <uchar.h> (C11, read by c.h since PostgreSQL 19) is the one macOS lacks,
 * where c.h falls back to its own char16_t/char32_t typedefs.
 */
#undef HAVE_XLOCALE_H
#undef HAVE_COPYFILE
#undef HAVE_COPYFILE_H
#undef HAVE_UCHAR_H
#undef HAVE_STRINGS_H
#undef HAVE_SYSLOG
#undef HAVE_TERMIOS_H
#undef HAVE_UNISTD_H
#ifdef __APPLE__
#define HAVE_XLOCALE_H 1
#define HAVE_COPYFILE 1
#define HAVE_COPYFILE_H 1
#endif
#ifndef __APPLE__
#define HAVE_UCHAR_H 1
#endif
#ifndef _WIN32
#define HAVE_STRINGS_H 1
#define HAVE_SYSLOG 1
#define HAVE_TERMIOS_H 1
#define HAVE_UNISTD_H 1
#endif

/*
 * Unix facilities Windows lacks and configure found on Linux.  Nothing the
 * crate compiles calls them (the few it does reach are stubbed), but
 * headers guard declarations and inline code on these, so they must describe
 * the target truthfully.
 */
#ifdef _WIN32
#undef HAVE_GETOPT
#undef HAVE_GETOPT_H
#undef HAVE_GETOPT_LONG
#undef HAVE_STRUCT_OPTION
#undef HAVE_INT_OPTERR
#undef HAVE_PTHREAD
#undef HAVE_PTHREAD_BARRIER_WAIT
#undef HAVE_PTHREAD_PRIO_INHERIT
#undef HAVE_STRSIGNAL
#undef HAVE_MKDTEMP
#undef HAVE_EXPLICIT_BZERO
#undef HAVE_INET_ATON
#undef HAVE_GETIFADDRS
#undef HAVE_IFADDRS_H
#undef HAVE_FSEEKO
#undef HAVE_STRUCT_TM_TM_ZONE
#undef HAVE_USELOCALE
#undef HAVE_GETAUXVAL
#undef HAVE_SYSLOG
#undef HAVE_LIBM
/* No pread()/preadv()/pwrite()/pwritev(): port.h uses pg_pread/pg_pwrite. */
#undef HAVE_DECL_PREADV
#define HAVE_DECL_PREADV 0
#undef HAVE_DECL_PWRITEV
#define HAVE_DECL_PWRITEV 0
/* No posix_fadvise()/strsep(); pg_config_manual.h must not enable it. */
#undef HAVE_DECL_POSIX_FADVISE
#define HAVE_DECL_POSIX_FADVISE 0
#undef HAVE_POSIX_FADVISE
#undef HAVE_DECL_STRSEP
#define HAVE_DECL_STRSEP 0
#endif

/* configure found glibc's getauxval; macOS and Windows do not have it. */
#if !defined(__GLIBC__)
#undef HAVE_GETAUXVAL
#endif

/*
 * HAVE_CRTDEFS_H: MSVC's <crtdefs.h> exists and port/win32.h includes it
 * early to rename its errcode(), so that PostgreSQL's errcode() macro does
 * not collide.
 */
#undef HAVE_CRTDEFS_H
#ifdef _WIN32
#define HAVE_CRTDEFS_H 1
#endif

/* -------------------------------------------------------------------------
 * compiler capabilities
 * ------------------------------------------------------------------------- */

/*
 * MSVC (cl.exe) has no computed goto, GNU typeof or visibility attribute,
 * no __builtin_*, no GCC-style atomics, no <cpuid.h> and no __int128.
 * PostgreSQL's meson.build reaches the same answers because its checks use
 * GNU syntax that cl rejects; libpg_query's pg_config.h has the same list.
 * MSVC does provide <intrin.h>'s __cpuid/__cpuidex, <immintrin.h>'s _xgetbv
 * and the AVX-512 intrinsics, which pg_cpu_x86.c and pg_popcount_x86.c
 * use instead, so those checks (which compile with cl) stay defined.
 */

/*
 * configure finds C23's typeof_unqual keyword when the import machine's
 * compiler defaults to C23 (GCC 15 and later), but the crate compiles the
 * tree as gnu11/c11, where the keyword does not exist, and with older
 * compilers on some targets.  Only nodes.h's copyObject() uses it, to cast
 * copyObjectImpl()'s result; without it, copyObject() returns void *.
 */
#undef HAVE_TYPEOF_UNQUAL

#ifdef _MSC_VER
#undef HAVE_COMPUTED_GOTO
#undef HAVE_TYPEOF
#undef HAVE_VISIBILITY_ATTRIBUTE
/* meson's test wraps _Static_assert in ({ ... }), which cl does not parse. */
#undef HAVE__STATIC_ASSERT
#undef HAVE_GCC__ATOMIC_INT32_CAS
#undef HAVE_GCC__ATOMIC_INT64_CAS
#undef HAVE_GCC__SYNC_CHAR_TAS
#undef HAVE_GCC__SYNC_INT32_CAS
#undef HAVE_GCC__SYNC_INT32_TAS
#undef HAVE_GCC__SYNC_INT64_CAS
#undef HAVE__BUILTIN_BSWAP16
#undef HAVE__BUILTIN_BSWAP32
#undef HAVE__BUILTIN_BSWAP64
#undef HAVE__BUILTIN_CLZ
#undef HAVE__BUILTIN_CONSTANT_P
#undef HAVE__BUILTIN_CTZ
#undef HAVE__BUILTIN_FRAME_ADDRESS
#undef HAVE__BUILTIN_OP_OVERFLOW
#undef HAVE__BUILTIN_POPCOUNT
#undef HAVE__BUILTIN_TYPES_COMPATIBLE_P
#undef HAVE__BUILTIN_UNREACHABLE
#undef HAVE__GET_CPUID
#undef HAVE__GET_CPUID_COUNT
#define HAVE__CPUID 1
#define HAVE__CPUIDEX 1
#define HAVE_XSAVE_INTRINSICS 1
/* pg_bitutils.h defines this itself for MSVC; configure's asm test fails. */
#undef HAVE_X86_64_POPCNTQ
/* cl has no __int128, so PostgreSQL's int128 support is off. */
#undef PG_INT128_TYPE
#undef ALIGNOF_PG_INT128_TYPE
/* cl accepts __restrict, but not the C99 spelling under the old default.  */
#ifndef __cplusplus
#define inline __inline
#endif
#undef restrict
#define restrict __restrict
/* A thread-local variable, spelled PostgreSQL's way on Windows. */
#define __thread __declspec(thread)
#endif /* _MSC_VER */

/* -------------------------------------------------------------------------
 * architecture
 * ------------------------------------------------------------------------- */

/*
 * x86-64-only code.  pg_cpu_x86.c's cpuid-driven feature checks require
 * <cpuid.h> (GCC/Clang on x86) or <intrin.h> (MSVC on x86), and
 * pg_popcount_x86.c uses AVX-512 only with USE_AVX512_POPCNT_WITH_RUNTIME_CHECK.
 * aarch64 uses src/port/pg_popcount_aarch64.c (Neon) instead; the CRC
 * sources those USE_*_CRC32C defines select are not imported at all, so
 * leaving them set on arm would misdescribe nothing but is still wrong.
 */
#if !defined(__x86_64__) && !defined(_M_X64) && !defined(__i386__) && !defined(_M_IX86)
#undef HAVE_X86_64_POPCNTQ
#undef HAVE__GET_CPUID
#undef HAVE__GET_CPUID_COUNT
#undef HAVE__CPUID
#undef HAVE__CPUIDEX
#undef HAVE_XSAVE_INTRINSICS
#undef USE_AVX512_POPCNT_WITH_RUNTIME_CHECK
#undef USE_AVX512_CRC32C_WITH_RUNTIME_CHECK
#undef USE_SSE42_CRC32C_WITH_RUNTIME_CHECK
#endif

/* -------------------------------------------------------------------------
 * Windows
 * ------------------------------------------------------------------------- */

#ifdef _WIN32
/*
 * Windows' long is 32 bits even on x64, and so is the MSVC/crt off_t,
 * unlike every other target, where both are 64 bits.  SIZEOF_LONG drives
 * c.h's strtoi64()/i64abs() and snprintf.c's %ld and %zu handling, so a
 * wrong value miscompiles arithmetic as well as formatting; ALIGNOF_LONG is
 * only used by LONGALIGN.
 */
#undef SIZEOF_LONG
#define SIZEOF_LONG 4
#undef ALIGNOF_LONG
#define ALIGNOF_LONG 4
#undef SIZEOF_OFF_T
#define SIZEOF_OFF_T 4

/*
 * The backend stack size PostgreSQL sets in its Windows build; stack_depth.c
 * returns it from get_stack_depth_rlimit() where POSIX uses getrlimit().
 * PostgreSQL's meson.build: WIN32_STACK_RLIMIT = 4194304, passed to the
 * linker as /STACK.
 */
#define WIN32_STACK_RLIMIT 4194304
#endif
