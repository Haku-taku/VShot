// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// The recording C shims: `shim.c` (the libavcodec encoder and the MP4 muxer),
// `pin_hdr_fp16.c` (the half-float surface a pinned HDR image is drawn on) and,
// where libpipewire's headers are installed, `pipewire_client.c` (the portal's
// screen-cast stream).  Both libraries are loaded with dlopen inside the C code,
// so the link here is against the small `vshot_av` archive alone — no
// libavcodec, libpipewire or libgbm symbols in the binary's dynamic table.

fn main() {
    println!("cargo:rerun-if-changed=src/record/shim.c");
    println!("cargo:rerun-if-changed=src/model/codec/ffmpeg_still.c");
    println!("cargo:rerun-if-changed=src/record/pipewire_client.c");
    println!("cargo:rerun-if-changed=src/record/pipewire_audio.c");
    println!("cargo:rerun-if-changed=src/pin_hdr_fp16.c");
    println!("cargo:rerun-if-changed=src/pin_hdr_fp16.h");
    // `cfg` names a build script chooses have to be declared, or rustc reports
    // every use of them as a typo.
    println!("cargo:rustc-check-cfg=cfg(vshot_pipewire)");
    // Still images need only the codec library: an encoded packet from these
    // encoders *is* the file, so there is no muxer step and no libavformat.
    // See the comment at the top of `ffmpeg_still.c` for the measurement.
    let mut still = cc::Build::new();
    still.file("src/model/codec/ffmpeg_still.c");
    for flag in pkg_config_cflags(&["libavcodec", "libavutil"]) {
        still.flag(flag);
    }
    still.warnings(true);
    still.compile("vshot_still");

    let mut build = cc::Build::new();
    build.file("src/record/shim.c");
    // The headers come from the system ffmpeg; a build machine without them
    // cannot compile the shim, which is the same class of dependency as the
    // system ONNX Runtime the OCR side already needs.  libavfilter and
    // libavformat are only header dependencies: the shim loads the libraries
    // itself with dlopen, and their absence is reported at runtime by the
    // paths that need them (the zero-copy path and the MP4 muxer
    // respectively).
    for flag in pkg_config_cflags(&["libavcodec", "libavutil", "libavfilter"]) {
        build.flag(flag);
    }
    // The portal's PipeWire client and the microphone's are the C files that
    // are optional: they are compiled only where libpipewire's headers are
    // installed, and their absence costs nothing else — the compositor's own
    // protocols still record, and `record --portal` / `record --mic` report
    // the missing package instead of the build failing for everybody.
    if pkg_config_present("libpipewire-0.3") {
        println!("cargo:rustc-cfg=vshot_pipewire");
        build.file("src/record/pipewire_client.c");
        build.file("src/record/pipewire_audio.c");
        for flag in pkg_config_cflags(&["libpipewire-0.3"]) {
            build.flag(flag);
        }
    } else {
        println!(
            "cargo:warning=libpipewire-0.3 has no pkg-config entry; this build leaves out \
             `record --portal` and `record --mic`"
        );
    }
    build.warnings(true);
    build.compile("vshot_av");

    // The half-float pin surface: GBM, EGL and OpenGL, all dlopen'd inside the C
    // file, so nothing here links against them either.  It compiles wherever the
    // headers are, and a build without them leaves the caller its SDR path --
    // which is why the include directories are best-effort rather than required.
    let mut fp16 = cc::Build::new();
    fp16.file("src/pin_hdr_fp16.c");
    // `gbm.h` pulls in `drm_fourcc.h`, which lives in libdrm's own directory on
    // the distributions that split the headers out.
    for flag in pkg_config_cflags(&["libdrm"]) {
        fp16.flag(flag);
    }
    fp16.warnings(true);
    fp16.compile("vshot_pin_hdr_fp16");

    // dlopen lives in libc on musl and libdl on glibc; linking `dl` covers
    // both without pulling in libavcodec itself.
    println!("cargo:rustc-link-lib=dl");
}

/// The compiler flags pkg-config reports for `packages`, verbatim: the include
/// directories mostly, but `-D_REENTRANT` and its neighbours matter to the
/// headers that check them.
fn pkg_config_cflags(packages: &[&str]) -> Vec<String> {
    let output = std::process::Command::new("pkg-config")
        .arg("--cflags")
        .args(packages)
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether pkg-config knows `package` at all.
fn pkg_config_present(package: &str) -> bool {
    std::process::Command::new("pkg-config")
        .arg("--exists")
        .arg(package)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
