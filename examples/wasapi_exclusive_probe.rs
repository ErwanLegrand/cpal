//! Diagnoses *why* a WASAPI endpoint refuses exclusive mode.
//!
//! `cpal`'s exclusive-mode support answers "can this device do exclusive mode?" with a yes or a
//! no. When the answer is no, that is not enough to act on: an endpoint can refuse because the
//! driver implements no exclusive-mode format at all, because the *shape* `cpal` offers is not the
//! one it wants, or because something outside the format negotiation says no. This example asks
//! the endpoint directly and prints the evidence for each of those.
//!
//! Two shapes in particular are things `cpal` cannot currently express, and both look identical to
//! "this device has no exclusive mode" from the outside:
//!
//! - **A positional channel mask.** In shared mode the Windows audio engine accepts
//!   `dwChannelMask = KSAUDIO_SPEAKER_DIRECTOUT` (0); in exclusive mode the format goes to the
//!   driver directly, and a driver that wants real speaker positions answers
//!   `AUDCLNT_E_UNSUPPORTED_FORMAT` to a zero mask while accepting the identical format with
//!   `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT`. `config_to_waveformatextensible` sent 0 for every
//!   format until `channel_mask_for` was added; it now sends a positional mask in exclusive mode.
//! - **Packed 24-bit.** `SampleFormat::I24` is 24 valid bits in a *4-byte* container, and
//!   `config_to_waveformatextensible` can emit nothing else. An endpoint that wants
//!   `wBitsPerSample = 24` with a 3-byte frame per channel is never offered it.
//!
//! So the matrix has three axes — sample rate x format x `dwChannelMask` — and the mask axis
//! always includes both the mask `cpal` sends today and the standard positional mask for the
//! device's channel count. A cell that flips on the mask alone is a bug in `cpal`'s exclusive-mode
//! path rather than a device limitation, and the report says so in as many words.
//!
//! For every render and capture endpoint it prints:
//!
//! 1. Identity — friendly name, data flow, device state and endpoint id.
//! 2. `GetMixFormat()`, decoded field by field. This describes the *shared-mode engine*, not the
//!    endpoint, so it is context and never an exclusive-mode answer.
//! 3. `GetDevicePeriod()`, default and minimum, in hns and in frames at each probed rate.
//! 4. The format matrix: one table per channel mask, then the cells whose answers disagree.
//! 5. A verdict line.
//!
//! What this example deliberately does not do: it never calls `IAudioClient::Initialize`. Every
//! call here is a query, so running it cannot take a device away from another application — and
//! equally, it cannot observe `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`, which only `Initialize`
//! reports. The device-period block exists so the frame counts that feed that path can be read
//! off directly.
//!
//! Output is plain stdout, one block per device, in a stable order (render endpoints then capture
//! endpoints, each sorted by endpoint id), so a run can be pasted into a bug report as-is.
//!
//! Run with: `cargo run --example wasapi_exclusive_probe`

#[cfg(not(target_os = "windows"))]
fn main() {
    println!(
        "wasapi_exclusive_probe: WASAPI is a Windows API, so there is nothing to probe on this \
         platform. Build and run this example on Windows."
    );
}

#[cfg(target_os = "windows")]
fn main() {
    windows_probe::run();
}

#[cfg(target_os = "windows")]
mod windows_probe {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, slice};

    use windows::{
        core::{Interface, GUID, HRESULT, PWSTR},
        Win32::{
            Devices::Properties,
            Foundation::{E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, PROPERTYKEY, S_FALSE, S_OK},
            Media::{Audio, KernelStreaming, Multimedia},
            System::{
                Com::{self, StructuredStorage},
                Variant::VT_LPWSTR,
            },
            UI::Shell::PropertiesSystem::IPropertyStore,
        },
    };

    /// Sample rates probed, in the order the matrix columns appear.
    const PROBE_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

    /// `cbSize` for a `WAVEFORMATEXTENSIBLE`: the number of bytes that follow the 18-byte
    /// `WAVEFORMATEX` header. This is the single most damaging field to get wrong — a driver that
    /// cannot find the extensible payload rejects every format, which reads exactly like a device
    /// with no exclusive-mode support and would also destroy the channel-mask comparison — so it
    /// is derived from the layout and checked at compile time rather than written down as 22.
    const CB_SIZE_EXTENSIBLE: u16 =
        (size_of::<Audio::WAVEFORMATEXTENSIBLE>() - size_of::<Audio::WAVEFORMATEX>()) as u16;

    // Both structures are `#[repr(C, packed(1))]`, so these are the wire sizes the driver expects.
    const _: () = assert!(size_of::<Audio::WAVEFORMATEX>() == 18);
    const _: () = assert!(size_of::<Audio::WAVEFORMATEXTENSIBLE>() == 40);
    const _: () = assert!(CB_SIZE_EXTENSIBLE == 22);

    /// Width of the horizontal rules, and the column the prose is wrapped at. Chosen so a matrix
    /// row fits without wrapping.
    const RULE: usize = 100;

    /// Prints `text` wrapped at [`RULE`] columns, `prefix` on the first line and `indent` on the
    /// rest. Verdicts are whole sentences and would otherwise be one very long line, which is
    /// exactly what does not survive being pasted into a bug report.
    fn print_wrapped(prefix: &str, indent: &str, text: &str) {
        let mut line = String::from(prefix);
        let mut has_word = false;
        for word in text.split_whitespace() {
            if has_word && line.chars().count() + 1 + word.chars().count() > RULE {
                println!("{line}");
                line = String::from(indent);
                has_word = false;
            }
            if has_word {
                line.push(' ');
            }
            line.push_str(word);
            has_word = true;
        }
        if has_word {
            println!("{line}");
        }
    }

    /// The channel mask `cpal` sends in **exclusive** mode, mirroring `channel_mask_for`
    /// (`src/host/wasapi/device.rs`) for the two-channel case.
    ///
    /// This is a hand-kept copy, and that is a real hazard worth stating: this example builds
    /// every `WAVEFORMATEXTENSIBLE` itself and never calls `config_to_waveformatextensible`, so
    /// nothing makes the two agree. An earlier revision of this file hard-coded
    /// `KSAUDIO_SPEAKER_DIRECTOUT` and kept printing "cpal sends 0, this is a cpal bug" for two
    /// commits after that bug was fixed. **If `channel_mask_for` changes, change this too** — and
    /// note that a run of this example can never, by construction, verify what the library does.
    /// Use `list_devices` in the consuming application for that.
    const CPAL_CHANNEL_MASK: u32 =
        KernelStreaming::SPEAKER_FRONT_LEFT | KernelStreaming::SPEAKER_FRONT_RIGHT;

    const _: () = assert!(CPAL_CHANNEL_MASK == 0x3);

    /// One row of the exclusive-mode format matrix.
    struct Candidate {
        /// Short label used in the matrix and in the verdict.
        label: &'static str,
        /// `false` builds a plain 18-byte `WAVE_FORMAT_PCM` header (`cbSize` 0); `true` builds a
        /// full `WAVE_FORMAT_EXTENSIBLE` one.
        extensible: bool,
        /// `wBitsPerSample` — the *container* size in bits, and what `nBlockAlign` is derived
        /// from.
        container_bits: u16,
        /// `wValidBitsPerSample` — how many of those bits carry signal.
        valid_bits: u16,
        /// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` rather than `KSDATAFORMAT_SUBTYPE_PCM`.
        float: bool,
        /// Whether `cpal`'s `config_to_waveformatextensible` can emit this exact layout for some
        /// `SampleFormat`. This is what separates "the device refuses exclusive mode" from "the
        /// device accepts exclusive mode in a shape cpal cannot ask for".
        cpal_can_emit: bool,
        /// Printed once, above the mask tables.
        note: &'static str,
    }

    /// The candidate formats.
    ///
    /// `I16-PCM` and `I16-EXT` carry identical samples and differ only in how the header spells
    /// it; some drivers accept only one of the two spellings, and `cpal` emits the plain
    /// `WAVE_FORMAT_PCM` one for `SampleFormat::U8`/`I16` and `WAVE_FORMAT_EXTENSIBLE` for
    /// everything else. Only the extensible spelling carries a `dwChannelMask` at all, so the
    /// `I16-PCM` row is expected to read the same in every mask table — if it does not, the
    /// driver is answering non-deterministically and nothing else here should be trusted either.
    static CANDIDATES: [Candidate; 6] = [
        Candidate {
            label: "I16-PCM",
            extensible: false,
            container_bits: 16,
            valid_bits: 16,
            float: false,
            cpal_can_emit: true,
            note: "plain WAVE_FORMAT_PCM, cbSize 0, no channel mask - what cpal emits for I16",
        },
        Candidate {
            label: "I16-EXT",
            extensible: true,
            container_bits: 16,
            valid_bits: 16,
            float: false,
            cpal_can_emit: false,
            note: "the same 16-bit samples spelled as WAVE_FORMAT_EXTENSIBLE; cpal never emits it",
        },
        Candidate {
            label: "packed-24",
            extensible: true,
            container_bits: 24,
            valid_bits: 24,
            float: false,
            cpal_can_emit: false,
            note: "24 bits in a 3-byte container; cpal cannot express this at all",
        },
        Candidate {
            label: "24-in-32",
            extensible: true,
            container_bits: 32,
            valid_bits: 24,
            float: false,
            cpal_can_emit: true,
            note: "24 valid bits in a 4-byte container - what cpal emits for SampleFormat::I24",
        },
        Candidate {
            label: "I32",
            extensible: true,
            container_bits: 32,
            valid_bits: 32,
            float: false,
            cpal_can_emit: true,
            note: "what cpal emits for SampleFormat::I32",
        },
        Candidate {
            label: "F32",
            extensible: true,
            container_bits: 32,
            valid_bits: 32,
            float: true,
            cpal_can_emit: true,
            note: "what cpal emits for SampleFormat::F32",
        },
    ];

    /// Preference order used only to name a "best" accepted format in the verdict: widest valid
    /// bit depth first, integer before float at equal depth.
    fn precision_rank(candidate: &Candidate) -> u32 {
        u32::from(candidate.valid_bits) * 2 + u32::from(!candidate.float)
    }

    /// HRESULTs decoded by name. Everything else is passed through as hex, never swallowed.
    ///
    /// The values come from the `windows` crate rather than from literals on purpose: at least one
    /// widely-repeated number for `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED` (0x8889001A) is wrong —
    /// the SDK value is 0x8889000E — and a probe that mislabels the very code it exists to find
    /// would be worse than one that prints hex.
    const KNOWN_HRESULTS: [(HRESULT, &str, &str); 16] = [
        (S_OK, "S_OK", "OK"),
        (S_FALSE, "S_FALSE", "S_FALSE"),
        (E_INVALIDARG, "E_INVALIDARG", "E_INVALIDARG"),
        (E_POINTER, "E_POINTER", "E_POINTER"),
        (E_OUTOFMEMORY, "E_OUTOFMEMORY", "E_OUTOFMEM"),
        (
            Audio::AUDCLNT_E_UNSUPPORTED_FORMAT,
            "AUDCLNT_E_UNSUPPORTED_FORMAT",
            "UNSUPPORTED",
        ),
        (
            Audio::AUDCLNT_E_DEVICE_IN_USE,
            "AUDCLNT_E_DEVICE_IN_USE",
            "IN_USE",
        ),
        (
            Audio::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
            "AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED",
            "EXCL_DENIED",
        ),
        (
            Audio::AUDCLNT_E_ENDPOINT_CREATE_FAILED,
            "AUDCLNT_E_ENDPOINT_CREATE_FAILED",
            "EPT_CREATE",
        ),
        (
            Audio::AUDCLNT_E_DEVICE_INVALIDATED,
            "AUDCLNT_E_DEVICE_INVALIDATED",
            "INVALIDATED",
        ),
        (
            Audio::AUDCLNT_E_SERVICE_NOT_RUNNING,
            "AUDCLNT_E_SERVICE_NOT_RUNNING",
            "NO_SERVICE",
        ),
        (
            Audio::AUDCLNT_E_WRONG_ENDPOINT_TYPE,
            "AUDCLNT_E_WRONG_ENDPOINT_TYPE",
            "WRONG_EPT",
        ),
        (
            Audio::AUDCLNT_E_EXCLUSIVE_MODE_ONLY,
            "AUDCLNT_E_EXCLUSIVE_MODE_ONLY",
            "EXCL_ONLY",
        ),
        (
            Audio::AUDCLNT_E_ENGINE_FORMAT_LOCKED,
            "AUDCLNT_E_ENGINE_FORMAT_LOCKED",
            "FMT_LOCKED",
        ),
        (
            Audio::AUDCLNT_E_RAW_MODE_UNSUPPORTED,
            "AUDCLNT_E_RAW_MODE_UNSUPPORTED",
            "NO_RAW_MODE",
        ),
        (
            Audio::AUDCLNT_E_INVALID_SIZE,
            "AUDCLNT_E_INVALID_SIZE",
            "INVALID_SIZE",
        ),
    ];

    /// The short token printed in a matrix cell.
    fn cell_token(hr: HRESULT) -> String {
        match KNOWN_HRESULTS.iter().find(|(code, _, _)| *code == hr) {
            Some((_, _, token)) => (*token).to_string(),
            // Unrecognised: the raw code, which is still actionable.
            None => format!("{:#010X}", hr.0),
        }
    }

    /// The full `NAME (0xHHHHHHHH)` form used in the legend.
    fn hresult_full(hr: HRESULT) -> String {
        match KNOWN_HRESULTS.iter().find(|(code, _, _)| *code == hr) {
            Some((_, name, _)) => format!("{name} ({:#010X})", hr.0),
            None => {
                // Not a code this example knows. Ask the OS for a description rather than
                // dropping the information on the floor; many AUDCLNT codes have none, hence the
                // emptiness check.
                let message = hr.message();
                let message = message.trim();
                if message.is_empty() {
                    format!("<unrecognised> ({:#010X})", hr.0)
                } else {
                    format!("<unrecognised: {message}> ({:#010X})", hr.0)
                }
            }
        }
    }

    /// RAII guard for this thread's COM apartment.
    struct ComGuard {
        /// `CoUninitialize` is called only for a `CoInitializeEx` that actually succeeded.
        uninitialize: bool,
    }

    impl ComGuard {
        fn new() -> Result<Self, HRESULT> {
            // SAFETY: no COM object exists on this thread yet, and the `Drop` below pairs every
            // successful initialisation with exactly one `CoUninitialize`.
            let hr = unsafe { Com::CoInitializeEx(None, Com::COINIT_APARTMENTTHREADED) };
            if hr == S_OK || hr == S_FALSE {
                // S_FALSE means COM was already initialised on this thread. That is a success:
                // the reference count went up, so it still has to come back down.
                Ok(Self { uninitialize: true })
            } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
                // Someone initialised this thread as MTA. COM still works for our purposes, but
                // our call failed, so it must not be balanced with `CoUninitialize`.
                Ok(Self {
                    uninitialize: false,
                })
            } else {
                Err(hr)
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.uninitialize {
                // SAFETY: balances the successful `CoInitializeEx` in `ComGuard::new`, on the
                // same thread. Every COM interface obtained here is a `windows` smart pointer
                // that has already released itself by the time this guard, declared first in
                // `run`, is dropped last.
                unsafe { Com::CoUninitialize() };
            }
        }
    }

    /// RAII wrapper for a COM-allocated wide string.
    struct ComString(PWSTR);

    impl Drop for ComString {
        fn drop(&mut self) {
            // SAFETY: the pointer came from a COM call that allocated it with `CoTaskMemAlloc`,
            // and is freed exactly once, here.
            unsafe { Com::CoTaskMemFree(Some(self.0.as_ptr().cast())) };
        }
    }

    /// RAII wrapper for the `WAVEFORMATEX` returned by `GetMixFormat`.
    struct MixFormat(*mut Audio::WAVEFORMATEX);

    impl Drop for MixFormat {
        fn drop(&mut self) {
            // SAFETY: `GetMixFormat` allocates with `CoTaskMemAlloc` and documents the caller as
            // responsible for freeing it; this happens exactly once.
            unsafe { Com::CoTaskMemFree(Some(self.0.cast())) };
        }
    }

    /// A `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE` copied out into aligned, owned fields.
    ///
    /// Both Windows structures are `#[repr(packed(1))]`, so their fields cannot be borrowed —
    /// which formatting macros do implicitly. Copying once here keeps every later use trivially
    /// sound and keeps the printing code readable.
    #[derive(Clone, Copy)]
    struct FormatFields {
        format_tag: u16,
        channels: u16,
        sample_rate: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
        /// Present only for `WAVE_FORMAT_EXTENSIBLE`.
        extensible: Option<ExtensibleFields>,
    }

    #[derive(Clone, Copy)]
    struct ExtensibleFields {
        valid_bits: u16,
        channel_mask: u32,
        sub_format: GUID,
    }

    /// Reads a `WAVEFORMATEX` into owned fields, following it into `WAVEFORMATEXTENSIBLE` when
    /// the tag says to.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a valid `WAVEFORMATEX` whose `cbSize` honestly describes its trailing
    /// bytes; when `wFormatTag` is `WAVE_FORMAT_EXTENSIBLE` the allocation must be a full
    /// `WAVEFORMATEXTENSIBLE`.
    unsafe fn read_format(ptr: *const Audio::WAVEFORMATEX) -> FormatFields {
        // SAFETY: field-by-field reads copy out of a packed structure without ever forming a
        // reference to a field, which is what the caller's contract makes sound.
        let (format_tag, channels, sample_rate, avg_bytes_per_sec, block_align, bits, cb_size) = unsafe {
            (
                (*ptr).wFormatTag,
                (*ptr).nChannels,
                (*ptr).nSamplesPerSec,
                (*ptr).nAvgBytesPerSec,
                (*ptr).nBlockAlign,
                (*ptr).wBitsPerSample,
                (*ptr).cbSize,
            )
        };

        let extensible = if u32::from(format_tag) == KernelStreaming::WAVE_FORMAT_EXTENSIBLE
            && cb_size >= CB_SIZE_EXTENSIBLE
        {
            let ext = ptr.cast::<Audio::WAVEFORMATEXTENSIBLE>();
            // SAFETY: the tag and `cbSize` above say the allocation extends to the full
            // `WAVEFORMATEXTENSIBLE`; `Samples` is a union of three `u16`s, all valid to read.
            unsafe {
                Some(ExtensibleFields {
                    valid_bits: (*ext).Samples.wValidBitsPerSample,
                    channel_mask: (*ext).dwChannelMask,
                    sub_format: (*ext).SubFormat,
                })
            }
        } else {
            None
        };

        FormatFields {
            format_tag,
            channels,
            sample_rate,
            avg_bytes_per_sec,
            block_align,
            bits_per_sample: bits,
            cb_size,
            extensible,
        }
    }

    /// One frame in bytes: `nBlockAlign = nChannels * wBitsPerSample / 8`.
    ///
    /// Saturating because a device is free to report an absurd `nChannels` and this example is
    /// usually run as a debug build, where a plain overflow would abort the whole report. A
    /// saturated value is a format no driver accepts, which is the right outcome anyway.
    fn block_align(candidate: &Candidate, channels: u16) -> u16 {
        channels.saturating_mul(candidate.container_bits / 8)
    }

    /// Builds the `WAVEFORMATEXTENSIBLE` for one matrix cell.
    ///
    /// The derived fields are the ones a driver validates hardest, so each is computed from a
    /// single source:
    ///
    /// - `nBlockAlign = nChannels * wBitsPerSample / 8` (one frame, in bytes). For packed 24-bit
    ///   that is 3 bytes per channel; for 24-in-32 it is 4.
    /// - `nAvgBytesPerSec = nSamplesPerSec * nBlockAlign`.
    /// - `cbSize` is 22 for the extensible spelling and 0 for the plain `WAVE_FORMAT_PCM` one.
    ///
    /// A plain `WAVE_FORMAT_PCM` header has no `dwChannelMask` field, so for that candidate the
    /// mask is written into the structure but lies beyond the `cbSize` the driver reads.
    fn build_format(
        candidate: &Candidate,
        channels: u16,
        sample_rate: u32,
        channel_mask: u32,
    ) -> Audio::WAVEFORMATEXTENSIBLE {
        let block_align = block_align(candidate, channels);
        let avg_bytes_per_sec = sample_rate.saturating_mul(u32::from(block_align));
        let (format_tag, cb_size) = if candidate.extensible {
            (KernelStreaming::WAVE_FORMAT_EXTENSIBLE, CB_SIZE_EXTENSIBLE)
        } else {
            (Audio::WAVE_FORMAT_PCM, 0)
        };
        let sub_format = if candidate.float {
            Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM
        };

        Audio::WAVEFORMATEXTENSIBLE {
            Format: Audio::WAVEFORMATEX {
                wFormatTag: format_tag as u16,
                nChannels: channels,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: avg_bytes_per_sec,
                nBlockAlign: block_align,
                wBitsPerSample: candidate.container_bits,
                cbSize: cb_size,
            },
            Samples: Audio::WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: candidate.valid_bits,
            },
            dwChannelMask: channel_mask,
            SubFormat: sub_format,
        }
    }

    /// Asks the endpoint whether it accepts `format` in exclusive mode, returning the raw HRESULT.
    ///
    /// # Safety
    ///
    /// `client` must be a live `IAudioClient` and `format` must point at a valid `WAVEFORMATEX`
    /// that stays alive for the call.
    unsafe fn query_exclusive(
        client: &Audio::IAudioClient,
        format: *const Audio::WAVEFORMATEX,
    ) -> HRESULT {
        // SAFETY: the caller guarantees both pointers. The closest-match out-parameter is NULL
        // because exclusive mode reports no closest match and documents NULL as the value to pass
        // there; passing one would leak whatever a driver wrote into it.
        unsafe { client.IsFormatSupported(Audio::AUDCLNT_SHAREMODE_EXCLUSIVE, format, None) }
    }

    /// Human-readable form of a channel mask.
    fn describe_channel_mask(mask: u32) -> String {
        const SPEAKERS: [&str; 18] = [
            "FL", "FR", "FC", "LFE", "BL", "BR", "FLC", "FRC", "BC", "SL", "SR", "TC", "TFL",
            "TFC", "TFR", "TBL", "TBC", "TBR",
        ];
        if mask == 0 {
            return "KSAUDIO_SPEAKER_DIRECTOUT".to_string();
        }
        let mut names: Vec<&str> = Vec::new();
        for (bit, name) in SPEAKERS.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                names.push(name);
            }
        }
        let unknown = mask & !((1u32 << SPEAKERS.len()) - 1);
        if unknown != 0 {
            return format!("{} + unknown bits {unknown:#010X}", names.join(" "));
        }
        names.join(" ")
    }

    /// The standard positional mask for a channel count: what a driver that wants real speaker
    /// positions expects to see.
    fn positional_channel_mask(channels: u16) -> u32 {
        match channels {
            0 => 0,
            1 => 0x4, // SPEAKER_FRONT_CENTER
            2 => 0x3, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
            n if n >= 18 => 0x3_FFFF,
            n => (1u32 << n) - 1,
        }
    }

    fn describe_sub_format(guid: &GUID) -> String {
        let name = if *guid == KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM {
            "KSDATAFORMAT_SUBTYPE_PCM"
        } else if *guid == Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            "KSDATAFORMAT_SUBTYPE_IEEE_FLOAT"
        } else {
            "unrecognised subtype"
        };
        format!("{} {name}", format_guid(guid))
    }

    fn format_guid(guid: &GUID) -> String {
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            guid.data1,
            guid.data2,
            guid.data3,
            guid.data4[0],
            guid.data4[1],
            guid.data4[2],
            guid.data4[3],
            guid.data4[4],
            guid.data4[5],
            guid.data4[6],
            guid.data4[7],
        )
    }

    fn describe_format_tag(tag: u16) -> &'static str {
        match u32::from(tag) {
            Audio::WAVE_FORMAT_PCM => "WAVE_FORMAT_PCM",
            Multimedia::WAVE_FORMAT_IEEE_FLOAT => "WAVE_FORMAT_IEEE_FLOAT",
            KernelStreaming::WAVE_FORMAT_EXTENSIBLE => "WAVE_FORMAT_EXTENSIBLE",
            _ => "unrecognised format tag",
        }
    }

    fn describe_data_flow(flow: Audio::EDataFlow) -> &'static str {
        if flow == Audio::eRender {
            "eRender (output)"
        } else if flow == Audio::eCapture {
            "eCapture (input)"
        } else if flow == Audio::eAll {
            "eAll"
        } else {
            "unrecognised data flow"
        }
    }

    fn describe_device_state(state: Audio::DEVICE_STATE) -> String {
        let name = match state {
            Audio::DEVICE_STATE_ACTIVE => "ACTIVE",
            Audio::DEVICE_STATE_DISABLED => "DISABLED",
            Audio::DEVICE_STATE_NOTPRESENT => "NOTPRESENT",
            Audio::DEVICE_STATE_UNPLUGGED => "UNPLUGGED",
            _ => "unrecognised state",
        };
        format!("{name} ({:#010X})", state.0)
    }

    /// Frames in a period of `hns` 100-nanosecond units at `sample_rate`.
    ///
    /// Same rounding as the backend's `buffer_duration_to_frames`, so the numbers printed here are
    /// the ones `cpal` would compute; widened to `i128` only so a nonsense period from a driver
    /// cannot overflow the report out of existence in a debug build.
    fn period_frames(hns: i64, sample_rate: u32) -> i64 {
        let frames =
            (i128::from(hns) * i128::from(sample_rate) * 100 + 500_000_000) / 1_000_000_000;
        frames.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// Whether a period is a whole number of frames at that rate. A period that is not is where
    /// `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` comes from.
    fn period_is_whole_frames(hns: i64, sample_rate: u32) -> bool {
        (i128::from(hns) * i128::from(sample_rate)) % 10_000_000 == 0
    }

    /// Copies a null-terminated wide string out of COM memory.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, null-terminated wide string.
    unsafe fn wide_to_string(ptr: *const u16) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        // A device name or endpoint id is far shorter than this; the bound only stops a runaway
        // read on a malformed string.
        const MAX_LEN: usize = 32_768;
        let mut len = 0;
        // SAFETY: the caller guarantees a null-terminated string; the loop stops at the
        // terminator or at the bound, whichever comes first.
        while len < MAX_LEN && unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        if len >= MAX_LEN {
            return None;
        }
        // SAFETY: `len` wide characters were just walked, so the slice is in bounds.
        let wide = unsafe { slice::from_raw_parts(ptr, len) };
        Some(OsString::from_wide(wide).to_string_lossy().into_owned())
    }

    /// Reads the endpoint id, which is stable across reboots and is what `cpal` reports as a
    /// device id.
    fn endpoint_id(device: &Audio::IMMDevice) -> Option<String> {
        // SAFETY: `device` is a live COM object; the returned string is owned by the caller and
        // is freed by `ComString` on the way out.
        unsafe {
            let id = device.GetId().ok()?;
            let owned = ComString(id);
            wide_to_string(owned.0.as_ptr())
        }
    }

    /// Reads the endpoint's friendly name.
    ///
    /// `PKEY_Device_FriendlyName` and `DEVPKEY_Device_FriendlyName` are the same property —
    /// `{a45c254e-df1c-4efd-8020-67d146a850e0}`, PID 14 — and only the latter is exposed by the
    /// `windows` features this package enables, so it is the one used here, exactly as the WASAPI
    /// backend does.
    fn friendly_name(device: &Audio::IMMDevice) -> Option<String> {
        // SAFETY: `device` is a live COM object. The property store is a COM smart pointer that
        // releases itself, and the PROPVARIANT is cleared before it goes out of scope.
        unsafe {
            let store: IPropertyStore = device.OpenPropertyStore(Com::STGM_READ).ok()?;
            let key = &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY;
            let mut value = store.GetValue(key).ok()?;
            let variant = &value.Anonymous.Anonymous;
            let name = if variant.vt == VT_LPWSTR {
                let ptr = *(&variant.Anonymous as *const _ as *const *const u16);
                wide_to_string(ptr)
            } else {
                None
            };
            StructuredStorage::PropVariantClear(&mut value).ok();
            name
        }
    }

    /// One `dwChannelMask` axis value, and every cell probed with it.
    struct MaskProbe {
        mask: u32,
        /// Why this mask is in the report.
        role: &'static str,
        /// Indexed `[candidate][rate]`.
        cells: Vec<Vec<HRESULT>>,
    }

    /// The full matrix for one channel count. `masks[0]` is always the mask `cpal` sends.
    struct Matrix {
        channels: u16,
        masks: Vec<MaskProbe>,
    }

    impl Matrix {
        /// The results for the mask `cpal` sends today.
        fn cpal_mask(&self) -> &MaskProbe {
            &self.masks[0]
        }

        /// Every cell that some other mask accepts and `KSAUDIO_SPEAKER_DIRECTOUT` refuses.
        fn mask_flips(&self) -> Vec<MaskFlip> {
            let mut flips = Vec::new();
            for probe in self.masks.iter().skip(1) {
                for (candidate_index, row) in probe.cells.iter().enumerate() {
                    for (rate_index, &hr) in row.iter().enumerate() {
                        let baseline = self.cpal_mask().cells[candidate_index][rate_index];
                        if hr == S_OK && baseline != S_OK {
                            flips.push(MaskFlip {
                                candidate: &CANDIDATES[candidate_index],
                                rate: PROBE_RATES[rate_index],
                                channels: self.channels,
                                mask: probe.mask,
                                refused_with: baseline,
                            });
                        }
                    }
                }
            }
            flips
        }
    }

    /// A cell accepted with a positional mask and refused with `dwChannelMask = 0`.
    struct MaskFlip {
        candidate: &'static Candidate,
        rate: u32,
        channels: u16,
        mask: u32,
        refused_with: HRESULT,
    }

    /// Everything one endpoint contributes to the final summary.
    struct DeviceSummary {
        heading: String,
        verdict: String,
    }

    pub fn run() {
        // Declared first so it is dropped last, after every COM interface below has released.
        let _com = match ComGuard::new() {
            Ok(guard) => guard,
            Err(hr) => {
                println!("FATAL: CoInitializeEx failed: {}", hresult_full(hr));
                return;
            }
        };

        print_header();

        // SAFETY: COM is initialised on this thread by the guard above.
        let enumerator: Audio::IMMDeviceEnumerator = match unsafe {
            Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, Com::CLSCTX_ALL)
        } {
            Ok(enumerator) => enumerator,
            Err(error) => {
                println!(
                    "FATAL: could not create MMDeviceEnumerator: {}",
                    hresult_full(error.code())
                );
                return;
            }
        };

        let mut summaries: Vec<DeviceSummary> = Vec::new();
        let mut index = 0;

        // Render first, then capture, each sorted by endpoint id: a stable order that does not
        // depend on how Windows happened to enumerate this time.
        for flow in [Audio::eRender, Audio::eCapture] {
            let mut devices = collect_devices(&enumerator, flow);
            devices.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (id, device) in devices {
                index += 1;
                summaries.push(report_device(index, flow, &id, &device));
            }
        }

        print_summary(&summaries);
    }

    fn print_header() {
        println!("cpal - WASAPI exclusive-mode diagnostic probe");
        println!("{}", "=".repeat(RULE));
        println!(
            "Every call below is a query: IsFormatSupported, GetMixFormat, GetDevicePeriod and the\n\
             endpoint property store. IAudioClient::Initialize is never called, so this probe\n\
             cannot take a device away from another application - and cannot observe\n\
             AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED either, which only Initialize reports."
        );
        println!();
        println!(
            "Probed rates: {}",
            PROBE_RATES
                .iter()
                .map(|rate| rate.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "The matrix has three axes: sample rate x format x dwChannelMask. Every\n\
             WAVEFORMATEXTENSIBLE is built by hand, so it can carry shapes cpal cannot emit -\n\
             packed 24-bit, and any channel mask other than KSAUDIO_SPEAKER_DIRECTOUT (0).\n\
             nBlockAlign = nChannels * wBitsPerSample / 8 and nAvgBytesPerSec = nSamplesPerSec *\n\
             nBlockAlign in every cell; both are printed per row, because a wrong value there\n\
             makes every cell read unsupported and would look exactly like a real negative."
        );
        println!();
        println!("Candidate formats - the rows of every matrix below:");
        for candidate in &CANDIDATES {
            println!("  {:<10} {}", candidate.label, candidate.note);
        }
    }

    /// Enumerates endpoints in `flow` in every device state, paired with their endpoint ids.
    fn collect_devices(
        enumerator: &Audio::IMMDeviceEnumerator,
        flow: Audio::EDataFlow,
    ) -> Vec<(String, Audio::IMMDevice)> {
        // SAFETY: `enumerator` is a live COM object; the collection and its items are smart
        // pointers that release themselves.
        let collection = match unsafe {
            enumerator.EnumAudioEndpoints(flow, Audio::DEVICE_STATE(Audio::DEVICE_STATEMASK_ALL))
        } {
            Ok(collection) => collection,
            Err(error) => {
                println!(
                    "WARNING: EnumAudioEndpoints({}) failed: {}",
                    describe_data_flow(flow),
                    hresult_full(error.code())
                );
                return Vec::new();
            }
        };

        // SAFETY: as above.
        let count = match unsafe { collection.GetCount() } {
            Ok(count) => count,
            Err(error) => {
                println!(
                    "WARNING: IMMDeviceCollection::GetCount failed: {}",
                    hresult_full(error.code())
                );
                return Vec::new();
            }
        };

        let mut devices = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: `i` is below the count just read from the same collection.
            match unsafe { collection.Item(i) } {
                Ok(device) => {
                    let id = endpoint_id(&device).unwrap_or_else(|| format!("<unknown id {i}>"));
                    devices.push((id, device));
                }
                Err(error) => println!(
                    "WARNING: IMMDeviceCollection::Item({i}) failed: {}",
                    hresult_full(error.code())
                ),
            }
        }
        devices
    }

    fn report_device(
        index: usize,
        enumerated_flow: Audio::EDataFlow,
        id: &str,
        device: &Audio::IMMDevice,
    ) -> DeviceSummary {
        let name = friendly_name(device).unwrap_or_else(|| "<no friendly name>".to_string());
        let heading = format!(
            "[{}] {name}",
            if enumerated_flow == Audio::eRender {
                "output"
            } else {
                "input "
            }
        );

        println!();
        println!("{}", "=".repeat(RULE));
        println!("Device {index}: {name}");
        println!("{}", "-".repeat(RULE));

        // The device's own answer, rather than the collection it came out of.
        // SAFETY: `device` is a live COM object; `IMMEndpoint` is one of its interfaces.
        let flow = unsafe {
            device
                .cast::<Audio::IMMEndpoint>()
                .and_then(|endpoint| endpoint.GetDataFlow())
        };
        match flow {
            Ok(flow) => println!("  data flow    : {}", describe_data_flow(flow)),
            Err(error) => println!(
                "  data flow    : IMMEndpoint::GetDataFlow failed: {} (enumerated as {})",
                hresult_full(error.code()),
                describe_data_flow(enumerated_flow)
            ),
        }

        // SAFETY: `device` is a live COM object.
        match unsafe { device.GetState() } {
            Ok(state) => println!("  state        : {}", describe_device_state(state)),
            Err(error) => println!(
                "  state        : IMMDevice::GetState failed: {}",
                hresult_full(error.code())
            ),
        }
        println!("  endpoint id  : {id}");

        // SAFETY: `device` is a live COM object; the `IAudioClient` is released when it drops.
        let client: Audio::IAudioClient = match unsafe { device.Activate(Com::CLSCTX_ALL, None) } {
            Ok(client) => client,
            Err(error) => {
                let verdict = format!(
                    "IAudioClient could not be activated ({}) - no exclusive-mode conclusion for \
                     this endpoint.",
                    hresult_full(error.code())
                );
                println!();
                print_wrapped("  VERDICT: ", "           ", &verdict);
                return DeviceSummary { heading, verdict };
            }
        };

        let mix = report_mix_format(&client);
        report_device_period(&client);

        // The device's own channel count, plus stereo when that is something else.
        let mut channel_counts: Vec<u16> = Vec::new();
        if let Some(mix) = mix.as_ref() {
            if mix.channels > 0 {
                channel_counts.push(mix.channels);
            }
        }
        if !channel_counts.contains(&2) {
            channel_counts.push(2);
        }

        let matrices: Vec<Matrix> = channel_counts
            .iter()
            .map(|&channels| probe_matrix(&client, channels, mix.as_ref()))
            .collect();

        for matrix in &matrices {
            print_matrix(matrix);
        }

        let verdict = verdict_for(&matrices);
        println!();
        print_wrapped("  VERDICT: ", "           ", &verdict);

        DeviceSummary { heading, verdict }
    }

    /// Prints `GetMixFormat` in full and returns it for later use.
    fn report_mix_format(client: &Audio::IAudioClient) -> Option<FormatFields> {
        println!();
        println!("  GetMixFormat (describes the shared-mode engine, NOT the endpoint):");

        // SAFETY: `client` is a live `IAudioClient`; the returned block is owned by us and freed
        // by `MixFormat`.
        let ptr = match unsafe { client.GetMixFormat() } {
            Ok(ptr) => ptr,
            Err(error) => {
                println!("    failed: {}", hresult_full(error.code()));
                return None;
            }
        };
        let owned = MixFormat(ptr);
        if owned.0.is_null() {
            // Documented as impossible alongside a success code, but a null dereference here
            // would take the whole report down with it.
            println!("    succeeded but returned a null format");
            return None;
        }
        // SAFETY: non-null and checked above; `GetMixFormat` returns a valid `WAVEFORMATEX`,
        // extended to a `WAVEFORMATEXTENSIBLE` exactly when its own tag and `cbSize` say so.
        let fields = unsafe { read_format(owned.0) };

        println!(
            "    wFormatTag          : {:#06X} {}",
            fields.format_tag,
            describe_format_tag(fields.format_tag)
        );
        println!("    nChannels           : {}", fields.channels);
        println!("    nSamplesPerSec      : {}", fields.sample_rate);
        println!("    nAvgBytesPerSec     : {}", fields.avg_bytes_per_sec);
        println!("    nBlockAlign         : {}", fields.block_align);
        println!("    wBitsPerSample      : {}", fields.bits_per_sample);
        println!("    cbSize              : {}", fields.cb_size);
        match fields.extensible.as_ref() {
            Some(ext) => {
                println!("    wValidBitsPerSample : {}", ext.valid_bits);
                println!(
                    "    dwChannelMask       : {:#010X} ({})",
                    ext.channel_mask,
                    describe_channel_mask(ext.channel_mask)
                );
                println!(
                    "    SubFormat           : {}",
                    describe_sub_format(&ext.sub_format)
                );
            }
            None => println!(
                "    (not WAVE_FORMAT_EXTENSIBLE: no valid-bits, channel mask or subformat)"
            ),
        }

        Some(fields)
    }

    fn report_device_period(client: &Audio::IAudioClient) {
        println!();
        println!("  GetDevicePeriod:");

        let mut default_hns = 0i64;
        let mut minimum_hns = 0i64;
        // SAFETY: `client` is a live `IAudioClient` and both out-parameters are valid `i64`s.
        if let Err(error) =
            unsafe { client.GetDevicePeriod(Some(&mut default_hns), Some(&mut minimum_hns)) }
        {
            println!("    failed: {}", hresult_full(error.code()));
            return;
        }

        let mut any_fractional = false;
        for (label, hns) in [("default", default_hns), ("minimum", minimum_hns)] {
            let frames: Vec<String> = PROBE_RATES
                .iter()
                .map(|&rate| {
                    let whole = period_is_whole_frames(hns, rate);
                    any_fractional |= !whole;
                    format!(
                        "{}{} @{rate}",
                        period_frames(hns, rate),
                        if whole { "" } else { "*" }
                    )
                })
                .collect();
            println!(
                "    {label} : {hns} hns ({:.3} ms) = {} frames",
                hns as f64 / 10_000.0,
                frames.join(", ")
            );
        }
        if any_fractional {
            println!(
                "    * = not a whole number of frames at that rate. An exclusive-mode Initialize\n\
                 \x20       passes the period as the buffer duration, and a buffer the\n\
                 \x20       endpoint cannot align is what returns\n\
                 \x20       AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED."
            );
        }
    }

    /// The channel-mask axis for one channel count: always the mask `cpal` sends first, then the
    /// standard positional mask, then the device's own mix-format mask if it is a third value.
    fn mask_axis(mix: Option<&FormatFields>, channels: u16) -> Vec<(u32, &'static str)> {
        let mut masks = vec![(CPAL_CHANNEL_MASK, "what cpal emits today")];

        let positional = positional_channel_mask(channels);
        if positional != CPAL_CHANNEL_MASK {
            masks.push((
                positional,
                "standard positional mask for this channel count",
            ));
        }

        if let Some(ext) = mix.and_then(|m| m.extensible.as_ref()) {
            if mix.map(|m| m.channels) == Some(channels)
                && !masks.iter().any(|(mask, _)| *mask == ext.channel_mask)
            {
                masks.push((ext.channel_mask, "this device's own mix-format mask"));
            }
        }

        masks
    }

    /// Probes every rate x format x mask cell for one channel count.
    fn probe_matrix(
        client: &Audio::IAudioClient,
        channels: u16,
        mix: Option<&FormatFields>,
    ) -> Matrix {
        let masks = mask_axis(mix, channels)
            .into_iter()
            .map(|(mask, role)| {
                let cells = CANDIDATES
                    .iter()
                    .map(|candidate| {
                        PROBE_RATES
                            .iter()
                            .map(|&rate| {
                                let format = build_format(candidate, channels, rate, mask);
                                // SAFETY: `client` is live and `format` outlives the call. A
                                // reference to `Format` is sound despite the packed layout
                                // because `WAVEFORMATEX` is itself `packed(1)` and so needs no
                                // alignment; this is the same idiom the WASAPI backend uses.
                                unsafe {
                                    query_exclusive(
                                        client,
                                        &format.Format as *const Audio::WAVEFORMATEX,
                                    )
                                }
                            })
                            .collect()
                    })
                    .collect();
                MaskProbe { mask, role, cells }
            })
            .collect();

        Matrix { channels, masks }
    }

    fn print_matrix(matrix: &Matrix) {
        println!();
        println!(
            "  Exclusive-mode IsFormatSupported matrix - {} channel(s)",
            matrix.channels
        );

        println!("    rows as built (identical in every mask table below):");
        println!(
            "      {:<10} {:<22} {:>4} {:>5} {:>3} {:>6} SubFormat",
            "format", "wFormatTag", "bits", "valid", "blk", "cbSize"
        );
        for candidate in &CANDIDATES {
            println!(
                "      {:<10} {:<22} {:>4} {:>5} {:>3} {:>6} {}",
                candidate.label,
                if candidate.extensible {
                    "WAVE_FORMAT_EXTENSIBLE"
                } else {
                    "WAVE_FORMAT_PCM"
                },
                candidate.container_bits,
                candidate.valid_bits,
                block_align(candidate, matrix.channels),
                if candidate.extensible {
                    CB_SIZE_EXTENSIBLE
                } else {
                    0
                },
                if candidate.float { "IEEE_FLOAT" } else { "PCM" },
            );
        }
        println!("      blk is nBlockAlign; nAvgBytesPerSec = nSamplesPerSec * blk in every cell.");

        for (index, probe) in matrix.masks.iter().enumerate() {
            println!();
            println!(
                "    [{}] dwChannelMask = {:#010X} ({}) - {}",
                mask_tag(index),
                probe.mask,
                describe_channel_mask(probe.mask),
                probe.role
            );
            let mut header = format!("        {:<10}", "format");
            for rate in PROBE_RATES {
                header.push_str(&format!(" {:<13}", rate.to_string()));
            }
            println!("{}", header.trim_end());
            for (candidate, row) in CANDIDATES.iter().zip(&probe.cells) {
                let mut line = format!("        {:<10}", candidate.label);
                for &hr in row {
                    line.push_str(&format!(" {:<13}", cell_token(hr)));
                }
                println!("{}", line.trim_end());
            }
        }

        print_mask_comparison(matrix);
        print_matrix_legend(matrix);
    }

    /// `[A]`, `[B]`, `[C]`, ... for the mask tables.
    fn mask_tag(index: usize) -> char {
        char::from(b'A'.saturating_add(index as u8))
    }

    /// The comparison the channel-mask hypothesis lives or dies on: every cell whose answer
    /// depends only on `dwChannelMask`.
    fn print_mask_comparison(matrix: &Matrix) {
        println!();
        if matrix.masks.len() < 2 {
            println!("    mask comparison: only one channel mask applies at this channel count.");
            return;
        }

        println!("    mask comparison (cells whose answer depends only on dwChannelMask):");
        let baseline = matrix.cpal_mask();
        let mut differences = 0usize;
        let mut flips = 0usize;

        for (index, probe) in matrix.masks.iter().enumerate().skip(1) {
            for (candidate_index, row) in probe.cells.iter().enumerate() {
                for (rate_index, &hr) in row.iter().enumerate() {
                    let base = baseline.cells[candidate_index][rate_index];
                    if hr == base {
                        continue;
                    }
                    differences += 1;
                    if hr == S_OK {
                        flips += 1;
                    }
                    println!(
                        "      {:<10} @{:<6} [A] {:<13} -> [{}] {}",
                        CANDIDATES[candidate_index].label,
                        PROBE_RATES[rate_index],
                        cell_token(base),
                        mask_tag(index),
                        cell_token(hr),
                    );
                }
            }
        }

        if differences == 0 {
            println!("      no cell changed its answer with any other channel mask.");
            return;
        }
        if flips > 0 {
            println!(
                "      => {flips} cell(s) ACCEPTED WITH A POSITIONAL MASK AND REFUSED WITH 0.\n\
                 \x20        cpal sends dwChannelMask = KSAUDIO_SPEAKER_DIRECTOUT (0) for every\n\
                 \x20        exclusive-mode format, so those formats are unreachable from cpal on\n\
                 \x20        this endpoint. That is a cpal bug, not a device limitation."
            );
        }
    }

    /// Expands every short token that actually appeared, so a pasted block is self-contained.
    fn print_matrix_legend(matrix: &Matrix) {
        let mut seen: Vec<HRESULT> = Vec::new();
        for probe in &matrix.masks {
            for row in &probe.cells {
                for &hr in row {
                    if !seen.contains(&hr) {
                        seen.push(hr);
                    }
                }
            }
        }
        seen.sort_by_key(|hr| hr.0);

        println!();
        println!("    results seen:");
        for hr in seen {
            println!("      {:<13} {}", cell_token(hr), hresult_full(hr));
        }
    }

    /// Turns the matrices into one verdict naming the most specific supported explanation.
    fn verdict_for(matrices: &[Matrix]) -> String {
        // The channel-mask finding comes first whenever there is one: it is a bug in cpal's
        // exclusive-mode path rather than anything about the device, and it must not end up
        // buried behind some other format that happens to work.
        let mut flips: Vec<MaskFlip> = matrices.iter().flat_map(Matrix::mask_flips).collect();
        flips.sort_by_key(|flip| std::cmp::Reverse(precision_rank(flip.candidate)));

        let mask_sentence = flips.first().map(|flip| {
            format!(
                "{} @ {} Hz, {} ch is ACCEPTED WITH A POSITIONAL MASK ({:#010X}) AND REFUSED WITH \
                 0 (which answered {}); {} cell(s) behave that way. This endpoint requires a \
                 positional dwChannelMask in exclusive mode -- a device fact, measured here. \
                 cpal sends one as of the commit that added channel_mask_for; a cpal older than \
                 that sent KSAUDIO_SPEAKER_DIRECTOUT (0) and could not reach these formats at all. \
                 This example builds its own formats and never calls the library, so it cannot \
                 tell you which of the two you are running -- check that from the consuming \
                 application.",
                flip.candidate.label,
                flip.rate,
                flip.channels,
                flip.mask,
                cell_token(flip.refused_with),
                flips.len(),
            )
        });

        let format_sentence = format_verdict(matrices);

        match mask_sentence {
            Some(mask_sentence) => {
                format!("{mask_sentence} With the mask cpal sends today: {format_sentence}")
            }
            None => format_sentence,
        }
    }

    /// The verdict for the mask `cpal` actually sends, ignoring the other mask tables.
    fn format_verdict(matrices: &[Matrix]) -> String {
        // 1. Something cpal can ask for today was accepted.
        let mut best: Option<(u32, String)> = None;
        for matrix in matrices {
            for (candidate, row) in CANDIDATES.iter().zip(&matrix.cpal_mask().cells) {
                if !candidate.cpal_can_emit {
                    continue;
                }
                for (rate, &hr) in PROBE_RATES.iter().zip(row) {
                    if hr == S_OK {
                        let rank = precision_rank(candidate);
                        if best.as_ref().is_none_or(|(seen, _)| rank > *seen) {
                            best = Some((
                                rank,
                                format!("{} @ {rate} Hz, {} ch", candidate.label, matrix.channels),
                            ));
                        }
                    }
                }
            }
        }
        if let Some((_, description)) = best {
            return format!(
                "exclusive mode supported and reachable through cpal - best cpal-expressible \
                 format = {description}."
            );
        }

        // 2. Exclusive mode works, but only in a layout cpal cannot emit.
        for matrix in matrices {
            for (candidate, row) in CANDIDATES.iter().zip(&matrix.cpal_mask().cells) {
                if candidate.cpal_can_emit {
                    continue;
                }
                for (rate, &hr) in PROBE_RATES.iter().zip(row) {
                    if hr == S_OK {
                        let why = if candidate.label == "packed-24" {
                            "cpal's SampleFormat::I24 is 24 valid bits in a 4-byte container and \
                             config_to_waveformatextensible can emit nothing else, so cpal never \
                             offers the 3-byte packed frame this endpoint wants"
                        } else {
                            "cpal emits 16-bit samples as a plain WAVE_FORMAT_PCM header, not as \
                             WAVE_FORMAT_EXTENSIBLE"
                        };
                        return format!(
                            "exclusive mode supported, but only as {} @ {rate} Hz, {} ch, which \
                             cpal cannot express: {why}.",
                            candidate.label, matrix.channels
                        );
                    }
                }
            }
        }

        // 3. Nothing was accepted. Say what the refusals were, since the code is the diagnosis.
        let mut codes: Vec<HRESULT> = Vec::new();
        for matrix in matrices {
            for row in &matrix.cpal_mask().cells {
                for &hr in row {
                    if !codes.contains(&hr) {
                        codes.push(hr);
                    }
                }
            }
        }
        codes.sort_by_key(|hr| hr.0);

        if codes.len() == 1 {
            let hr = codes[0];
            let explanation = if hr == Audio::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED {
                "exclusive mode is switched off for this endpoint - set 'Allow applications to \
                 take exclusive control of this device' in Sound settings and re-run"
            } else if hr == Audio::AUDCLNT_E_DEVICE_IN_USE {
                "another process already holds this endpoint in exclusive mode - close it (a \
                 vendor control panel such as PreSonus Universal Control is the usual one) and \
                 re-run"
            } else if hr == Audio::AUDCLNT_E_UNSUPPORTED_FORMAT {
                "every probed format was refused as unsupported, packed 24-bit and every probed \
                 channel mask included, so neither cpal's 4-byte I24 container nor its zero \
                 channel mask explains it; consistent with a driver that implements no \
                 exclusive-mode format at all, though only the probed rates, channel counts, \
                 masks and sample formats were asked about"
            } else {
                "the endpoint refused every probed format with this single code"
            };
            return format!(
                "no format accepted in exclusive mode at any probed rate; every answer was {} - \
                 {explanation}.",
                hresult_full(hr)
            );
        }

        format!(
            "no format accepted in exclusive mode at any probed rate; the refusals were {}.",
            codes
                .iter()
                .map(|hr| hresult_full(*hr))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    fn print_summary(summaries: &[DeviceSummary]) {
        println!();
        println!("{}", "=".repeat(RULE));
        println!("Summary - one line per endpoint");
        println!("{}", "-".repeat(RULE));
        if summaries.is_empty() {
            println!("  no endpoints were enumerated.");
            return;
        }
        for (index, summary) in summaries.iter().enumerate() {
            println!("  {}. {}", index + 1, summary.heading);
            print_wrapped("     ", "     ", &summary.verdict);
        }
    }
}
