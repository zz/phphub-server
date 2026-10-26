/* Interface between the Rust server and the embedded PHP runtime.
 * The struct layouts here are mirrored in src/php/ffi.rs. */
#ifndef PHPHUB_H
#define PHPHUB_H

#include <stddef.h>
#include <stdint.h>

typedef struct {
  const char *ptr;
  size_t len;
} hub_str;

/* One HTTP request as PHP sees it. Every pointer stays valid until Rust is
 * told the response is finished. */
typedef struct {
  uint64_t seq;
  const char *method;       /* NUL-terminated */
  const char *uri;          /* NUL-terminated request target */
  const char *query;        /* NUL-terminated, "" when absent */
  const char *content_type; /* NUL-terminated or NULL */
  int64_t content_length;
  const char *cookie;        /* NUL-terminated or NULL */
  const char *authorization; /* NUL-terminated or NULL */
  const hub_str *headers;    /* name, value, name, value, ... */
  size_t num_headers;        /* number of pairs */
  const hub_str *vars;       /* $_SERVER key, value pairs */
  size_t num_vars;           /* number of pairs */
  double request_time;
  int proto_num; /* 1000 or 1001 */
  /* Script to execute when the request starts a run (NUL-terminated).
   * NULL means the entry script. Ignored inside a worker loop. */
  const char *script;
} hub_request;

/* Flags returned by phphub_run(). */
#define HUB_RUN_FATAL 1
#define HUB_RUN_HANDLED 2
#define HUB_RUN_LOOP_DONE 4

/* Provided by Rust. `rs` is the Rust thread state passed to
 * phphub_thread_init(). */
extern const hub_request *phphub_rs_next(void *rs);
extern void phphub_rs_detach(void *rs);
extern size_t phphub_rs_read_body(void *rs, char *buf, size_t len);
extern void phphub_rs_write(void *rs, const char *buf, size_t len);
extern void phphub_rs_send_headers(void *rs, int status, const hub_str *lines,
                                   size_t count);
extern void phphub_rs_flush(void *rs);
extern void phphub_rs_finish(void *rs, int crashed);
extern void phphub_rs_log(const char *msg, size_t len);
extern void phphub_rs_opcache_restart(int reason);

/* Provided by C, called from Rust. */
int phphub_php_startup(int threads, const char *ini_entries,
                       const char *ini_defaults, int timeout_seconds);
void phphub_php_shutdown(void);
void *phphub_thread_init(void *rs, const char *script);
void phphub_thread_free(void *ctx);
/* `once`: index/file modes. phphub_handle_request() returns true once for
 * the attached request, then false; the run never becomes a worker. */
int phphub_run(void *ctx, const hub_request *req, int once);
void phphub_kick(void *ctx, uint64_t seq);

#endif
