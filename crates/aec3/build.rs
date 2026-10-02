//! Builds WebRTC's audio processing (AEC3, noise suppression, high-pass filter) from the
//! pinned source used by EchoBridge since 0.1, plus the C shim in `cpp/`.
//!
//! The source is downloaded once per build directory and verified by its SHA-256. Set
//! `ECHOBRIDGE_WEBRTC_SRC` to an extracted `vendor/webrtc_audio` folder to build offline.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const COMMIT: &str = "1bc860a2051c2b9b3c10b8c778770794b9321d84";
const URL: &str =
    "https://codeload.github.com/strands-labs/pywebrtc-audio/tar.gz/1bc860a2051c2b9b3c10b8c778770794b9321d84";
const SHA256: &str = "f99dd804a4ba33cf6658ecf0e87c9e5fcc0eaa961a90b7cc8d819e12d942f214";

/// Sources EchoBridge does not use (automatic gain control and its voice detector).
const UNUSED: &[&str] = &["audio_processing/agc2/", "third_party/rnnoise/", "resampler/push_resampler.cc"];

fn main() {
    println!("cargo:rerun-if-changed=cpp");
    println!("cargo:rerun-if-env-changed=ECHOBRIDGE_WEBRTC_SRC");
    let source = match env::var_os("ECHOBRIDGE_WEBRTC_SRC") {
        Some(path) => PathBuf::from(path),
        None => download(),
    };
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let msvc = env::var("CARGO_CFG_TARGET_ENV").unwrap() == "msvc";

    let mut files = source_list(&source.join("CMakeLists.txt"));
    match target_arch.as_str() {
        "x86_64" => files.extend(
            ["audio_processing/resampler/sinc_resampler_sse.cc", "audio_processing/utility/ooura_fft_sse2.cc"]
                .map(String::from),
        ),
        "aarch64" => files.extend(
            ["audio_processing/resampler/sinc_resampler_neon.cc", "audio_processing/utility/ooura_fft_neon.cc"]
                .map(String::from),
        ),
        _ => {}
    }
    if target_os == "linux" {
        files.push("system_wrappers/source/cpu_features_linux.c".into());
    }
    files.retain(|file| !UNUSED.iter().any(|unused| file.contains(unused)));

    let base = || {
        let mut build = cc::Build::new();
        build
            .include(&source)
            .include(source.join("audio_processing"))
            .include(source.join("audio_processing/logging"))
            .include(source.join("abseil"))
            .define("WEBRTC_APM_DEBUG_DUMP", "0")
            .define("RTC_DISABLE_CHECK_MSG", None)
            .warnings(false);
        if target_os == "windows" {
            build.define("WEBRTC_WIN", None).define("NOMINMAX", None);
        } else {
            build.define("WEBRTC_POSIX", None);
            if target_os == "linux" {
                build.define("WEBRTC_LINUX", None).define("WEBRTC_THREAD_RR", None);
            } else if target_os == "macos" {
                build.define("WEBRTC_MAC", None);
            }
        }
        match target_arch.as_str() {
            "x86_64" => {
                build.define("WEBRTC_ARCH_X86_FAMILY", None);
            }
            "aarch64" => {
                build
                    .define("WEBRTC_ARCH_ARM64", None)
                    .define("WEBRTC_ARCH_ARM_FAMILY", None)
                    .define("WEBRTC_HAS_NEON", None);
            }
            _ => {}
        }
        let compat = source.join("compat_includes.h");
        if msvc {
            build.flag(format!("/FI{}", compat.display()));
        } else {
            build.flag("-include").flag(compat.to_str().unwrap());
            if target_arch == "x86_64" {
                build.flag("-msse2");
            }
        }
        build
    };

    let (c_files, cpp_files): (Vec<_>, Vec<_>) = files.iter().partition(|file| file.ends_with(".c"));
    let mut cpp = base();
    cpp.cpp(true).std(if msvc { "c++20" } else { "c++17" });
    if msvc {
        cpp.flag("/EHsc");
    }
    cpp.files(cpp_files.iter().map(|file| source.join(file))).file("cpp/shim.cpp").include("cpp");
    cpp.compile("echobridge_webrtc");
    let mut c = base();
    c.files(c_files.iter().map(|file| source.join(file)));
    c.compile("echobridge_webrtc_c");
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=winmm");
    }
}

/// The library sources named in the vendor's CMake list.
fn source_list(cmake: &Path) -> Vec<String> {
    let text = fs::read_to_string(cmake).expect("vendor CMakeLists.txt");
    let start = text.find("set(WEBRTC_AUDIO_SOURCES").expect("source list in CMakeLists.txt");
    let list = &text[start..];
    let end = list.find(')').unwrap();
    list[..end]
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Download and verify the pinned source; returns its `vendor/webrtc_audio` folder.
fn download() -> PathBuf {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let root = out.join(format!("pywebrtc-audio-{COMMIT}"));
    let vendor = root.join("vendor/webrtc_audio");
    if vendor.join("CMakeLists.txt").is_file() {
        return vendor;
    }
    let archive = out.join("webrtc.tar.gz");
    let status = Command::new("curl").args(["-sSfL", "--retry", "3", "-o"]).arg(&archive).arg(URL).status();
    assert!(
        status.is_ok_and(|s| s.success()),
        "downloading the WebRTC source failed; set ECHOBRIDGE_WEBRTC_SRC to build offline"
    );
    let digest = Sha256::digest(fs::read(&archive).unwrap());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, SHA256, "the WebRTC source archive does not match its pinned SHA-256");
    let status = Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&out).status();
    assert!(status.is_ok_and(|s| s.success()), "extracting the WebRTC source failed");
    fs::remove_file(&archive).ok();
    vendor
}
