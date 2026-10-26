//! Mirrors csrc/phphub.h.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HubStr {
    pub ptr: *const c_char,
    pub len: usize,
}

#[repr(C)]
pub struct HubRequest {
    pub seq: u64,
    pub method: *const c_char,
    pub uri: *const c_char,
    pub query: *const c_char,
    pub content_type: *const c_char,
    pub content_length: i64,
    pub cookie: *const c_char,
    pub authorization: *const c_char,
    pub headers: *const HubStr,
    pub num_headers: usize,
    pub vars: *const HubStr,
    pub num_vars: usize,
    pub request_time: f64,
    pub proto_num: c_int,
    pub script: *const c_char,
}

pub const RUN_FATAL: c_int = 1;
pub const RUN_HANDLED: c_int = 2;
#[allow(dead_code)]
pub const RUN_LOOP_DONE: c_int = 4;

unsafe extern "C" {
    pub fn phphub_php_startup(
        threads: c_int,
        ini_entries: *const c_char,
        ini_defaults: *const c_char,
        timeout_seconds: c_int,
    ) -> c_int;
    pub fn phphub_php_shutdown();
    pub fn phphub_thread_init(rs: *mut c_void, script: *const c_char) -> *mut c_void;
    pub fn phphub_thread_free(ctx: *mut c_void);
    pub fn phphub_run(ctx: *mut c_void, req: *const HubRequest, once: c_int) -> c_int;
    pub fn phphub_kick(ctx: *mut c_void, seq: u64);
}
