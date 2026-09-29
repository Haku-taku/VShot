// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Remembered defaults, read from the same `config.json` the Qt helper writes.
//!
//! The file is shared with the helper, which owns the `editor` section — the
//! style the user last left the annotation editor in.  This side reads only
//! the `cli` section, which the helper leaves alone.
//!
//! A config file is a convenience, never a requirement: a missing, unreadable,
//! or malformed file yields the built-in defaults, and a bad value inside a
//! valid file falls back field by field.  Nothing here can fail a capture.

use std::path::PathBuf;

use serde::Deserialize;

use crate::model::PngCompression;

/// The defaults a command-line flag falls back to when it is not given.
///
/// The JSON keys are the flag names, so `--png-compression` is written
/// `"png-compression"` and `--max-height` is `"max-height"`; that is what a
/// user editing the file by hand would reach for, and it matches the Qt side's
/// camelCase habit of naming things as they appear in the UI.
///
/// Unknown keys are ignored rather than rejected: this is a file the user may
/// edit by hand and that a newer vshot may write, and refusing the whole
/// section over one unrecognized name would silently drop every default in it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CliDefaults {
    /// `--png-compression`, one of `none` / `fastest` / `fast` / `balanced` /
    /// `high`.
    pub png_compression: Option<String>,
    /// `monitor`'s output name when none is given; `current` means the output
    /// under the pointer.
    pub monitor: Option<String>,
    pub long: LongDefaults,
    pub pin: PinDefaults,
    pub ocr: OcrDefaults,
    pub translate: TranslateDefaults,
    pub record: RecordDefaults,
    pub replay: ReplayDefaults,
}

/// Defaults for `vshot record`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RecordDefaults {
    /// The microphone to record when `--mic` is given without a name, or
    /// when the command line says neither `--mic` nor `--no-mic`: an empty
    /// string is the session's default source, a name is a PipeWire node.
    /// `null` (the default) means a recording is silent.
    pub mic: Option<String>,
    /// The codec `--encoder` falls back to: `h264` (the built-in default),
    /// `hevc` or `av1`.
    pub encoder: Option<String>,
    /// The hardware encoder `--encoder-backend` falls back to: `auto` (the
    /// built-in default, VAAPI where it opens else Vulkan else NVENC),
    /// `vaapi`, `vulkan` or `nvenc`.
    pub encoder_backend: Option<String>,
    /// The frame rate `--fps` falls back to, 1-240.
    pub fps: Option<u32>,
    /// Whether a recording goes through the desktop portal without
    /// `--portal`.  `null` (the default) keeps the compositor's own
    /// protocols; `--no-portal` overrides a remembered `true`.
    pub portal: Option<bool>,
    /// The windows a `record window` moves between without `--follow`: the
    /// names are the same ones the flag takes, and the fallback is read only
    /// when the target is a plain `window` (no NAME, no `--pick`) — a
    /// remembered follow must not turn a `record monitor` into an error, so
    /// every other target ignores it.  `null` (the default) means whatever the
    /// focus is on stays recorded; `--no-follow` turns a remembered list off
    /// for one recording.
    pub follow: Option<Vec<String>>,
    /// Whether a finished recording raises a desktop notification.  A
    /// recording started from a keybinding ends with nothing on screen to
    /// mark the moment, so the notification is its receipt; that is why the
    /// absent key means yes rather than no.  Like `ocr.notify` this is a
    /// settings-window and hand-edit key with no flag of its own, and only
    /// `false` is ever written.
    pub notify: Option<bool>,
}

/// Defaults for `vshot replay`: the memory-replay settings the flags fall back
/// to when they are not given.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ReplayDefaults {
    /// How many seconds of history the ring keeps; `--window` overrides it.
    /// 30 unless the file says otherwise.
    pub window: Option<u64>,
    /// The codec `--encoder` falls back to for a replay: `h264` (the built-in
    /// default), `hevc` or `av1`.
    pub encoder: Option<String>,
    /// The hardware encoder `--encoder-backend` falls back to for a replay:
    /// `auto` (the default), `vaapi`, `vulkan` or `nvenc`.
    pub encoder_backend: Option<String>,
    /// The frame rate `--fps` falls back to, 1-240.
    pub fps: Option<u32>,
    /// The key-frame distance in seconds (1-10): a smaller value makes a save
    /// start closer to the requested edge at the cost of a bigger ring.
    pub gop: Option<u64>,
    /// The microphone the replay keeps in the ring, if any: an empty string is
    /// the session's default source, a name is a PipeWire node.  `null` (the
    /// default) means the replay is silent.
    pub mic: Option<String>,
    /// Whether a replay uses the desktop portal instead of the compositor's
    /// own protocols, as `record --portal` does.
    pub portal: Option<bool>,
    /// The windows a `replay start window` moves between without `--follow`,
    /// read on the same terms as `record.follow`: only a plain `window` target
    /// falls back to them, and `--no-follow` turns a remembered list off for
    /// one session.
    pub follow: Option<Vec<String>>,
    /// Where a triggered save lands; strftime is expanded.  The videos
    /// directory with a timestamped name when unset.
    pub save_dir: Option<String>,
    /// Whether a save raises a desktop notification.  Absent means yes.
    pub notify: Option<bool>,
}

/// Text recognition, used by `vshot ocr` and the editor's text tool.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct OcrDefaults {
    /// `builtin` (the default) or `external`.
    pub engine: Option<String>,
    /// Only read when `engine` is `external`.
    pub external: Option<OcrExternalDefaults>,
    /// Whether a finished recognition raises a desktop notification.  Absent
    /// means yes; the settings window writes it out.
    pub notify: Option<bool>,
}

/// How to reach an OCR program the user runs themselves.
///
/// This is the way to use a GPU without vshot linking a GPU runtime: the
/// program may be anything that reads a PNG and writes text, and vshot only
/// has to start it and read its stdout.  A user with a ROCm or CUDA build of
/// ONNX Runtime — or a machine elsewhere on the network — points this at it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct OcrExternalDefaults {
    /// The program and its arguments, as an array.  The image's path is
    /// appended unless `stdin` is set.
    pub command: Option<Vec<String>>,
    /// Send the PNG on stdin instead of naming a file on the command line.
    pub stdin: Option<bool>,
    /// Seconds to wait before giving up.  Defaults to 30.
    pub timeout: Option<u64>,
}

/// Text translation, used by `vshot translate` and the editor's translate
/// mode.  Absent keys mean the built-in defaults; the section is entirely
/// optional and `google` needs no account at all.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateDefaults {
    /// Which service: `google` (the default), `microsoft`, `volcengine`,
    /// `transmart`, `lingocloud`, `bing`, `baidu`, `ai` or `external`, or
    /// `auto` to try the usable ones in that order.  The keyless-by-config ones
    /// need no settings; `lingocloud` borrows a token unless one is given below.
    pub provider: Option<String>,
    /// Source language, in vshot's own BCP-47-ish spelling (`zh-Hans`,
    /// `zh-Hant`, `en`, `ja`, `ko`, ...); `auto` (the default) detects it.
    /// Each provider is handed its own code for the tag (see `translate.rs`).
    pub from: Option<String>,
    /// Target language, written the same way.  `zh-Hans` by default.
    pub to: Option<String>,
    /// Seconds before a provider's HTTP request is given up on.  Defaults to
    /// 20; `external` has its own timeout below.
    pub timeout: Option<u64>,
    /// Whether a finished translation raises a desktop notification.  Absent
    /// means no — unlike `ocr.notify`, the Qt editor drives translation while
    /// its window is on screen, so a note is noise more often than a receipt.
    /// The `--stdin-ocr` route never notifies at all.
    pub notify: Option<bool>,
    /// More providers to try, in order, when the one that ran produced no
    /// usable result — a name from the same set as `provider`.  Empty by
    /// default, so nothing runs that was not asked for; a name that repeats
    /// one already in the chain is skipped, and an unknown name is an error.
    pub fallback: Option<Vec<String>>,
    /// The tokenless Google endpoint takes no settings.  The key exists only so
    /// the default provider has a named place in the file; it is an empty
    /// object.
    pub google: Option<TranslateGoogleDefaults>,
    /// Caiyun's endpoint (the `lingocloud` provider).  Optional: the built-in
    /// borrowed token is used when no token is given here.
    pub lingocloud: Option<TranslateLingocloudDefaults>,
    /// Azure Translator (the `bing` provider).
    pub bing: Option<TranslateBingDefaults>,
    /// Baidu's fanyi API.
    pub baidu: Option<TranslateBaiduDefaults>,
    /// Any OpenAI-compatible chat-completions endpoint.
    pub ai: Option<TranslateAiDefaults>,
    /// A program of the user's own, run the way the external OCR engine is.
    pub external: Option<TranslateExternalDefaults>,
}

/// Google needs nothing; the struct is here so the default provider has a named
/// place in the file, next to the ones that do take settings.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateGoogleDefaults {}

/// Caiyun's `lingocloud` endpoint.  It needs nothing to run — a token is built
/// in — and this key exists so it does not have to keep borrowing that one: the
/// built-in is lifted from Caiyun's web app and passed between third-party
/// forks, and Caiyun can revoke it at any time.  A user with a Caiyun account
/// of their own puts their token here and stops borrowing (see the README's
/// Translation section); the request's `X-Authorization` header is built as
/// `token <value>`, so the value is the token itself.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateLingocloudDefaults {
    /// The token, without the `token ` scheme the header carries.  Absent or
    /// blank leaves the built-in borrowed token in place.
    pub token: Option<String>,
}

/// Azure Translator's credentials and endpoint.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateBingDefaults {
    /// The resource endpoint; the default is the global
    /// `https://api.cognitive.microsofttranslator.com`.
    pub endpoint: Option<String>,
    /// The subscription key, sent as `Ocp-Apim-Subscription-Key`.  Required.
    pub api_key: Option<String>,
    /// A single-region resource's region, sent as
    /// `Ocp-Apim-Subscription-Region`.  Optional.
    pub region: Option<String>,
}

/// Baidu's fanyi credentials.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateBaiduDefaults {
    /// The developer app's app id.  Required.
    pub app_id: Option<String>,
    /// The app's secret, used only to sign the request.  Required.
    pub secret_key: Option<String>,
}

/// An OpenAI-compatible chat endpoint.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateAiDefaults {
    /// The base URL, or the full `/chat/completions` URL.  Required.
    pub endpoint: Option<String>,
    /// The bearer token.  Required.
    pub api_key: Option<String>,
    /// The model name.  Required.
    pub model: Option<String>,
    /// Replaces the built-in system prompt when non-empty.
    pub prompt: Option<String>,
}

/// How to reach a translation program the user runs themselves.
///
/// The same escape hatch the OCR side has: the program reads the source lines
/// on stdin and writes the translations on stdout, one per line, and vshot
/// only starts it and checks that one line came back for each one sent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TranslateExternalDefaults {
    /// The program and its arguments, as an array.
    pub command: Option<Vec<String>>,
    /// Seconds to wait before giving up.  Defaults to 30.
    pub timeout: Option<u64>,
}

/// Defaults for `vshot long`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LongDefaults {
    pub notches: Option<u32>,
    pub max_height: Option<u32>,
    pub max_frames: Option<u32>,
    pub timeout: Option<u64>,
    pub ignore_top: Option<u32>,
    /// Scroll backend: `auto` / `wlr` / `portal` / `uinput`.
    pub inject: Option<String>,
}

/// Defaults for `vshot pin`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PinDefaults {
    /// `--density` for every pinned image.
    pub density: Option<u32>,
}

/// The whole file, as far as this side is concerned.  The helper's `editor`
/// section is deliberately not modelled: it is the helper's to read and write,
/// and an unknown section must not make the file "malformed" here.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    cli: CliDefaults,
}

/// The path of the config file: `$XDG_CONFIG_HOME/vshot/config.json`, falling
/// back to `$HOME/.config/vshot/config.json`.  `None` when neither variable is
/// set, in which case there is nothing to read.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("vshot").join("config.json"))
}

/// Reads the remembered command-line defaults.  Every failure — no file, no
/// path, unreadable, unparseable — is the built-in default.
pub fn load() -> CliDefaults {
    let Some(path) = config_path() else {
        return CliDefaults::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return CliDefaults::default();
    };
    serde_json::from_str::<ConfigFile>(&text)
        .map(|file| file.cli)
        .unwrap_or_default()
}

/// The compression the config file remembers, or `None` when it says nothing
/// usable.  The caller keeps its own built-in default.
pub fn compression_default() -> Option<PngCompression> {
    parse_compression(load().png_compression.as_deref()?)
}

/// The names are the ones `--png-compression` accepts, so a value copied out
/// of `vshot --help` works in the file unchanged.
fn parse_compression(name: &str) -> Option<PngCompression> {
    match name {
        "none" => Some(PngCompression::None),
        "fastest" => Some(PngCompression::Fastest),
        "fast" => Some(PngCompression::Fast),
        "balanced" => Some(PngCompression::Balanced),
        "high" => Some(PngCompression::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compression_name_the_cli_accepts_is_parsed() {
        for name in ["none", "fastest", "fast", "balanced", "high"] {
            assert!(parse_compression(name).is_some(), "{name} must parse");
        }
        // An unknown name is a typo in a hand-edited file: ignore it and let
        // the built-in default stand.
        assert!(parse_compression("slowest").is_none());
    }

    #[test]
    fn a_config_without_the_cli_section_reads_as_defaults() {
        let file: ConfigFile = serde_json::from_str(r##"{"editor":{"color":"#ff0000"}}"##)
            .expect("an editor-only file is valid");
        assert!(file.cli.png_compression.is_none());
        assert!(file.cli.long.notches.is_none());
        assert!(file.cli.pin.density.is_none());
    }

    #[test]
    fn the_ocr_section_carries_the_notification_switch() {
        let file: ConfigFile = serde_json::from_str(
            r#"{"cli":{"ocr":{"notify":false,"engine":"external","external":{"command":["x"]}}}}"#,
        )
        .expect("an ocr section parses");
        assert_eq!(file.cli.ocr.notify, Some(false));
        // The switch does not displace the rest of the section.
        assert_eq!(file.cli.ocr.engine.as_deref(), Some("external"));
        // Absent is not "off": the caller's own default -- on -- is what
        // stands, which is what a file written before this key existed says.
        let silent: ConfigFile =
            serde_json::from_str(r#"{"cli":{"ocr":{"engine":"builtin"}}}"#).unwrap();
        assert_eq!(silent.cli.ocr.notify, None);
    }

    #[test]
    fn cli_values_are_read_field_by_field() {
        let file: ConfigFile = serde_json::from_str(
            r#"{"cli":{"png-compression":"high","long":{"notches":3,"max-height":1000}}}"#,
        )
        .expect("a cli section parses");
        assert_eq!(file.cli.png_compression.as_deref(), Some("high"));
        assert_eq!(file.cli.long.notches, Some(3));
        assert_eq!(file.cli.long.max_height, Some(1000));
        // Absent fields stay absent rather than becoming zero, so the caller's
        // own default is what stands.
        assert!(file.cli.long.timeout.is_none());
        assert!(file.cli.monitor.is_none());
        // The microphone is off unless the file says otherwise: `null` and
        // an absent key are the same silence, an empty string is the
        // session's default source, and a name is that input.
        assert!(file.cli.record.mic.is_none());
        let file: ConfigFile =
            serde_json::from_str(r#"{"cli":{"record":{"mic":""}}}"#).expect("an empty name parses");
        assert_eq!(file.cli.record.mic.as_deref(), Some(""));
        let file: ConfigFile =
            serde_json::from_str(r#"{"cli":{"record":{"mic":"55"}}}"#).expect("a serial parses");
        assert_eq!(file.cli.record.mic.as_deref(), Some("55"));
    }

    #[test]
    fn the_record_section_carries_the_encoder_fps_and_portal() {
        let file: ConfigFile = serde_json::from_str(
            r#"{"cli":{"record":{"encoder":"hevc","fps":120,"portal":true,"mic":"alsa_input.x"}}}"#,
        )
        .expect("a record section parses");
        assert_eq!(file.cli.record.encoder.as_deref(), Some("hevc"));
        assert_eq!(file.cli.record.fps, Some(120));
        assert_eq!(file.cli.record.portal, Some(true));
        assert_eq!(file.cli.record.mic.as_deref(), Some("alsa_input.x"));

        // Absent is absent: the caller's own default is what stands.
        let bare: ConfigFile = serde_json::from_str(r#"{"cli":{"record":{}}}"#)
            .expect("an empty record section parses");
        assert!(bare.cli.record.encoder.is_none());
        assert!(bare.cli.record.fps.is_none());
        assert!(bare.cli.record.portal.is_none());
        assert!(bare.cli.record.mic.is_none());
        assert!(bare.cli.record.follow.is_none());
    }

    #[test]
    fn the_record_section_remembers_the_windows_to_follow() {
        // The follow list is the one value here that is not a scalar: the
        // settings window writes several windows at once, and a session that
        // read only the first would follow the wrong one of them.
        let file: ConfigFile =
            serde_json::from_str(r#"{"cli":{"record":{"follow":["game","chat"]}}}"#)
                .expect("a follow list parses");
        assert_eq!(
            file.cli.record.follow.as_deref(),
            Some(["game".to_owned(), "chat".to_owned()].as_slice())
        );
        // An empty list is a value the file can carry, and it means the same
        // thing as the absent key: nothing to follow.
        let empty: ConfigFile = serde_json::from_str(r#"{"cli":{"record":{"follow":[]}}}"#)
            .expect("an empty list parses");
        assert_eq!(empty.cli.record.follow.as_deref(), Some([].as_slice()));
    }

    #[test]
    fn the_replay_section_carries_its_own_follow_and_save_places() {
        let file: ConfigFile = serde_json::from_str(
            r#"{"cli":{"replay":{"follow":["game"],"save-dir":"/tmp/clips","notify":false}}}"#,
        )
        .expect("a replay section parses");
        assert_eq!(
            file.cli.replay.follow.as_deref(),
            Some(["game".to_owned()].as_slice())
        );
        assert_eq!(file.cli.replay.save_dir.as_deref(), Some("/tmp/clips"));
        assert_eq!(file.cli.replay.notify, Some(false));
        // The recording side's follow is its own field: a replay list must not
        // appear as one.
        assert!(file.cli.record.follow.is_none());
    }

    #[test]
    fn the_translate_section_carries_the_provider_settings() {
        let file: ConfigFile = serde_json::from_str(
            r#"{"cli":{"translate":{
                "provider":"baidu","from":"ja","to":"zh-Hans","timeout":40,"notify":true,
                "fallback":["bing","google"],
                "google":{},
                "lingocloud":{"token":"mine"},
                "bing":{"endpoint":"https://x","api-key":"k","region":"westus"},
                "baidu":{"app-id":"a","secret-key":"s"},
                "ai":{"endpoint":"https://y","api-key":"k","model":"m","prompt":"p"},
                "external":{"command":["/bin/tr"],"timeout":5}
            }}}"#,
        )
        .expect("a translate section parses");
        let translate = &file.cli.translate;
        assert_eq!(translate.provider.as_deref(), Some("baidu"));
        assert_eq!(translate.from.as_deref(), Some("ja"));
        assert_eq!(translate.to.as_deref(), Some("zh-Hans"));
        assert_eq!(translate.timeout, Some(40));
        assert_eq!(translate.notify, Some(true));
        assert_eq!(
            translate.fallback.as_deref(),
            Some(["bing".to_owned(), "google".to_owned()].as_slice())
        );
        // `google` is the empty object that gives the default provider a named
        // place next to the ones that take settings.
        assert!(translate.google.is_some());
        // `lingocloud` is optional and carries only a token override.
        let lingocloud = translate.lingocloud.as_ref().unwrap();
        assert_eq!(lingocloud.token.as_deref(), Some("mine"));
        let bing = translate.bing.as_ref().unwrap();
        assert_eq!(bing.endpoint.as_deref(), Some("https://x"));
        assert_eq!(bing.api_key.as_deref(), Some("k"));
        assert_eq!(bing.region.as_deref(), Some("westus"));
        let baidu = translate.baidu.as_ref().unwrap();
        assert_eq!(baidu.app_id.as_deref(), Some("a"));
        assert_eq!(baidu.secret_key.as_deref(), Some("s"));
        let ai = translate.ai.as_ref().unwrap();
        assert_eq!(ai.endpoint.as_deref(), Some("https://y"));
        assert_eq!(ai.model.as_deref(), Some("m"));
        assert_eq!(ai.prompt.as_deref(), Some("p"));
        let external = translate.external.as_ref().unwrap();
        assert_eq!(
            external.command.as_deref(),
            Some(["/bin/tr".to_owned()].as_slice())
        );
        assert_eq!(external.timeout, Some(5));

        // An absent section is the built-in defaults, not a failure.
        let bare: ConfigFile = serde_json::from_str(r#"{"cli":{}}"#).unwrap();
        assert!(bare.cli.translate.provider.is_none());
        assert!(bare.cli.translate.bing.is_none());
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn the_config_path_honours_xdg_config_home() {
        // SAFETY: single-threaded test body, and the variable is restored
        // before returning.
        let saved = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/vshot-cfg-probe");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/tmp/vshot-cfg-probe/vshot/config.json"))
        );
        match saved {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
