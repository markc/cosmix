use std::env;

fn main() {
    // Cargo owns PROFILE for build-script processes and overwrites any parent
    // value. Keying only on the literal release profile means
    // CARGO_PROFILE_DEV_DEBUG_ASSERTIONS cannot authorise the live path. The
    // rustc-env marker is also required because RUSTFLAGS can forge any named
    // cfg, including the audit-friendly cfg emitted alongside it.
    let cargo_release = env::var("PROFILE").as_deref() == Ok("release");
    println!(
        "cargo::rustc-env=COSMIX_KMS_LIVE_CARGO_PROFILE={}",
        if cargo_release {
            "release"
        } else {
            "not-release"
        }
    );
    if cargo_release {
        println!("cargo::rustc-cfg=cosmix_kms_live_release");
    }
}
