/* src/include/pg_config.h.  Generated from pg_config.h.in by configure.  */
/* src/include/pg_config.h.in.  Generated from configure.ac by autoheader.  */

/* Define if building universal (internal helper macro) */
/* #undef AC_APPLE_UNIVERSAL_BUILD */

/* The normal alignment of `double', in bytes. */
#define ALIGNOF_DOUBLE 8

/* The normal alignment of `int', in bytes. */
#define ALIGNOF_INT 4

/* The normal alignment of `int64_t', in bytes. */
#define ALIGNOF_INT64_T 8

/* The normal alignment of `PG_INT128_TYPE', in bytes. */
#define ALIGNOF_PG_INT128_TYPE 16

/* The normal alignment of `short', in bytes. */
#define ALIGNOF_SHORT 2

/* Size of a disk block --- this also limits the size of a tuple. You can set
   it bigger if you need bigger tuples (although TOAST should reduce the need
   to have large tuples, since fields can be spread across multiple tuples).
   BLCKSZ must be a power of 2. The maximum possible value of BLCKSZ is
   currently 2^15 (32768). This is determined by the 15-bit widths of the
   lp_off and lp_len fields in ItemIdData (see include/storage/itemid.h).
   Changing BLCKSZ requires an initdb. */
#define BLCKSZ 8192

/* Saved arguments from configure */
#define CONFIGURE_ARGS " '--without-icu' '--without-readline' '--without-zlib' '--without-zstd' '--without-lz4' '--without-openssl' 'PKG_CONFIG_PATH=/etc/profiles/per-user/guilherme/lib/pkgconfig'"

/* Define to the default TCP port number on which the server listens and to
   which clients will try to connect. This can be overridden at run-time, but
   it's convenient if your clients have the right default compiled in.
   (--with-pgport=PORTNUM) */
#define DEF_PGPORT 5432

/* Define to the default TCP port number as a string constant. */
#define DEF_PGPORT_STR "5432"

/* Define to the file name extension of dynamically-loadable modules. */
#define DLSUFFIX ".so"

/* Define to build with GSSAPI support. (--with-gssapi) */
/* #undef ENABLE_GSS */

/* Define to 1 if you want National Language Support. (--enable-nls) */
/* #undef ENABLE_NLS */

/* Define to 1 if you have the `append_history' function. */
/* #undef HAVE_APPEND_HISTORY */

/* Define to 1 if you have the `backtrace_symbols' function. */
#define HAVE_BACKTRACE_SYMBOLS 1

/* Define to 1 if your compiler handles computed gotos. */
#define HAVE_COMPUTED_GOTO 1

/* Define to 1 if you have the `copyfile' function. */
/* #undef HAVE_COPYFILE */

/* Define to 1 if you have the <copyfile.h> header file. */
/* #undef HAVE_COPYFILE_H */

/* Define to 1 if you have the `copy_file_range' function. */
#define HAVE_COPY_FILE_RANGE 1

/* Define to 1 if you have the <crtdefs.h> header file. */
/* #undef HAVE_CRTDEFS_H */

/* Define to 1 if your C++ compiler understands `typeof' or something similar.
   */
#define HAVE_CXX_TYPEOF 1

/* Define to 1 if your C++ compiler understands `typeof_unqual' or something
   similar. */
/* #undef HAVE_CXX_TYPEOF_UNQUAL */

/* Define to 1 if you have the declaration of `fdatasync', and to 0 if you
   don't. */
#define HAVE_DECL_FDATASYNC 1

/* Define to 1 if you have the declaration of `F_FULLFSYNC', and to 0 if you
   don't. */
#define HAVE_DECL_F_FULLFSYNC 0

/* Define to 1 if you have the declaration of `memset_s', and to 0 if you
   don't. */
#define HAVE_DECL_MEMSET_S 0

/* Define to 1 if you have the declaration of `posix_fadvise', and to 0 if you
   don't. */
#define HAVE_DECL_POSIX_FADVISE 1

/* Define to 1 if you have the declaration of `preadv', and to 0 if you don't.
   */
#define HAVE_DECL_PREADV 1

/* Define to 1 if you have the declaration of `pwritev', and to 0 if you
   don't. */
#define HAVE_DECL_PWRITEV 1

/* Define to 1 if you have the declaration of `strchrnul', and to 0 if you
   don't. */
#define HAVE_DECL_STRCHRNUL 1

/* Define to 1 if you have the declaration of `strlcat', and to 0 if you
   don't. */
#define HAVE_DECL_STRLCAT 1

/* Define to 1 if you have the declaration of `strlcpy', and to 0 if you
   don't. */
#define HAVE_DECL_STRLCPY 1

/* Define to 1 if you have the declaration of `strsep', and to 0 if you don't.
   */
#define HAVE_DECL_STRSEP 1

/* Define to 1 if you have the declaration of `timingsafe_bcmp', and to 0 if
   you don't. */
#define HAVE_DECL_TIMINGSAFE_BCMP 0

/* Define to 1 if you have the <editline/history.h> header file. */
/* #undef HAVE_EDITLINE_HISTORY_H */

/* Define to 1 if you have the <editline/readline.h> header file. */
/* #undef HAVE_EDITLINE_READLINE_H */

/* Define to 1 if you have the `elf_aux_info' function. */
/* #undef HAVE_ELF_AUX_INFO */

/* Define to 1 if you have the <execinfo.h> header file. */
#define HAVE_EXECINFO_H 1

/* Define to 1 if you have the `explicit_bzero' function. */
#define HAVE_EXPLICIT_BZERO 1

/* Define to 1 if you have the `explicit_memset' function. */
/* #undef HAVE_EXPLICIT_MEMSET */

/* Define to 1 if fseeko (and presumably ftello) exists and is declared. */
#define HAVE_FSEEKO 1

/* Define to 1 if you have __atomic_compare_exchange_n(int *, int *, int). */
#define HAVE_GCC__ATOMIC_INT32_CAS 1

/* Define to 1 if you have __atomic_compare_exchange_n(int64 *, int64 *,
   int64). */
#define HAVE_GCC__ATOMIC_INT64_CAS 1

/* Define to 1 if you have __sync_lock_test_and_set(char *) and friends. */
#define HAVE_GCC__SYNC_CHAR_TAS 1

/* Define to 1 if you have __sync_val_compare_and_swap(int *, int, int). */
#define HAVE_GCC__SYNC_INT32_CAS 1

/* Define to 1 if you have __sync_lock_test_and_set(int *) and friends. */
#define HAVE_GCC__SYNC_INT32_TAS 1

/* Define to 1 if you have __sync_val_compare_and_swap(int64_t *, int64_t,
   int64_t). */
#define HAVE_GCC__SYNC_INT64_CAS 1

/* Define to 1 if you have the `getauxval' function. */
#define HAVE_GETAUXVAL 1

/* Define to 1 if you have the `getifaddrs' function. */
#define HAVE_GETIFADDRS 1

/* Define to 1 if you have the `getopt' function. */
#define HAVE_GETOPT 1

/* Define to 1 if you have the <getopt.h> header file. */
#define HAVE_GETOPT_H 1

/* Define to 1 if you have the `getopt_long' function. */
#define HAVE_GETOPT_LONG 1

/* Define to 1 if you have the `getpeereid' function. */
/* #undef HAVE_GETPEEREID */

/* Define to 1 if you have the `getpeerucred' function. */
/* #undef HAVE_GETPEERUCRED */

/* Define to 1 if you have the <gssapi_ext.h> header file. */
/* #undef HAVE_GSSAPI_EXT_H */

/* Define to 1 if you have the <gssapi/gssapi_ext.h> header file. */
/* #undef HAVE_GSSAPI_GSSAPI_EXT_H */

/* Define to 1 if you have the <gssapi/gssapi.h> header file. */
/* #undef HAVE_GSSAPI_GSSAPI_H */

/* Define to 1 if you have the <gssapi.h> header file. */
/* #undef HAVE_GSSAPI_H */

/* Define to 1 if you have the <history.h> header file. */
/* #undef HAVE_HISTORY_H */

/* Define to 1 if you have the `history_truncate_file' function. */
/* #undef HAVE_HISTORY_TRUNCATE_FILE */

/* Define to 1 if you have the <ifaddrs.h> header file. */
#define HAVE_IFADDRS_H 1

/* Define to 1 if you have the `inet_aton' function. */
#define HAVE_INET_ATON 1

/* Define to 1 if you have the `inet_pton' function. */
#define HAVE_INET_PTON 1

/* Define to 1 if you have the <inttypes.h> header file. */
#define HAVE_INTTYPES_H 1

/* Define to 1 if you have the global variable 'int opterr'. */
#define HAVE_INT_OPTERR 1

/* Define to 1 if you have the global variable 'int optreset'. */
/* #undef HAVE_INT_OPTRESET */

/* Define to 1 if you have the global variable 'int timezone'. */
#define HAVE_INT_TIMEZONE 1

/* Define to 1 if you have the `io_uring_queue_init_mem' function. */
/* #undef HAVE_IO_URING_QUEUE_INIT_MEM */

/* Define to 1 if __builtin_constant_p(x) implies "i"(x) acceptance. */
/* #undef HAVE_I_CONSTRAINT__BUILTIN_CONSTANT_P */

/* Define to 1 if you have the `kqueue' function. */
/* #undef HAVE_KQUEUE */

/* Define to 1 if you have the `ldap_initialize' function. */
/* #undef HAVE_LDAP_INITIALIZE */

/* Define to 1 if you have the `crypto' library (-lcrypto). */
/* #undef HAVE_LIBCRYPTO */

/* Define to 1 if you have the `curl' library (-lcurl). */
/* #undef HAVE_LIBCURL */

/* Define to 1 if you have the `ldap' library (-lldap). */
/* #undef HAVE_LIBLDAP */

/* Define to 1 if you have the `lz4' library (-llz4). */
/* #undef HAVE_LIBLZ4 */

/* Define to 1 if you have the `m' library (-lm). */
#define HAVE_LIBM 1

/* Define to 1 if you have the `numa' library (-lnuma). */
/* #undef HAVE_LIBNUMA */

/* Define to 1 if you have the `pam' library (-lpam). */
/* #undef HAVE_LIBPAM */

/* Define if you have a function readline library */
/* #undef HAVE_LIBREADLINE */

/* Define to 1 if you have the `selinux' library (-lselinux). */
/* #undef HAVE_LIBSELINUX */

/* Define to 1 if you have the `ssl' library (-lssl). */
/* #undef HAVE_LIBSSL */

/* Define to 1 if you have the `wldap32' library (-lwldap32). */
/* #undef HAVE_LIBWLDAP32 */

/* Define to 1 if you have the `xml2' library (-lxml2). */
/* #undef HAVE_LIBXML2 */

/* Define to 1 if you have the `xslt' library (-lxslt). */
/* #undef HAVE_LIBXSLT */

/* Define to 1 if you have the `z' library (-lz). */
/* #undef HAVE_LIBZ */

/* Define to 1 if you have the `zstd' library (-lzstd). */
/* #undef HAVE_LIBZSTD */

/* Define to 1 if you have the `localeconv_l' function. */
/* #undef HAVE_LOCALECONV_L */

/* Define to 1 if you have the `mbstowcs_l' function. */
/* #undef HAVE_MBSTOWCS_L */

/* Define to 1 if you have the <memory.h> header file. */
#define HAVE_MEMORY_H 1

/* Define to 1 if you have the `memset_explicit' function. */
#define HAVE_MEMSET_EXPLICIT 1

/* Define to 1 if you have the `mkdtemp' function. */
#define HAVE_MKDTEMP 1

/* Define to 1 if you have the <ossp/uuid.h> header file. */
/* #undef HAVE_OSSP_UUID_H */

/* Define to 1 if you have the <pam/pam_appl.h> header file. */
/* #undef HAVE_PAM_PAM_APPL_H */

/* Define to 1 if you have the `posix_fadvise' function. */
#define HAVE_POSIX_FADVISE 1

/* Define to 1 if you have the `posix_fallocate' function. */
#define HAVE_POSIX_FALLOCATE 1

/* Define to 1 if you have the `ppoll' function. */
#define HAVE_PPOLL 1

/* Define if you have POSIX threads libraries and header files. */
#define HAVE_PTHREAD 1

/* Define to 1 if you have the `pthread_barrier_wait' function. */
#define HAVE_PTHREAD_BARRIER_WAIT 1

/* Define to 1 if you have the `pthread_is_threaded_np' function. */
/* #undef HAVE_PTHREAD_IS_THREADED_NP */

/* Have PTHREAD_PRIO_INHERIT. */
#define HAVE_PTHREAD_PRIO_INHERIT 1

/* Define to 1 if you have the <readline.h> header file. */
/* #undef HAVE_READLINE_H */

/* Define to 1 if you have the <readline/history.h> header file. */
/* #undef HAVE_READLINE_HISTORY_H */

/* Define to 1 if you have the <readline/readline.h> header file. */
/* #undef HAVE_READLINE_READLINE_H */

/* Define to 1 if you have the `rl_completion_matches' function. */
/* #undef HAVE_RL_COMPLETION_MATCHES */

/* Define to 1 if you have the global variable 'rl_completion_suppress_quote'.
   */
/* #undef HAVE_RL_COMPLETION_SUPPRESS_QUOTE */

/* Define to 1 if you have the `rl_filename_completion_function' function. */
/* #undef HAVE_RL_FILENAME_COMPLETION_FUNCTION */

/* Define to 1 if you have the global variable 'rl_filename_quote_characters'.
   */
/* #undef HAVE_RL_FILENAME_QUOTE_CHARACTERS */

/* Define to 1 if you have the global variable 'rl_filename_quoting_function'.
   */
/* #undef HAVE_RL_FILENAME_QUOTING_FUNCTION */

/* Define to 1 if you have the `rl_reset_screen_size' function. */
/* #undef HAVE_RL_RESET_SCREEN_SIZE */

/* Define to 1 if you have the `rl_variable_bind' function. */
/* #undef HAVE_RL_VARIABLE_BIND */

/* Define to 1 if you have SA_SIGINFO available. */
#define HAVE_SA_SIGINFO 1

/* Define to 1 if you have the <security/pam_appl.h> header file. */
/* #undef HAVE_SECURITY_PAM_APPL_H */

/* Define to 1 if you have the `setproctitle' function. */
/* #undef HAVE_SETPROCTITLE */

/* Define to 1 if you have the `setproctitle_fast' function. */
/* #undef HAVE_SETPROCTITLE_FAST */

/* Define to 1 if the system has the type `socklen_t'. */
#define HAVE_SOCKLEN_T 1

/* Define to 1 if you have the `SSL_CTX_set_cert_cb' function. */
/* #undef HAVE_SSL_CTX_SET_CERT_CB */

/* Define to 1 if you have the `SSL_CTX_set_ciphersuites' function. */
/* #undef HAVE_SSL_CTX_SET_CIPHERSUITES */

/* Define to 1 if you have the `SSL_CTX_set_client_hello_cb' function. */
/* #undef HAVE_SSL_CTX_SET_CLIENT_HELLO_CB */

/* Define to 1 if you have the `SSL_CTX_set_keylog_callback' function. */
/* #undef HAVE_SSL_CTX_SET_KEYLOG_CALLBACK */

/* Define to 1 if you have the `SSL_CTX_set_num_tickets' function. */
/* #undef HAVE_SSL_CTX_SET_NUM_TICKETS */

/* Define to 1 if you have the <stdint.h> header file. */
#define HAVE_STDINT_H 1

/* Define to 1 if you have the <stdlib.h> header file. */
#define HAVE_STDLIB_H 1

/* Define to 1 if you have the `strerror_r' function. */
#define HAVE_STRERROR_R 1

/* Define to 1 if you have the <strings.h> header file. */
#define HAVE_STRINGS_H 1

/* Define to 1 if you have the <string.h> header file. */
#define HAVE_STRING_H 1

/* Define to 1 if you have the `strlcat' function. */
#define HAVE_STRLCAT 1

/* Define to 1 if you have the `strlcpy' function. */
#define HAVE_STRLCPY 1

/* Define to 1 if you have the `strsep' function. */
#define HAVE_STRSEP 1

/* Define to 1 if you have the `strsignal' function. */
#define HAVE_STRSIGNAL 1

/* Define to 1 if the system has the type `struct option'. */
#define HAVE_STRUCT_OPTION 1

/* Define to 1 if `sa_len' is a member of `struct sockaddr'. */
/* #undef HAVE_STRUCT_SOCKADDR_SA_LEN */

/* Define to 1 if `tm_zone' is a member of `struct tm'. */
#define HAVE_STRUCT_TM_TM_ZONE 1

/* Define to 1 if you have the `syncfs' function. */
#define HAVE_SYNCFS 1

/* Define to 1 if you have the `sync_file_range' function. */
#define HAVE_SYNC_FILE_RANGE 1

/* Define to 1 if you have the syslog interface. */
#define HAVE_SYSLOG 1

/* Define to 1 if you have the <sys/epoll.h> header file. */
#define HAVE_SYS_EPOLL_H 1

/* Define to 1 if you have the <sys/event.h> header file. */
/* #undef HAVE_SYS_EVENT_H */

/* Define to 1 if you have the <sys/personality.h> header file. */
#define HAVE_SYS_PERSONALITY_H 1

/* Define to 1 if you have the <sys/prctl.h> header file. */
#define HAVE_SYS_PRCTL_H 1

/* Define to 1 if you have the <sys/procctl.h> header file. */
/* #undef HAVE_SYS_PROCCTL_H */

/* Define to 1 if you have the <sys/signalfd.h> header file. */
#define HAVE_SYS_SIGNALFD_H 1

/* Define to 1 if you have the <sys/stat.h> header file. */
#define HAVE_SYS_STAT_H 1

/* Define to 1 if you have the <sys/types.h> header file. */
#define HAVE_SYS_TYPES_H 1

/* Define to 1 if you have the <sys/ucred.h> header file. */
/* #undef HAVE_SYS_UCRED_H */

/* Define to 1 if you have the <termios.h> header file. */
#define HAVE_TERMIOS_H 1

/* Define to 1 if curl_global_init() is guaranteed to be thread-safe. */
/* #undef HAVE_THREADSAFE_CURL_GLOBAL_INIT */

/* Define to 1 if you have the `timingsafe_bcmp' function. */
/* #undef HAVE_TIMINGSAFE_BCMP */

/* Define to 1 if your compiler understands `typeof' or something similar. */
#define HAVE_TYPEOF 1

/* Define to 1 if your compiler understands `typeof_unqual' or something
   similar. */
#define HAVE_TYPEOF_UNQUAL 1

/* Define to 1 if you have the <uchar.h> header file. */
#define HAVE_UCHAR_H 1

/* Define to 1 if you have the <ucred.h> header file. */
/* #undef HAVE_UCRED_H */

/* Define to 1 if the system has the type `union semun'. */
/* #undef HAVE_UNION_SEMUN */

/* Define to 1 if you have the <unistd.h> header file. */
#define HAVE_UNISTD_H 1

/* Define to 1 if you have the `uselocale' function. */
#define HAVE_USELOCALE 1

/* Define to 1 if you have BSD UUID support. */
/* #undef HAVE_UUID_BSD */

/* Define to 1 if you have E2FS UUID support. */
/* #undef HAVE_UUID_E2FS */

/* Define to 1 if you have the <uuid.h> header file. */
/* #undef HAVE_UUID_H */

/* Define to 1 if you have OSSP UUID support. */
/* #undef HAVE_UUID_OSSP */

/* Define to 1 if you have the <uuid/uuid.h> header file. */
/* #undef HAVE_UUID_UUID_H */

/* Define to 1 if your compiler knows the visibility("hidden") attribute. */
#define HAVE_VISIBILITY_ATTRIBUTE 1

/* Define to 1 if you have the `wcstombs_l' function. */
/* #undef HAVE_WCSTOMBS_L */

/* Define to 1 if you have the `X509_get_signature_info' function. */
/* #undef HAVE_X509_GET_SIGNATURE_INFO */

/* Define to 1 if the assembler supports X86_64's POPCNTQ instruction. */
#define HAVE_X86_64_POPCNTQ 1

/* Define to 1 if you have the <xlocale.h> header file. */
/* #undef HAVE_XLOCALE_H */

/* Define to 1 if you have XSAVE intrinsics. */
#define HAVE_XSAVE_INTRINSICS 1

/* Define to 1 if your compiler understands __builtin_bswap16. */
#define HAVE__BUILTIN_BSWAP16 1

/* Define to 1 if your compiler understands __builtin_bswap32. */
#define HAVE__BUILTIN_BSWAP32 1

/* Define to 1 if your compiler understands __builtin_bswap64. */
#define HAVE__BUILTIN_BSWAP64 1

/* Define to 1 if your compiler understands __builtin_clz. */
#define HAVE__BUILTIN_CLZ 1

/* Define to 1 if your compiler understands __builtin_constant_p. */
#define HAVE__BUILTIN_CONSTANT_P 1

/* Define to 1 if your compiler understands __builtin_ctz. */
#define HAVE__BUILTIN_CTZ 1

/* Define to 1 if your compiler understands __builtin_frame_address. */
#define HAVE__BUILTIN_FRAME_ADDRESS 1

/* Define to 1 if your compiler understands __builtin_$op_overflow. */
#define HAVE__BUILTIN_OP_OVERFLOW 1

/* Define to 1 if your compiler understands __builtin_types_compatible_p. */
#define HAVE__BUILTIN_TYPES_COMPATIBLE_P 1

/* Define to 1 if your compiler understands __builtin_unreachable. */
#define HAVE__BUILTIN_UNREACHABLE 1

/* Define to 1 if you have __cpuid. */
/* #undef HAVE__CPUID */

/* Define to 1 if you have __cpuidex. */
#define HAVE__CPUIDEX 1

/* Define to 1 if you have __get_cpuid. */
#define HAVE__GET_CPUID 1

/* Define to 1 if you have __get_cpuid_count. */
#define HAVE__GET_CPUID_COUNT 1

/* Define as the maximum alignment requirement of any C data type. */
#define MAXIMUM_ALIGNOF 8

/* Define bytes to use libc memset(). */
#define MEMSET_LOOP_LIMIT 1024

/* Define to the OpenSSL API version in use. This avoids deprecation warnings
   from newer OpenSSL versions. */
/* #undef OPENSSL_API_COMPAT */

/* Define to the address where bug reports for this package should be sent. */
#define PACKAGE_BUGREPORT "pgsql-bugs@lists.postgresql.org"

/* Define to the full name of this package. */
#define PACKAGE_NAME "PostgreSQL"

/* Define to the full name and version of this package. */
#define PACKAGE_STRING "PostgreSQL 19.0"

/* Define to the one symbol short name of this package. */
#define PACKAGE_TARNAME "postgresql"

/* Define to the home page for this package. */
#define PACKAGE_URL "https://www.postgresql.org/"

/* Define to the version of this package. */
#define PACKAGE_VERSION "19.0"

/* Define to best C++ printf format archetype, usually gnu_printf if
   available. */
#define PG_CXX_PRINTF_ATTRIBUTE gnu_printf

/* Define to best C printf format archetype, usually gnu_printf if available.
   */
#define PG_C_PRINTF_ATTRIBUTE gnu_printf

/* Define to the name of a signed 128-bit integer type. */
#define PG_INT128_TYPE __int128

/* Define to the name of the default PostgreSQL service principal in Kerberos
   (GSSAPI). (--with-krb-srvnam=NAME) */
#define PG_KRB_SRVNAM "postgres"

/* PostgreSQL major version as a string */
#define PG_MAJORVERSION "19"

/* PostgreSQL major version number */
#define PG_MAJORVERSION_NUM 19

/* PostgreSQL minor version number */
#define PG_MINORVERSION_NUM 0

/* PostgreSQL version as a string */
#define PG_VERSION "19.0"

/* PostgreSQL version as a number */
#define PG_VERSION_NUM 190000

/* A string containing the version number, platform, and C compiler */
#define PG_VERSION_STR "PostgreSQL 19.0 on x86_64-pc-linux-gnu, compiled by gcc (GCC) 16.2.0, 64-bit"

/* Define to 1 to allow profiling output to be saved separately for each
   process. */
/* #undef PROFILE_PID_DIR */

/* Define to necessary symbol if this constant uses a non-standard name on
   your system. */
/* #undef PTHREAD_CREATE_JOINABLE */

/* RELSEG_SIZE is the maximum number of blocks allowed in one disk file. Thus,
   the maximum size of a single file is RELSEG_SIZE * BLCKSZ; relations bigger
   than that are divided into multiple files. RELSEG_SIZE * BLCKSZ must be
   less than your OS' limit on file size. This is often 2 GB or 4GB in a
   32-bit operating system, unless you have large file support enabled. By
   default, we make the limit 1 GB to avoid any possible integer-overflow
   problems within the OS. A limit smaller than necessary only means we divide
   a large relation into more chunks than necessary, so it seems best to err
   in the direction of a small limit. A power-of-2 value is recommended to
   save a few cycles in md.c, but is not absolutely required. Changing
   RELSEG_SIZE requires an initdb. */
#define RELSEG_SIZE 131072

/* The size of `intmax_t', as computed by sizeof. */
#define SIZEOF_INTMAX_T 8

/* The size of `long', as computed by sizeof. */
#define SIZEOF_LONG 8

/* The size of `long long', as computed by sizeof. */
#define SIZEOF_LONG_LONG 8

/* The size of `off_t', as computed by sizeof. */
#define SIZEOF_OFF_T 8

/* The size of `size_t', as computed by sizeof. */
#define SIZEOF_SIZE_T 8

/* The size of `void *', as computed by sizeof. */
#define SIZEOF_VOID_P 8

/* Define to 1 if you have the ANSI C header files. */
#define STDC_HEADERS 1

/* Define to 1 if strerror_r() returns int. */
/* #undef STRERROR_R_INT */

/* Define to 1 to use ARMv8 CRC Extension. */
/* #undef USE_ARMV8_CRC32C */

/* Define to 1 to use ARMv8 CRC Extension with a runtime check. */
/* #undef USE_ARMV8_CRC32C_WITH_RUNTIME_CHECK */

/* Define to 1 to build with assertion checks. (--enable-cassert) */
/* #undef USE_ASSERT_CHECKING */

/* Define to 1 to use AVX2 instructions with a runtime check. */
#define USE_AVX2_WITH_RUNTIME_CHECK 1

/* Define to 1 to use AVX-512 CRC algorithms with a runtime check. */
#define USE_AVX512_CRC32C_WITH_RUNTIME_CHECK 1

/* Define to 1 to use AVX-512 popcount instructions with a runtime check. */
#define USE_AVX512_POPCNT_WITH_RUNTIME_CHECK 1

/* Define to 1 to build with Bonjour support. (--with-bonjour) */
/* #undef USE_BONJOUR */

/* Define to 1 to build with BSD Authentication support. (--with-bsd-auth) */
/* #undef USE_BSD_AUTH */

/* Define to build with ICU support. (--with-icu) */
/* #undef USE_ICU */

/* Define to 1 to build with injection points. (--enable-injection-points) */
/* #undef USE_INJECTION_POINTS */

/* Define to 1 to build with LDAP support. (--with-ldap) */
/* #undef USE_LDAP */

/* Define to 1 to build with libcurl support. (--with-libcurl) */
/* #undef USE_LIBCURL */

/* Define to build with NUMA support. (--with-libnuma) */
/* #undef USE_LIBNUMA */

/* Define to build with io_uring support. (--with-liburing) */
/* #undef USE_LIBURING */

/* Define to 1 to build with XML support. (--with-libxml) */
/* #undef USE_LIBXML */

/* Define to 1 to use XSLT support when building contrib/xml2.
   (--with-libxslt) */
/* #undef USE_LIBXSLT */

/* Define to 1 to build with LLVM based JIT support. (--with-llvm) */
/* #undef USE_LLVM */

/* Define to 1 to use LoongArch CRCC instructions. */
/* #undef USE_LOONGARCH_CRC32C */

/* Define to 1 to build with LZ4 support. (--with-lz4) */
/* #undef USE_LZ4 */

/* Define to select named POSIX semaphores. */
/* #undef USE_NAMED_POSIX_SEMAPHORES */

/* Define to 1 to build with OpenSSL support. (--with-ssl=openssl) */
/* #undef USE_OPENSSL */

/* Define to 1 to build with PAM support. (--with-pam) */
/* #undef USE_PAM */

/* Define to 1 to use Arm PMULL CRC algorithms with a runtime check. */
/* #undef USE_PMULL_CRC32C_WITH_RUNTIME_CHECK */

/* Define to 1 to use software CRC-32C implementation (slicing-by-8). */
/* #undef USE_SLICING_BY_8_CRC32C */

/* Define to 1 use Intel SSE 4.2 CRC instructions. */
/* #undef USE_SSE42_CRC32C */

/* Define to 1 to use Intel SSE 4.2 CRC instructions with a runtime check. */
#define USE_SSE42_CRC32C_WITH_RUNTIME_CHECK 1

/* Define to 1 to use SVE popcount instructions with a runtime check. */
/* #undef USE_SVE_POPCNT_WITH_RUNTIME_CHECK */

/* Define to build with systemd support. (--with-systemd) */
/* #undef USE_SYSTEMD */

/* Define to select SysV-style semaphores. */
/* #undef USE_SYSV_SEMAPHORES */

/* Define to select SysV-style shared memory. */
#define USE_SYSV_SHARED_MEMORY 1

/* Define to select unnamed POSIX semaphores. */
#define USE_UNNAMED_POSIX_SEMAPHORES 1

/* Define to select Win32-style semaphores. */
/* #undef USE_WIN32_SEMAPHORES */

/* Define to select Win32-style shared memory. */
/* #undef USE_WIN32_SHARED_MEMORY */

/* Define to 1 to build with ZSTD support. (--with-zstd) */
/* #undef USE_ZSTD */

/* Define WORDS_BIGENDIAN to 1 if your processor stores words with the most
   significant byte first (like Motorola and SPARC, unlike Intel). */
#if defined AC_APPLE_UNIVERSAL_BUILD
# if defined __BIG_ENDIAN__
#  define WORDS_BIGENDIAN 1
# endif
#else
# ifndef WORDS_BIGENDIAN
/* #  undef WORDS_BIGENDIAN */
# endif
#endif

/* Size of a WAL file block. This need have no particular relation to BLCKSZ.
   XLOG_BLCKSZ must be a power of 2, and if your system supports O_DIRECT I/O,
   XLOG_BLCKSZ must be a multiple of the alignment requirement for direct-I/O
   buffers, else direct I/O may fail. Changing XLOG_BLCKSZ requires an initdb.
   */
#define XLOG_BLCKSZ 8192



/* Number of bits in a file offset, on hosts where this is settable. */
/* #undef _FILE_OFFSET_BITS */

/* Define to 1 to make fseeko visible on some hosts (e.g. glibc 2.2). */
/* #undef _LARGEFILE_SOURCE */

/* Define for large files, on AIX-style hosts. */
/* #undef _LARGE_FILES */

/* Define to how the C++ compiler spells `typeof'. */
/* #undef pg_cxx_typeof */

/* Define to how the C++ compiler spells `typeof_unqual'. */
/* #undef pg_cxx_typeof_unqual */

/* Define to keyword to use for C99 restrict support, or to nothing if not
   supported */
#define pg_restrict __restrict

/* Define to the equivalent of the C99 'restrict' keyword, or to
   nothing if this is not supported.  Do not define if restrict is
   supported directly.  */
#define restrict __restrict
/* Work around a bug in Sun C++: it does not support _Restrict or
   __restrict__, even though the corresponding Sun C compiler ends up with
   "#define restrict _Restrict" or "#define restrict __restrict__" in the
   previous line.  Perhaps some future version of Sun C++ will work with
   restrict; if so, hopefully it defines __RESTRICT like Sun C does.  */
#if defined __SUNPRO_CC && !defined __RESTRICT
# define _Restrict
# define __restrict__
#endif

/* Define to how the compiler spells `typeof'. */
/* #undef typeof */

/* Define to how the compiler spells `typeof_unqual'. */
/* #undef typeof_unqual */
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
