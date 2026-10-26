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

    let prefix = php_config("--prefix");
    println!("cargo:rustc-link-search=native={prefix}/lib");
    println!("cargo:rustc-link-lib=dylib=php");
    // Extensions loaded with dlopen() resolve PHP symbols from libphp.so,
    // so nothing extra has to be exported from the executable.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{prefix}/lib");
}
