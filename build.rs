use std::process::Command;

fn php_config(arg: &str) -> String {
    let bin = std::env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let out = Command::new(&bin)
        .arg(arg)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {bin}: {e}"));
    assert!(out.status.success(), "{bin} {arg} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn main() {
    println!("cargo:rerun-if-changed=csrc/phphub.c");
    println!("cargo:rerun-if-changed=csrc/phphub.h");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");

    let mut build = cc::Build::new();
    build.file("csrc/phphub.c").include("csrc").opt_level(2).warnings(true);
    for inc in php_config("--includes").split_whitespace() {
        build.include(inc.trim_start_matches("-I"));
    }
    build.flag_if_supported("-Wno-unused-parameter");
    build.flag_if_supported("-Wno-missing-field-initializers");
    build.compile("phphub_sapi");

    // mimalloc replaces musl's malloc for the whole process, libphp and the
    // extensions included: musl's allocator takes one lock for all threads.
    // Index mode measured 12-17% more requests per second with it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // The plain build, not libmimalloc-secure that -lmimalloc points to,
        // and listed before libc so its malloc is the one symbols bind to.
        println!("cargo:rustc-link-lib=dylib:+verbatim=libmimalloc.so.2");
    }

    let prefix = php_config("--prefix");
    println!("cargo:rustc-link-search=native={prefix}/lib");
    println!("cargo:rustc-link-lib=dylib=php");
    // Extensions loaded with dlopen() resolve PHP symbols from libphp.so,
    // so nothing extra has to be exported from the executable.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{prefix}/lib");
}
