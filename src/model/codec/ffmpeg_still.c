// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

// vshot's still-image encoder boundary: JPEG, WebP and JPEG XL through
// ffmpeg's libavcodec, loaded with dlopen at runtime.
//
// Why libavcodec and not a subprocess: a screenshot is already pixels in
// memory, and handing them to a child process would mean writing a temporary
// file, re-reading it, and parsing whatever the child printed.  The libraries
// are the same ones `record` already uses for video (see `src/record/shim.c`),
// so a machine that can record can write these too.
//
// The library is optional.  Everything here is resolved through `dlopen`, so a
// build machine -- or a user's machine -- without ffmpeg still screenshots, and
// `vshot formats` simply does not offer the formats this file would provide.
//
// Two facts shape the whole file, and both were measured against the system
// ffmpeg rather than assumed:
//
//   * An encoded *packet* from these still encoders is already the whole file.
//     `mjpeg` begins with the JPEG SOI marker, `libwebp` with a RIFF header and
//     `libjxl` with the JPEG XL container signature and its `ftyp` box.  There
//     is no muxer step: the packet bytes are what the file contains, which is
//     why this file links no libavformat at all.
//
//   * A still decoder returns its frame only once it has been pushed: after
//     `avcodec_send_packet`, a second `send_packet(NULL)` is what makes
//     `avcodec_receive_frame` produce it.  Without that the first receive
//     answers `EAGAIN` and every decode looks like a corrupt file.
//
// The interface Rust sees is one function per direction plus the probes:
//   vshot_still_available(kind)                    -> 1 when codec + decoder open
//   vshot_still_encode(kind, ..., out, out_len)    -> 0 on success
//   vshot_still_decode_jxl(...)                    -> 0 on success
//   vshot_still_free(bytes) / vshot_still_load_error()

#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavcodec/codec.h>
#include <libavcodec/packet.h>
#include <libavutil/dict.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/mem.h>

#define STILL_JPEG 1
#define STILL_WEBP 2
#define STILL_JXL 3

/// The box this program writes its reference white in: four big-endian bytes of
/// a float, appended after the JPEG XL container.
///
/// JPEG XL carries a transfer function and primaries, but not the one thing
/// this program's HDR currency needs: what the frame's `1.0` stands for in
/// cd/m².  A capture is always taken against an output's own SDR white, and
/// without it a decode cannot tell 203 from the 100 of another display.  A box
/// of our own is the extension point the container format provides; readers
/// that do not know it skip it and show the file at BT.2408's reference, which
/// is the right answer for anything that did not come from here.
#define VSHOT_REF_BOX "vshw"
/// The same idea for a gamut this program made from the compositor's own
/// chromaticities: one flag byte, one primaries byte, then six floats.
#define VSHOT_GAMUT_BOX "vshg"

/// A ceiling on a decoded image, so a hostile or corrupt header cannot make
/// this side allocate whatever it names.
#define MAX_JXL_PIXELS (32u * 1024u * 1024u)

static void *try_open(const char *const *names)
{
    for (int i = 0; names[i]; i++) {
        void *handle = dlopen(names[i], RTLD_NOW | RTLD_LOCAL);
        if (handle) return handle;
    }
    return NULL;
}

/// Opens `stem.so.<version>` for every version a rolling release might carry,
/// newest first, and falls back to the unversioned name.  A distribution that
/// bumps the soname keeps working without a rebuild of this program.
static void *open_versioned(const char *stem, int from, int to)
{
    for (int version = from; version >= to; version--) {
        char name[64];
        snprintf(name, sizeof(name), "%s.so.%d", stem, version);
        void *handle = dlopen(name, RTLD_NOW | RTLD_LOCAL);
        if (handle) return handle;
    }
    char plain[64];
    snprintf(plain, sizeof(plain), "%s.so", stem);
    const char *const names[] = {plain, NULL};
    return try_open(names);
}

typedef struct StillApi {
    const AVCodec *(*find_encoder)(const char *);
    const AVCodec *(*find_decoder)(const char *);
    AVCodecContext *(*alloc_context)(const AVCodec *);
    void (*free_context)(AVCodecContext **);
    int (*open)(AVCodecContext *, const AVCodec *, AVDictionary **);
    int (*send_frame)(AVCodecContext *, const AVFrame *);
    int (*receive_packet)(AVCodecContext *, AVPacket *);
    int (*send_packet)(AVCodecContext *, const AVPacket *);
    int (*receive_frame)(AVCodecContext *, AVFrame *);
    AVPacket *(*packet_alloc)(void);
    void (*packet_free)(AVPacket **);
    int (*packet_new)(AVPacket *, int);
    AVFrame *(*frame_alloc)(void);
    void (*frame_free)(AVFrame **);
    int (*frame_get_buffer)(AVFrame *, int);
    int (*dict_set)(AVDictionary **, const char *, const char *, int);
    void (*dict_free)(AVDictionary **);
    char error[256];
} StillApi;

static StillApi api;
static int api_state; // 0 untouched, 1 loaded, -1 failed

#define LOAD_SYM(field, handle, name)                                          \
    do {                                                                       \
        api.field = (void *)(uintptr_t)dlsym(handle, name);                    \
        if (!api.field) {                                                      \
            snprintf(api.error, sizeof(api.error), "ffmpeg is missing %s", name); \
            goto failed;                                                       \
        }                                                                      \
    } while (0)

static StillApi *load_api(void)
{
    if (api_state != 0) return api_state > 0 ? &api : NULL;
    void *codec = open_versioned("libavcodec", 70, 55);
    void *util = open_versioned("libavutil", 70, 55);
    if (!codec || !util) {
        snprintf(api.error, sizeof(api.error),
                 "libavcodec and libavutil are required for JPEG, WebP and JPEG XL "
                 "(they come with ffmpeg)");
        goto failed;
    }
    LOAD_SYM(find_encoder, codec, "avcodec_find_encoder_by_name");
    LOAD_SYM(find_decoder, codec, "avcodec_find_decoder_by_name");
    LOAD_SYM(alloc_context, codec, "avcodec_alloc_context3");
    LOAD_SYM(free_context, codec, "avcodec_free_context");
    LOAD_SYM(open, codec, "avcodec_open2");
    LOAD_SYM(send_frame, codec, "avcodec_send_frame");
    LOAD_SYM(receive_packet, codec, "avcodec_receive_packet");
    LOAD_SYM(send_packet, codec, "avcodec_send_packet");
    LOAD_SYM(receive_frame, codec, "avcodec_receive_frame");
    LOAD_SYM(packet_alloc, codec, "av_packet_alloc");
    LOAD_SYM(packet_free, codec, "av_packet_free");
    LOAD_SYM(packet_new, codec, "av_new_packet");
    LOAD_SYM(frame_alloc, util, "av_frame_alloc");
    LOAD_SYM(frame_free, util, "av_frame_free");
    LOAD_SYM(frame_get_buffer, util, "av_frame_get_buffer");
    LOAD_SYM(dict_set, util, "av_dict_set");
    LOAD_SYM(dict_free, util, "av_dict_free");
    api_state = 1;
    return &api;
failed:
    api_state = -1;
    return NULL;
}
#undef LOAD_SYM

static const char *encoder_name(int kind)
{
    switch (kind) {
    case STILL_JPEG: return "mjpeg";
    case STILL_WEBP: return "libwebp";
    case STILL_JXL: return "libjxl";
    default: return NULL;
    }
}

static const char *decoder_name(int kind)
{
    switch (kind) {
    case STILL_JPEG: return "mjpeg";
    case STILL_WEBP: return "webp";
    case STILL_JXL: return "libjxl";
    default: return NULL;
    }
}

/// The pixel format each encoder is fed.  These are the ones the encoders
/// themselves advertise, not a preference: `libjxl` is the only one that takes
/// the linear float the HDR half is made of.
static enum AVPixelFormat encoder_pix_fmt(int kind)
{
    switch (kind) {
    case STILL_JPEG: return AV_PIX_FMT_YUVJ444P;
    case STILL_WEBP: return AV_PIX_FMT_BGRA;
    case STILL_JXL: return AV_PIX_FMT_RGBAF32LE;
    default: return AV_PIX_FMT_NONE;
    }
}

/// Whether this ffmpeg can really do `kind`, not just name it.
///
/// A build can advertise an encoder and still refuse it -- the library is
/// there but the codec was compiled out, or the options it needs are missing --
/// so the probe opens both directions at the pixel format and the settings
/// this file actually asks for.  A format that cannot do that is absent from
/// `vshot formats` rather than failing halfway through a capture.
static int codec_available(int kind)
{
    StillApi *a = load_api();
    if (!a) return 0;
    const AVCodec *encoder = a->find_encoder(encoder_name(kind));
    const AVCodec *decoder = a->find_decoder(decoder_name(kind));
    if (!encoder || !decoder) return 0;

    AVCodecContext *ctx = a->alloc_context(encoder);
    if (!ctx) return 0;
    ctx->width = 2;
    ctx->height = 2;
    ctx->time_base = (AVRational){1, 1};
    ctx->pix_fmt = encoder_pix_fmt(kind);
    AVDictionary *opts = NULL;
    if (kind == STILL_JPEG) {
        // The probe uses the same bounds the encoder path sets, so a build that
        // cannot take them is reported as unavailable rather than failing at
        // the first capture.
        ctx->qmin = 6;
        ctx->qmax = 6;
    } else if (kind == STILL_WEBP) {
        a->dict_set(&opts, "lossless", "1", 0);
    } else {
        a->dict_set(&opts, "distance", "0", 0);
    }
    int opened = a->open(ctx, encoder, &opts) >= 0;
    a->dict_free(&opts);
    a->free_context(&ctx);
    if (!opened) return 0;

    ctx = a->alloc_context(decoder);
    if (!ctx) return 0;
    opened = a->open(ctx, decoder, NULL) >= 0;
    a->free_context(&ctx);
    return opened;
}

int vshot_still_available(int kind)
{
    // Cached: the probe opens two codecs, and `vshot formats` asks about every
    // one of them.  The answer cannot change inside one process.
    static int state[4] = {0, -1, -1, -1};
    if (kind < STILL_JPEG || kind > STILL_JXL) return 0;
    if (state[kind] < 0) state[kind] = codec_available(kind) ? 1 : 0;
    return state[kind];
}

const char *vshot_still_load_error(void)
{
    StillApi *a = load_api();
    return a ? "" : api.error;
}

/// Declares the colour of a linear-light float frame: linear transfer, RGB
/// matrix, full range, and `primaries` (0 BT.709, 1 Display P3, 2 BT.2020, or
/// "unspecified" for a gamut the chromaticities box carries instead).
///
/// Without these libjxl warns that it is *assuming* BT.709 and sRGB and says
/// the colours may be wrong -- measured: it wrote exactly that, and the file
/// came back tagged bt709 whatever the frame held.  They are the difference
/// between a file that states its colour and one that guesses.
static void set_frame_colors(AVFrame *frame, int primaries)
{
    frame->color_range = AVCOL_RANGE_JPEG;
    frame->colorspace = AVCOL_SPC_RGB;
    frame->color_trc = AVCOL_TRC_LINEAR;
    switch (primaries) {
    case 0: frame->color_primaries = AVCOL_PRI_BT709; break;
    case 1: frame->color_primaries = AVCOL_PRI_SMPTE432; break;
    case 2: frame->color_primaries = AVCOL_PRI_BT2020; break;
    default: frame->color_primaries = AVCOL_PRI_UNSPECIFIED; break;
    }
}

static void put_be_float(uint8_t *out, float value)
{
    union { float f; uint32_t u; } bits = {.f = value};
    out[0] = (uint8_t)(bits.u >> 24);
    out[1] = (uint8_t)(bits.u >> 16);
    out[2] = (uint8_t)(bits.u >> 8);
    out[3] = (uint8_t)bits.u;
}

static float get_be_float(const uint8_t *in)
{
    union { float f; uint32_t u; } bits;
    bits.u = ((uint32_t)in[0] << 24) | ((uint32_t)in[1] << 16) |
             ((uint32_t)in[2] << 8) | (uint32_t)in[3];
    return bits.f;
}

/// Grows `bytes` by one ISO-BMFF-style box and returns the new length.
///
/// Appending past the container is what keeps the file readable elsewhere:
/// libjxl decodes a container with trailing boxes it does not know, measured
/// with `ffprobe` on a file carrying one.  Rewriting the container's own boxes
/// to make room would risk the one thing that must not break -- the codestream.
static int append_box(uint8_t **bytes, size_t *len, const char type[4],
                     const void *payload, uint32_t payload_len)
{
    if (*len > UINT32_MAX - payload_len - 8) return -1;
    uint8_t *grown = realloc(*bytes, *len + (size_t)payload_len + 8);
    if (!grown) return -1;
    *bytes = grown;
    const uint32_t box_len = payload_len + 8;
    grown[*len + 0] = (uint8_t)(box_len >> 24);
    grown[*len + 1] = (uint8_t)(box_len >> 16);
    grown[*len + 2] = (uint8_t)(box_len >> 8);
    grown[*len + 3] = (uint8_t)box_len;
    memcpy(grown + *len + 4, type, 4);
    memcpy(grown + *len + 8, payload, payload_len);
    *len += box_len;
    return 0;
}

/// Reads the boxes this program appended, if the file is a JPEG XL container
/// carrying them.  A bare codestream has nowhere to put them, and a file from
/// another writer simply leaves `reference_nits` at zero -- which the caller
/// reads as "no answer of its own" and falls back to the setting.
static void read_jxl_boxes(const uint8_t *bytes, size_t len, float *reference_nits,
                           int *custom, float chromaticities[6])
{
    if (len < 12 || memcmp(bytes, "\0\0\0\x0cJXL \r\n\x87\n", 12) != 0) return;
    size_t at = 12;
    while (at + 8 <= len) {
        const uint32_t box_len = ((uint32_t)bytes[at] << 24) | ((uint32_t)bytes[at + 1] << 16) |
                                 ((uint32_t)bytes[at + 2] << 8) | bytes[at + 3];
        if (box_len < 8 || box_len > len - at) return;
        const uint8_t *type = bytes + at + 4;
        const uint8_t *payload = bytes + at + 8;
        const uint32_t payload_len = box_len - 8;
        if (memcmp(type, VSHOT_REF_BOX, 4) == 0 && payload_len == 4) {
            *reference_nits = get_be_float(payload);
        } else if (memcmp(type, VSHOT_GAMUT_BOX, 4) == 0 && payload_len == 29) {
            *custom = payload[0] == 1;
            for (int i = 0; i < 6; i++) chromaticities[i] = get_be_float(payload + 5 + i * 4);
        }
        at += box_len;
    }
}

int vshot_still_encode(int kind, int width, int height, const uint8_t *rgba,
                       const float *hdr_rgba, float reference_nits, int primaries, int custom,
                       const float *chromaticities, int quality, int lossless, int effort,
                       float distance, uint8_t **out, size_t *out_len, char *err, size_t err_cap)
{
    *out = NULL;
    *out_len = 0;
    if (!out || !out_len || width <= 0 || height <= 0 || width > INT_MAX / 16 ||
        height > INT_MAX / (width * 16)) {
        snprintf(err, err_cap, "the image dimensions are outside what ffmpeg accepts");
        return -1;
    }
    if (!vshot_still_available(kind)) {
        snprintf(err, err_cap, "this ffmpeg build cannot write %s", encoder_name(kind));
        return -1;
    }
    StillApi *a = load_api();
    const AVCodec *codec = a->find_encoder(encoder_name(kind));
    AVCodecContext *ctx = a->alloc_context(codec);
    AVFrame *frame = a->frame_alloc();
    AVPacket *packet = a->packet_alloc();
    if (!ctx || !frame || !packet) {
        snprintf(err, err_cap, "could not allocate ffmpeg's encoder");
        goto fail;
    }
    ctx->width = width;
    ctx->height = height;
    ctx->time_base = (AVRational){1, 1};
    ctx->pix_fmt = encoder_pix_fmt(kind);

    AVDictionary *opts = NULL;
    if (kind == STILL_JPEG) {
        // MJPEG's quality is its quantizer bounds, not the generic
        // `global_quality`: measured on this ffmpeg, setting `global_quality`
        // with `AV_CODEC_FLAG_QSCALE` (the documented fixed-quality path) left
        // the file at 24454 bytes whatever the value, while `qmin`/`qmax` moved
        // it from 33005 to 7450.  Only the bounds are read here.
        //
        // The scale runs the opposite way to the quality a user means -- 2 is
        // the best and 31 the worst -- so it is inverted on the way in.
        int qscale = 31 - ((quality - 1) * 29 + 49) / 99;
        if (qscale < 2) qscale = 2;
        if (qscale > 31) qscale = 31;
        ctx->qmin = qscale;
        ctx->qmax = qscale;
    } else if (kind == STILL_WEBP) {
        a->dict_set(&opts, "lossless", lossless ? "1" : "0", 0);
        char value[24];
        snprintf(value, sizeof(value), "%d", quality);
        a->dict_set(&opts, "quality", value, 0);
    } else {
        char value[32];
        snprintf(value, sizeof(value), "%d", effort);
        a->dict_set(&opts, "effort", value, 0);
        // Distance is Butteraugli's: zero is lossless, and it is the only
        // quality knob libjxl exposes.  Measured on a 16x16 float frame, 0 kept
        // all 1024 samples bit-exact where 3 wrote a file a quarter the size.
        snprintf(value, sizeof(value), "%.6g", (double)distance);
        a->dict_set(&opts, "distance", value, 0);
    }
    if (a->open(ctx, codec, &opts) < 0) {
        snprintf(err, err_cap, "ffmpeg refused the %s encoder", encoder_name(kind));
        a->dict_free(&opts);
        goto fail;
    }
    a->dict_free(&opts);

    frame->format = ctx->pix_fmt;
    frame->width = width;
    frame->height = height;
    frame->pts = 0;
    if (kind == STILL_JXL) {
        if (!hdr_rgba) {
            snprintf(err, err_cap, "JPEG XL needs the HDR half's linear pixels");
            goto fail;
        }
        set_frame_colors(frame, custom ? -1 : primaries);
    }
    if (a->frame_get_buffer(frame, 32) < 0) {
        snprintf(err, err_cap, "could not allocate the frame ffmpeg encodes from");
        goto fail;
    }

    if (kind == STILL_JXL) {
        for (int y = 0; y < height; y++)
            memcpy(frame->data[0] + (size_t)y * frame->linesize[0],
                   hdr_rgba + (size_t)y * width * 4, (size_t)width * 4 * sizeof(float));
    } else if (kind == STILL_WEBP) {
        // The frame is RGBA and the encoder asks for BGRA: only the byte order
        // differs, so the channels are swapped rather than converted.  WebP
        // carries alpha, so it is kept -- unlike JPEG, below.
        for (int y = 0; y < height; y++) {
            const uint8_t *src = rgba + (size_t)y * width * 4;
            uint8_t *dst = frame->data[0] + (size_t)y * frame->linesize[0];
            for (int x = 0; x < width; x++) {
                dst[x * 4 + 0] = src[x * 4 + 2];
                dst[x * 4 + 1] = src[x * 4 + 1];
                dst[x * 4 + 2] = src[x * 4 + 0];
                dst[x * 4 + 3] = src[x * 4 + 3];
            }
        }
    } else {
        // JPEG has no alpha channel, so a translucent pixel has to become a
        // solid one before it is encoded.  It is composited over black: a
        // screenshot's透明 area is the desktop behind it showing through, and
        // black is what a viewer that does not composite shows anyway -- the
        // alternative, white, would turn a dark capture's edges into a frame.
        for (int y = 0; y < height; y++) {
            const uint8_t *src = rgba + (size_t)y * width * 4;
            uint8_t *yy = frame->data[0] + (size_t)y * frame->linesize[0];
            uint8_t *uu = frame->data[1] + (size_t)y * frame->linesize[1];
            uint8_t *vv = frame->data[2] + (size_t)y * frame->linesize[2];
            for (int x = 0; x < width; x++) {
                const int alpha = src[x * 4 + 3];
                const int r = (src[x * 4 + 0] * alpha + 127) / 255;
                const int g = (src[x * 4 + 1] * alpha + 127) / 255;
                const int b = (src[x * 4 + 2] * alpha + 127) / 255;
                yy[x] = (uint8_t)((77 * r + 150 * g + 29 * b + 128) >> 8);
                uu[x] = (uint8_t)(128 + ((-43 * r - 85 * g + 128 * b + 128) >> 8));
                vv[x] = (uint8_t)(128 + ((128 * r - 107 * g - 21 * b + 128) >> 8));
            }
        }
    }

    if (a->send_frame(ctx, frame) < 0) {
        snprintf(err, err_cap, "ffmpeg would not take the frame");
        goto fail;
    }
    if (a->receive_packet(ctx, packet) < 0 || packet->size <= 0) {
        snprintf(err, err_cap, "ffmpeg produced no image");
        goto fail;
    }

    // The packet is the file -- see the comment at the top of this file.
    size_t len = (size_t)packet->size;
    uint8_t *result = malloc(len + (kind == STILL_JXL ? 45 : 0));
    if (!result) {
        snprintf(err, err_cap, "out of memory copying the encoded image");
        goto fail;
    }
    memcpy(result, packet->data, len);

    if (kind == STILL_JXL) {
        uint8_t white[4];
        put_be_float(white, reference_nits);
        if (append_box(&result, &len, VSHOT_REF_BOX, white, sizeof(white)) != 0) {
            free(result);
            snprintf(err, err_cap, "could not write the reference white");
            goto fail;
        }
        if (custom) {
            uint8_t gamut[29] = {1, (uint8_t)primaries};
            if (chromaticities) {
                for (int i = 0; i < 6; i++) put_be_float(gamut + 5 + i * 4, chromaticities[i]);
            }
            if (append_box(&result, &len, VSHOT_GAMUT_BOX, gamut, sizeof(gamut)) != 0) {
                free(result);
                snprintf(err, err_cap, "could not write the gamut");
                goto fail;
            }
        }
    }
    *out = result;
    *out_len = len;
    a->packet_free(&packet);
    a->frame_free(&frame);
    a->free_context(&ctx);
    return 0;
fail:
    if (packet) a->packet_free(&packet);
    if (frame) a->frame_free(&frame);
    if (ctx) a->free_context(&ctx);
    return -1;
}

/// The pixel format a JPEG XL decode should hand back.
///
/// The decoder offers several and would pick a byte format by default, which
/// would throw away everything above SDR white: an HDR half is linear `f32`.
static enum AVPixelFormat choose_float_rgb(AVCodecContext *ctx, const enum AVPixelFormat *fmts)
{
    (void)ctx;
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++)
        if (*p == AV_PIX_FMT_RGBAF32LE) return *p;
    return fmts[0];
}

int vshot_still_decode_jxl(const uint8_t *bytes, size_t len, float fallback_nits,
                           float **rgba_out, int *width, int *height, int *primaries,
                           int *custom, float *reference_nits, float chromaticities[6],
                           char *err, size_t err_cap)
{
    *rgba_out = NULL;
    if (!bytes || !len || !rgba_out || !width || !height || !primaries || !custom ||
        !reference_nits) {
        snprintf(err, err_cap, "JPEG XL decode was called without somewhere to put the image");
        return -1;
    }
    if (!vshot_still_available(STILL_JXL)) {
        snprintf(err, err_cap, "this ffmpeg build cannot read JPEG XL");
        return -1;
    }
    *reference_nits = 0.0f;
    *custom = 0;
    read_jxl_boxes(bytes, len, reference_nits, custom, chromaticities);

    StillApi *a = load_api();
    const AVCodec *codec = a->find_decoder("libjxl");
    AVCodecContext *ctx = a->alloc_context(codec);
    AVPacket *packet = a->packet_alloc();
    AVFrame *frame = a->frame_alloc();
    if (!ctx || !packet || !frame) {
        snprintf(err, err_cap, "could not allocate ffmpeg's decoder");
        goto fail;
    }
    ctx->get_format = choose_float_rgb;
    if (a->open(ctx, codec, NULL) < 0 || len > INT_MAX || a->packet_new(packet, (int)len) < 0) {
        snprintf(err, err_cap, "could not open ffmpeg's JPEG XL decoder");
        goto fail;
    }
    memcpy(packet->data, bytes, len);
    // Decoders read past the end of the packet; the padding is what stops that
    // being a read off the end of our buffer.
    memset(packet->data + len, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    if (a->send_packet(ctx, packet) < 0) {
        snprintf(err, err_cap, "ffmpeg would not take the JPEG XL file");
        goto fail;
    }
    // One image in, one frame out, and it only comes out once the decoder has
    // been pushed with an end-of-stream packet.
    a->send_packet(ctx, NULL);
    int got = a->receive_frame(ctx, frame);
    if (got == AVERROR(EAGAIN)) got = a->receive_frame(ctx, frame);
    if (got < 0) {
        snprintf(err, err_cap, "ffmpeg could not decode the JPEG XL file");
        goto fail;
    }
    if (frame->format != AV_PIX_FMT_RGBAF32LE || frame->width <= 0 || frame->height <= 0 ||
        (size_t)frame->width > SIZE_MAX / (size_t)frame->height / (4 * sizeof(float)) ||
        (size_t)frame->width * (size_t)frame->height > MAX_JXL_PIXELS) {
        // Anything but linear float is refused rather than converted: a JPEG XL
        // that does not hold the light cannot be turned back into an HDR half,
        // and guessing at a gamut would silently recolour the capture.
        snprintf(err, err_cap, "the JPEG XL file is not linear float, or is too large");
        goto fail;
    }
    *width = frame->width;
    *height = frame->height;
    *rgba_out = malloc((size_t)*width * (size_t)*height * 4 * sizeof(float));
    if (!*rgba_out) {
        snprintf(err, err_cap, "out of memory copying the decoded image");
        goto fail;
    }
    for (int y = 0; y < *height; y++)
        memcpy(*rgba_out + (size_t)y * (*width) * 4,
               frame->data[0] + (size_t)y * frame->linesize[0],
               (size_t)(*width) * 4 * sizeof(float));

    // A file that names no white of its own is read at the setting, which is
    // what `--hdr-reference-white` is for.  A file that does is read at its own.
    if (!(*reference_nits > 0.0f) || *reference_nits > 10000.0f) {
        *reference_nits = fallback_nits;
    }
    if (!*custom) {
        switch (frame->color_primaries) {
        case AVCOL_PRI_BT709: *primaries = 0; break;
        case AVCOL_PRI_SMPTE432: *primaries = 1; break;
        case AVCOL_PRI_BT2020: *primaries = 2; break;
        default:
            // Not a gamut this side can name.  Saying so is better than reading
            // the file as BT.709 and showing a wide-gamut capture wrong.
            free(*rgba_out);
            *rgba_out = NULL;
            snprintf(err, err_cap, "the JPEG XL file names a gamut this build cannot read");
            goto fail;
        }
    }
    a->frame_free(&frame);
    a->packet_free(&packet);
    a->free_context(&ctx);
    return 0;
fail:
    if (frame) a->frame_free(&frame);
    if (packet) a->packet_free(&packet);
    if (ctx) a->free_context(&ctx);
    return -1;
}

void vshot_still_free(void *bytes) { free(bytes); }
