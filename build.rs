fn main() {
    // We need to explicitly link with libctru at the top level so all of its
    // symbols get pulled in for std. Without this, the linker will throw away
    // some of libctru's symbols (from ctru-sys) before it processes std's
    // unknown symbols.
    let profile = std::env::var("PROFILE").unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-link-lib=static={}",
        match profile.as_str() {
            "debug" => "ctrud",
            _ => "ctru",
        }
    );
}