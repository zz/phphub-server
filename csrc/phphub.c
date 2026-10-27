/* Embedded ZTS PHP for PHPHub.
 *
 * Each Rust PHP thread owns one TSRM context and runs the worker script
 * with phphub_run(). A worker script loops on phphub_handle_request(); a
 * script that never calls it is run once per request (classic mode).
 *
 * The per-request startup and shutdown used inside the worker loop follow
 * FrankenPHP's worker mode (frankenphp.c), which in turn is adapted from
 * php_request_startup() and php_request_shutdown(). */

#include "phphub.h"

#include <SAPI.h>
#include <Zend/zend_atomic.h>
#include <Zend/zend_exceptions.h>
#include <ext/session/php_session.h>
#include <ext/standard/head.h>
#include <php.h>
#include <php_config.h>
#include <php_ini.h>
#include <php_main.h>
#include <php_output.h>
#include <php_variables.h>
#include <rfc1867.h>

#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern char **environ;

typedef struct {
  void *rs;
  const char *script;
  /* Request attached to the SAPI: source of $_SERVER, body and cookies. */
  const hub_request *req;
  /* Output and headers go to Rust only while this is set. */
  int live;
  int handle_called;
  int loop_done;
  int once;
  int once_used;
  _Atomic uint64_t cur_seq;
  _Atomic uint64_t kill_seq;
  zend_atomic_bool *vm_interrupt;
  pthread_t tid;
  /* EG(timeout_seconds) before a kick changed it for the error message. */
  zend_long saved_timeout;
  int timeout_changed;
  /* A kick raised the timeout; Rust already logged it in one line. */
  int kicked;
} hub_ctx;

static __thread hub_ctx *tctx = NULL;

static HashTable *env_snapshot = NULL;
static char *ini_defaults_src = NULL;
static int hub_timeout_seconds = 60;
static void (*prev_interrupt)(zend_execute_data *ed) = NULL;

#if defined(SIGRTMIN)
#define HUB_KILL_SIGNAL (SIGRTMIN + 3)
static atomic_bool kill_signal_ready = false;
#endif

static const char *MODULES_TO_RELOAD[] = {"filter", NULL};

/* ---------------------------------------------------------------- SAPI */

static size_t hub_ub_write(const char *str, size_t len) {
  hub_ctx *c = tctx;
  if (c && c->live) {
    phphub_rs_write(c->rs, str, len);
  }
  return len;
}

static void hub_flush(void *server_context) {
  (void)server_context;
  hub_ctx *c = tctx;
  if (!c || !c->live) {
    return;
  }
  if (!SG(headers_sent)) {
    sapi_send_headers();
  }
  phphub_rs_flush(c->rs);
}

static int hub_send_headers(sapi_headers_struct *h) {
  hub_ctx *c = tctx;
  if (!c || !c->live || SG(request_info).no_headers == 1) {
    return SAPI_HEADER_SENT_SUCCESSFULLY;
  }

  int status = h->http_response_code ? h->http_response_code : 200;
  size_t count = zend_llist_count(&h->headers);
  hub_str stack[64];
  hub_str *lines = stack;
  if (count > 64) {
    lines = emalloc(sizeof(hub_str) * count);
  }

  size_t n = 0;
  zend_llist_position pos;
  sapi_header_struct *hd = zend_llist_get_first_ex(&h->headers, &pos);
  while (hd && n < count) {
    lines[n].ptr = hd->header;
    lines[n].len = hd->header_len;
    n++;
    hd = zend_llist_get_next_ex(&h->headers, &pos);
  }

  phphub_rs_send_headers(c->rs, status, lines, n);

  if (lines != stack) {
    efree(lines);
  }
  return SAPI_HEADER_SENT_SUCCESSFULLY;
}

static size_t hub_read_post(char *buf, size_t len) {
  hub_ctx *c = tctx;
  if (!c || !c->req) {
    return 0;
  }
  return phphub_rs_read_body(c->rs, buf, len);
}

static char *hub_read_cookies(void) {
  hub_ctx *c = tctx;
  if (!c || !c->req) {
    return NULL;
  }
  return (char *)c->req->cookie;
}

static void hub_register_variables(zval *track_vars_array) {
  HashTable *ht = Z_ARRVAL_P(track_vars_array);
  hub_ctx *c = tctx;
  const hub_request *r = c ? c->req : NULL;

  size_t extra = r ? r->num_vars + r->num_headers : 0;
  if (env_snapshot) {
    zend_hash_extend(ht, zend_hash_num_elements(env_snapshot) + extra, 0);
    /* Keys and values are interned, so copying never touches refcounts
     * shared between threads. */
    zend_hash_copy(ht, env_snapshot, NULL);
  }
  if (!r) {
    return;
  }

  for (size_t i = 0; i < r->num_vars; i++) {
    const hub_str *k = &r->vars[i * 2];
    const hub_str *v = &r->vars[i * 2 + 1];
    zval z;
    ZVAL_STRINGL_FAST(&z, v->ptr, v->len);
    zend_hash_str_update_ind(ht, k->ptr, k->len, &z);
  }

  char key[256];
  memcpy(key, "HTTP_", 5);
  for (size_t i = 0; i < r->num_headers; i++) {
    const hub_str *k = &r->headers[i * 2];
    const hub_str *v = &r->headers[i * 2 + 1];
    if (k->len == 0 || k->len > sizeof(key) - 6) {
      continue;
    }
    for (size_t j = 0; j < k->len; j++) {
      char ch = k->ptr[j];
      if (ch == '-') {
        ch = '_';
      } else if (ch >= 'a' && ch <= 'z') {
        ch = (char)(ch - 'a' + 'A');
      }
      key[5 + j] = ch;
    }
    zval z;
    ZVAL_STRINGL_FAST(&z, v->ptr, v->len);
    zend_hash_str_update_ind(ht, key, 5 + k->len, &z);
  }
}

static void hub_log_message(const char *msg, int syslog_type) {
  (void)syslog_type;
  hub_ctx *c = tctx;
  if (c && c->kicked && strstr(msg, "Maximum execution time")) {
    c->kicked = 0;
    return;
  }
  phphub_rs_log(msg, strlen(msg));
}

static zend_result hub_get_request_time(double *t) {
  hub_ctx *c = tctx;
  if (c && c->req) {
    *t = c->req->request_time;
    return SUCCESS;
  }
  return FAILURE;
}

static int hub_activate(void) {
  hub_ctx *c = tctx;
  /* sapi_activate() resets this to HTTP/1.0 before calling us. */
  SG(request_info).proto_num = (c && c->req) ? c->req->proto_num : 1000;
  return SUCCESS;
}

static int hub_deactivate(void) { return SUCCESS; }

static void hub_ini_defaults(HashTable *cfg) {
  if (!ini_defaults_src) {
    return;
  }
  /* "key=value\n" lines; php.ini and conf.d override these. */
  const char *p = ini_defaults_src;
  while (*p) {
    const char *eol = strchr(p, '\n');
    size_t line_len = eol ? (size_t)(eol - p) : strlen(p);
    const char *eq = memchr(p, '=', line_len);
    if (eq) {
      zval tmp;
      ZVAL_NEW_STR(&tmp, zend_string_init(eq + 1, line_len - (eq + 1 - p), 1));
      zend_hash_str_update(cfg, p, eq - p, &tmp);
    }
    p += line_len;
    if (*p == '\n') {
      p++;
    }
  }
}

/* ------------------------------------------------- worker request cycle */

static void hub_reset_super_globals(void) {
  zend_try {
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor_nogc(files);
    memset(files, 0, sizeof(*files));
    zend_hash_str_del(&EG(symbol_table), "_SESSION", sizeof("_SESSION") - 1);
  }
  zend_end_try();

  zend_auto_global *ag;
  zend_string *env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);
  zend_string *server = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_SERVER);
  zend_string *request = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_REQUEST);
  ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), ag) {
    if (ag->name == env) {
      /* keep $_ENV */
    } else if (ag->name == server) {
      ag->armed = ag->auto_global_callback(ag->name);
    } else if (ag->jit) {
      if (ag->name == request &&
          zend_hash_exists(&EG(symbol_table), ag->name)) {
        ag->armed = ag->auto_global_callback(ag->name);
      }
    } else if (ag->auto_global_callback) {
      /* $_GET, $_POST, $_COOKIE, $_FILES */
      ag->armed = ag->auto_global_callback(ag->name);
    } else {
      ag->armed = 0;
    }
  }
  ZEND_HASH_FOREACH_END();
}

static void hub_release_temporary_streams(void) {
  zend_resource *val;
  int stream_type = php_file_le_stream();
  ZEND_HASH_FOREACH_PTR(&EG(regular_list), val) {
    if (val->type == stream_type) {
      php_stream *stream = (php_stream *)val->ptr;
      if (stream != NULL && stream->ops == &php_stream_temp_ops &&
          stream->__exposed == 0 && GC_REFCOUNT(val) == 1) {
        zend_list_delete(val);
      }
    }
  }
  ZEND_HASH_FOREACH_END();
}

static void hub_reset_session_state(void) {
  if (PS(session_status) == php_session_active) {
    php_session_flush(1);
  }
  if (!Z_ISUNDEF(PS(http_session_vars))) {
    zval_ptr_dtor(&PS(http_session_vars));
    ZVAL_UNDEF(&PS(http_session_vars));
  }
  if (PS(mod_data) || PS(mod_user_implemented)) {
    zend_try { PS(mod)->s_close(&PS(mod_data)); }
    zend_end_try();
  }
  if (PS(id)) {
    zend_string_release_ex(PS(id), 0);
    PS(id) = NULL;
  }
  if (PS(session_vars)) {
    zend_string_release_ex(PS(session_vars), 0);
    PS(session_vars) = NULL;
  }
  if (PS(session_started_filename)) {
    zend_string_release(PS(session_started_filename));
    PS(session_started_filename) = NULL;
    PS(session_started_lineno) = 0;
  }
  PS(session_status) = php_session_none;
  PS(in_save_handler) = 0;
  PS(set_handler) = 0;
  PS(mod_data) = NULL;
  PS(mod_user_is_open) = 0;
  PS(define_sid) = 1;
}

static void hub_reset_last_errors(void) {
  if (PG(last_error_message)) {
    zend_string_release(PG(last_error_message));
    PG(last_error_message) = NULL;
  }
  if (PG(last_error_file)) {
    zend_string_release(PG(last_error_file));
    PG(last_error_file) = NULL;
  }
  PG(last_error_type) = 0;
  PG(last_error_lineno) = 0;

  zend_function *fn = zend_hash_str_find_ptr(
      CG(function_table), ZEND_STRL("libxml_clear_errors"));
  if (fn) {
    zend_call_known_function(fn, NULL, NULL, NULL, 0, NULL, NULL);
  }
}

static void hub_attach(hub_ctx *c, const hub_request *r) {
  c->req = r;
  atomic_store(&c->cur_seq, r ? r->seq : 0);

  /* SG(server_context) must not be NULL or PHP skips POST and cookies. */
  SG(server_context) = c;
  SG(sapi_headers).http_response_code = 200;
  SG(request_info).request_method = r ? r->method : NULL;
  SG(request_info).query_string = r ? (char *)r->query : NULL;
  SG(request_info).request_uri = r ? (char *)r->uri : NULL;
  SG(request_info).content_type = r ? r->content_type : NULL;
  SG(request_info).content_length = r ? r->content_length : 0;
  SG(request_info).path_translated =
      (char *)((r && r->script) ? r->script : c->script);
  SG(request_info).proto_num = r ? r->proto_num : 1000;
  php_handle_auth_data(r ? r->authorization : NULL);
}

static void hub_detach_request_info(void) {
  SG(request_info).request_method = NULL;
  SG(request_info).query_string = NULL;
  SG(request_info).request_uri = NULL;
  SG(request_info).content_type = NULL;
  SG(request_info).path_translated = NULL;
  SG(request_info).cookie_data = NULL;
}

/* Adapted from php_request_shutdown(), keeping the script's state. */
static void hub_worker_request_shutdown(hub_ctx *c) {
  (void)c;
  zend_try { php_output_end_all(); }
  zend_end_try();

  for (const char **name = MODULES_TO_RELOAD; *name; name++) {
    zend_module_entry *m =
        zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
    if (m && m->request_shutdown_func) {
      m->request_shutdown_func(m->type, m->module_number);
    }
  }

  hub_reset_session_state();
  hub_reset_last_errors();

  zend_try { php_output_deactivate(); }
  zend_end_try();
  zend_try { sapi_deactivate(); }
  zend_end_try();
  hub_detach_request_info();

  zend_set_memory_limit(PG(memory_limit));
}

/* Adapted from php_request_startup(), keeping the script's state. */
static int hub_worker_request_startup(hub_ctx *c, const hub_request *r) {
  volatile int ret = SUCCESS;
  hub_attach(c, r);

  zend_try {
    hub_release_temporary_streams();
    php_output_activate();
    PG(header_is_being_sent) = 0;
    PG(connection_status) = PHP_CONNECTION_NORMAL;

    sapi_activate();

    if (PG(output_handler) && PG(output_handler)[0]) {
      zval oh;
      ZVAL_STRING(&oh, PG(output_handler));
      php_output_start_user(&oh, 0, PHP_OUTPUT_HANDLER_STDFLAGS);
      zval_ptr_dtor(&oh);
    } else if (PG(output_buffering)) {
      php_output_start_user(NULL,
                            PG(output_buffering) > 1 ? PG(output_buffering) : 0,
                            PHP_OUTPUT_HANDLER_STDFLAGS);
    } else if (PG(implicit_flush)) {
      php_output_set_implicit_flush(1);
    }

    hub_reset_super_globals();

    for (const char **name = MODULES_TO_RELOAD; *name; name++) {
      zend_module_entry *m =
          zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
      if (m && m->request_startup_func) {
        m->request_startup_func(m->type, m->module_number);
      }
    }
  }
  zend_catch { ret = FAILURE; }
  zend_end_try();

  SG(sapi_started) = 1;
  return ret;
}

/* Send everything this request produced and hand the response to Rust. */
static void hub_finish_response(hub_ctx *c) {
  zend_try { php_output_end_all(); }
  zend_end_try();
  zend_try {
    if (!SG(headers_sent)) {
      php_header();
    }
  }
  zend_end_try();

  c->live = 0;
  phphub_rs_finish(c->rs, 0);
  atomic_store(&c->cur_seq, 0);

  if (SG(rfc1867_uploaded_files)) {
    destroy_uploaded_files_hash();
  }
}

/* ------------------------------------------------------- PHP functions */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_phphub_bool, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(phphub_handle_request) {
  ZEND_PARSE_PARAMETERS_NONE();
  hub_ctx *c = tctx;
  if (!c) {
    RETURN_FALSE;
  }

  /* index and file modes: serve the attached request once, never loop. */
  if (c->once) {
    if (!c->once_used && c->live) {
      c->once_used = 1;
      RETURN_TRUE;
    }
    RETURN_FALSE;
  }

  if (!c->handle_called) {
    c->handle_called = 1;
    if (c->live) {
      /* The script started with a request attached. Drop what the bootstrap
       * printed; Rust hands the same request back from phphub_rs_next(). */
      c->live = 0;
      phphub_rs_detach(c->rs);
    }
  } else if (c->live) {
    hub_finish_response(c);
  }

  if (c->loop_done) {
    RETURN_FALSE;
  }

#ifdef ZEND_MAX_EXECUTION_TIMERS
  zend_unset_timeout();
#endif

  hub_worker_request_shutdown(c);

  const hub_request *r = phphub_rs_next(c->rs);
  if (!r) {
    c->loop_done = 1;
    hub_worker_request_startup(c, NULL);
    RETURN_FALSE;
  }

  c->live = 1;
  if (hub_worker_request_startup(c, r) == FAILURE) {
    RETURN_FALSE;
  }

#ifdef ZEND_MAX_EXECUTION_TIMERS
  if (EG(timeout_seconds) > 0) {
    zend_set_timeout(EG(timeout_seconds), 0);
  }
#endif

  RETURN_TRUE;
}

PHP_FUNCTION(phphub_finish_request) {
  ZEND_PARSE_PARAMETERS_NONE();
  hub_ctx *c = tctx;
  if (!c || !c->live) {
    RETURN_FALSE;
  }
  hub_finish_response(c);
  RETURN_TRUE;
}

static const zend_function_entry hub_functions[] = {
    PHP_FE(phphub_handle_request, arginfo_phphub_bool)
        PHP_FE(phphub_finish_request, arginfo_phphub_bool) PHP_FE_END};

static zend_module_entry hub_module = {STANDARD_MODULE_HEADER,
                                       "phphub",
                                       hub_functions,
                                       NULL,
                                       NULL,
                                       NULL,
                                       NULL,
                                       NULL,
                                       "0.1.0",
                                       STANDARD_MODULE_PROPERTIES};

static int hub_startup(sapi_module_struct *sm) {
  return php_module_startup(sm, &hub_module);
}

static sapi_module_struct hub_sapi = {
    "phphub", /* name */
    "PHPHub", /* pretty name */

    hub_startup,                 /* startup */
    php_module_shutdown_wrapper, /* shutdown */

    hub_activate,   /* activate */
    hub_deactivate, /* deactivate */

    hub_ub_write, /* unbuffered write */
    hub_flush,    /* flush */
    NULL,         /* get uid */
    NULL,         /* getenv */

    php_error, /* error handler */

    NULL,             /* header handler */
    hub_send_headers, /* send headers handler */
    NULL,             /* send header handler */

    hub_read_post,    /* read POST data */
    hub_read_cookies, /* read Cookies */

    hub_register_variables, /* register server variables */
    hub_log_message,        /* log message */
    hub_get_request_time,   /* get request time */
    NULL,                   /* child terminate */

    STANDARD_SAPI_MODULE_PROPERTIES};

/* ------------------------------------------------------ process & threads */

static zend_string *persistent_interned(const char *s, size_t len) {
  zend_string *z = zend_string_init(s, len, 1);
  zend_string_hash_val(z);
  GC_ADD_FLAGS(z, IS_STR_INTERNED);
  return z;
}

static void build_env_snapshot(void) {
  env_snapshot = pemalloc(sizeof(HashTable), 1);
  zend_hash_init(env_snapshot, 32, NULL, NULL, 1);
  for (char **e = environ; e && *e; e++) {
    const char *eq = strchr(*e, '=');
    if (!eq || eq == *e) {
      continue;
    }
    zend_string *k = persistent_interned(*e, eq - *e);
    zend_string *v = persistent_interned(eq + 1, strlen(eq + 1));
    zval zv;
    ZVAL_INTERNED_STR(&zv, v);
    zend_hash_update(env_snapshot, k, &zv);
  }
}

static void hub_opcache_hook(int reason) { phphub_rs_opcache_restart(reason); }

static void hub_interrupt(zend_execute_data *ed) {
  hub_ctx *c = tctx;
  if (c) {
    uint64_t k = atomic_load(&c->kill_seq);
    if (k != 0 && k == atomic_load(&c->cur_seq)) {
      atomic_store(&c->kill_seq, 0);
      if (!c->timeout_changed) {
        c->saved_timeout = EG(timeout_seconds);
        c->timeout_changed = 1;
      }
      EG(timeout_seconds) = hub_timeout_seconds;
      c->kicked = 1;
      zend_timeout();
    }
  }
  if (prev_interrupt) {
    prev_interrupt(ed);
  }
}

#ifdef HUB_KILL_SIGNAL
/* Delivery alone interrupts blocking syscalls with EINTR. */
static void hub_kill_signal_handler(int sig) { (void)sig; }
#endif

int phphub_php_startup(int threads, const char *ini_entries,
                       const char *ini_defaults, int timeout_seconds) {
  hub_timeout_seconds = timeout_seconds;

  php_tsrm_startup_ex(threads + 1);
  sapi_startup(&hub_sapi);

  hub_sapi.ini_entries = strdup(ini_entries);
  ini_defaults_src = strdup(ini_defaults);
  hub_sapi.ini_defaults = hub_ini_defaults;
  hub_sapi.php_ini_ignore = 0;

  if (hub_sapi.startup(&hub_sapi) == FAILURE) {
    return -1;
  }

  zend_accel_schedule_restart_hook = hub_opcache_hook;
  prev_interrupt = zend_interrupt_function;
  zend_interrupt_function = hub_interrupt;

  build_env_snapshot();

#ifdef HUB_KILL_SIGNAL
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = hub_kill_signal_handler;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = 0; /* no SA_RESTART */
  if (sigaction(HUB_KILL_SIGNAL, &sa, NULL) == 0) {
    atomic_store(&kill_signal_ready, true);
  }
#endif
  return 0;
}

void phphub_php_shutdown(void) {
  php_module_shutdown();
  sapi_shutdown();
  tsrm_shutdown();
  free((char *)hub_sapi.ini_entries);
  hub_sapi.ini_entries = NULL;
}

void *phphub_thread_init(void *rs, const char *script) {
  (void)ts_resource(0);

  hub_ctx *c = calloc(1, sizeof(hub_ctx));
  c->rs = rs;
  c->script = script;
  c->vm_interrupt = &EG(vm_interrupt);
  c->tid = pthread_self();

#ifdef HUB_KILL_SIGNAL
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, HUB_KILL_SIGNAL);
  pthread_sigmask(SIG_UNBLOCK, &set, NULL);
#endif

  tctx = c;
  return c;
}

void phphub_thread_free(void *ctx) {
  tctx = NULL;
  ts_free_thread();
  free(ctx);
}

/* Run the script once. With `req` the script starts inside that request;
 * without it the script is a worker booting before any traffic. */
int phphub_run(void *ctx, const hub_request *req, int once) {
  hub_ctx *c = ctx;
  volatile int flags = 0;
  const char *volatile script = (req && req->script) ? req->script : c->script;

  c->handle_called = 0;
  c->loop_done = 0;
  c->once = once;
  c->once_used = 0;
  c->live = req != NULL;
  hub_attach(c, req);

  zend_first_try {
    if (php_request_startup() == FAILURE) {
      flags |= HUB_RUN_FATAL;
    } else {
      zend_file_handle fh;
      zend_stream_init_filename(&fh, script);
      fh.primary_script = 1;
      EG(exit_status) = 0;
      if (!php_execute_script(&fh) || EG(exit_status) == 255) {
        flags |= HUB_RUN_FATAL;
      }
      zend_destroy_file_handle(&fh);
    }
  }
  zend_catch { flags |= HUB_RUN_FATAL; }
  zend_end_try();

  /* A bailout inside a session save handler leaves this set and blocks the
   * handler's close() in RSHUTDOWN. */
  PS(in_save_handler) = 0;

  /* Flushes what is left to the live request, if any. */
  zend_try { php_request_shutdown(NULL); }
  zend_end_try();

  if (c->handle_called) {
    flags |= HUB_RUN_HANDLED;
  }
  if (c->loop_done) {
    flags |= HUB_RUN_LOOP_DONE;
  }
  if (c->live) {
    c->live = 0;
    phphub_rs_finish(c->rs, (flags & HUB_RUN_FATAL) ? 1 : 0);
  }

  hub_detach_request_info();
  c->req = NULL;
  atomic_store(&c->cur_seq, 0);
  SG(server_context) = NULL;
  if (c->timeout_changed) {
    EG(timeout_seconds) = c->saved_timeout;
    c->timeout_changed = 0;
  }
  c->kicked = 0;
  return flags;
}

/* Called from another thread: abort request `seq` if it is still running. */
void phphub_kick(void *ctx, uint64_t seq) {
  hub_ctx *c = ctx;
  atomic_store(&c->kill_seq, seq);
  zend_atomic_bool_store(c->vm_interrupt, true);
#ifdef HUB_KILL_SIGNAL
  if (atomic_load(&kill_signal_ready)) {
    pthread_kill(c->tid, HUB_KILL_SIGNAL);
  }
#endif
}
