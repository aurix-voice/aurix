//! Safe bindings to libopus 1.6 (bundled, built from source) for the Aurix server and native
//! client. Besides the classic encoder/decoder controls the crate exposes what the voice path
//! needs for loss resilience:
//!
//! * in-band FEC (LBRR): [`Encoder::set_inband_fec`] + [`Encoder::set_packet_loss_perc`] on the
//!   sender, [`Decoder::decode`] with `fec = true` on the receiver, [`packet::has_lbrr`] to
//!   tell whether a packet carries redundancy for the previous frame;
//! * Deep REDundancy (DRED): [`Encoder::set_dred_duration`] embeds up to ~1 s of compressed
//!   redundancy in the packet padding (an Opus extension — older decoders and relays ignore
//!   it); a receiver parses it with [`DredDecoder::parse`] and reconstructs lost frames with
//!   [`Decoder::dred_decode_float`];
//! * deep PLC and OSCE speech enhancement, both selected by the decoder complexity
//!   ([`Decoder::set_complexity`]: `>= 5` neural PLC, `>= 6` LACE, `>= 7` NoLACE).
//!
//! All handles are `Send` (libopus states carry no thread affinity) but not `Sync`.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::ffi::CStr;
use std::fmt;
use std::os::raw::c_int;
use std::ptr::NonNull;

use opusic_sys as ffi;

/// Sample rates libopus accepts.
pub const SAMPLE_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];
/// DRED duration cap in 10 ms frames (`DRED_MAX_FRAMES`).
pub const MAX_DRED_FRAMES: u32 = 104;
/// Decoder complexity from which libopus runs the neural PLC instead of the classic one.
pub const DEEP_PLC_COMPLEXITY: i32 = 5;
/// Decoder complexity from which OSCE (LACE) enhances SILK speech; `7` selects NoLACE.
pub const OSCE_COMPLEXITY: i32 = 6;

/// The libopus version string (e.g. `libopus 1.6.1`).
pub fn version() -> &'static str {
    // SAFETY: libopus returns a pointer to a static NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::opus_get_version_string()) }
        .to_str()
        .unwrap_or("libopus")
}

/// libopus error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    BadArg,
    BufferTooSmall,
    InternalError,
    InvalidPacket,
    Unimplemented,
    InvalidState,
    AllocFail,
    Unknown,
}

impl ErrorCode {
    fn from_int(code: c_int) -> Self {
        match code {
            ffi::OPUS_BAD_ARG => Self::BadArg,
            ffi::OPUS_BUFFER_TOO_SMALL => Self::BufferTooSmall,
            ffi::OPUS_INTERNAL_ERROR => Self::InternalError,
            ffi::OPUS_INVALID_PACKET => Self::InvalidPacket,
            ffi::OPUS_UNIMPLEMENTED => Self::Unimplemented,
            ffi::OPUS_INVALID_STATE => Self::InvalidState,
            ffi::OPUS_ALLOC_FAIL => Self::AllocFail,
            _ => Self::Unknown,
        }
    }

    /// The libopus description of the code.
    pub fn description(self) -> &'static str {
        match self {
            Self::BadArg => "invalid argument",
            Self::BufferTooSmall => "buffer too small",
            Self::InternalError => "internal error",
            Self::InvalidPacket => "corrupted stream",
            Self::Unimplemented => "request not implemented",
            Self::InvalidState => "invalid state",
            Self::AllocFail => "memory allocation failed",
            Self::Unknown => "unknown error",
        }
    }
}

/// A failed libopus call: which function failed and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    function: &'static str,
    code: ErrorCode,
}

impl Error {
    fn new(function: &'static str, code: c_int) -> Self {
        Self {
            function,
            code: ErrorCode::from_int(code),
        }
    }

    pub fn function(&self) -> &'static str {
        self.function
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn description(&self) -> &'static str {
        self.code.description()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.function, self.description())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(function: &'static str, ret: c_int) -> Result<c_int> {
    if ret < 0 {
        Err(Error::new(function, ret))
    } else {
        Ok(ret)
    }
}

/// Encoder application profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    /// Speech: SILK/hybrid tuned for intelligibility (the only profile that emits DRED).
    Voip,
    /// Music and mixed content.
    Audio,
    /// Lowest algorithmic delay, CELT only.
    LowDelay,
}

impl Application {
    fn to_int(self) -> c_int {
        match self {
            Self::Voip => ffi::OPUS_APPLICATION_VOIP,
            Self::Audio => ffi::OPUS_APPLICATION_AUDIO,
            Self::LowDelay => ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Mono = 1,
    Stereo = 2,
}

impl Channels {
    pub fn count(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    Auto,
    Narrowband,
    Mediumband,
    Wideband,
    Superwideband,
    Fullband,
}

impl Bandwidth {
    fn to_int(self) -> c_int {
        match self {
            Self::Auto => ffi::OPUS_AUTO,
            Self::Narrowband => ffi::OPUS_BANDWIDTH_NARROWBAND,
            Self::Mediumband => ffi::OPUS_BANDWIDTH_MEDIUMBAND,
            Self::Wideband => ffi::OPUS_BANDWIDTH_WIDEBAND,
            Self::Superwideband => ffi::OPUS_BANDWIDTH_SUPERWIDEBAND,
            Self::Fullband => ffi::OPUS_BANDWIDTH_FULLBAND,
        }
    }

    fn from_int(function: &'static str, value: c_int) -> Result<Self> {
        Ok(match value {
            ffi::OPUS_AUTO => Self::Auto,
            ffi::OPUS_BANDWIDTH_NARROWBAND => Self::Narrowband,
            ffi::OPUS_BANDWIDTH_MEDIUMBAND => Self::Mediumband,
            ffi::OPUS_BANDWIDTH_WIDEBAND => Self::Wideband,
            ffi::OPUS_BANDWIDTH_SUPERWIDEBAND => Self::Superwideband,
            ffi::OPUS_BANDWIDTH_FULLBAND => Self::Fullband,
            _ => return Err(Error::new(function, ffi::OPUS_BAD_ARG)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Auto,
    Voice,
    Music,
}

impl Signal {
    fn to_int(self) -> c_int {
        match self {
            Self::Auto => ffi::OPUS_AUTO,
            Self::Voice => ffi::OPUS_SIGNAL_VOICE,
            Self::Music => ffi::OPUS_SIGNAL_MUSIC,
        }
    }

    fn from_int(function: &'static str, value: c_int) -> Result<Self> {
        Ok(match value {
            ffi::OPUS_AUTO => Self::Auto,
            ffi::OPUS_SIGNAL_VOICE => Self::Voice,
            ffi::OPUS_SIGNAL_MUSIC => Self::Music,
            _ => return Err(Error::new(function, ffi::OPUS_BAD_ARG)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitrate {
    /// Explicit target in bits per second.
    Bits(i32),
    /// As many bits as the packet size allows.
    Max,
    /// libopus picks from sample rate and channels.
    Auto,
}

impl Bitrate {
    fn to_int(self) -> c_int {
        match self {
            Self::Bits(b) => b,
            Self::Max => ffi::OPUS_BITRATE_MAX,
            Self::Auto => ffi::OPUS_AUTO,
        }
    }

    fn from_int(value: c_int) -> Self {
        match value {
            ffi::OPUS_BITRATE_MAX => Self::Max,
            ffi::OPUS_AUTO => Self::Auto,
            b => Self::Bits(b),
        }
    }
}

fn frame_size(function: &'static str, samples: usize, channels: Channels) -> Result<c_int> {
    if samples == 0 || !samples.is_multiple_of(channels.count()) {
        return Err(Error::new(function, ffi::OPUS_BAD_ARG));
    }
    c_int::try_from(samples / channels.count()).map_err(|_| Error::new(function, ffi::OPUS_BAD_ARG))
}

fn byte_len(function: &'static str, len: usize) -> Result<i32> {
    i32::try_from(len).map_err(|_| Error::new(function, ffi::OPUS_BAD_ARG))
}

/// An Opus encoder.
pub struct Encoder {
    ptr: NonNull<ffi::OpusEncoder>,
    channels: Channels,
}

// SAFETY: libopus states are plain heap memory without thread affinity; `&mut self` is
// required for every call that touches them.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(sample_rate: u32, channels: Channels, mode: Application) -> Result<Self> {
        let mut error = 0;
        // SAFETY: plain FFI call; the error slot is a valid out-pointer.
        let ptr = unsafe {
            ffi::opus_encoder_create(
                sample_rate as i32,
                channels as c_int,
                mode.to_int(),
                &mut error,
            )
        };
        match NonNull::new(ptr) {
            Some(ptr) if error == ffi::OPUS_OK => Ok(Self { ptr, channels }),
            _ => Err(Error::new("opus_encoder_create", error)),
        }
    }

    pub fn channels(&self) -> Channels {
        self.channels
    }

    /// Encode one frame of interleaved 16-bit PCM (`input.len() / channels` samples per
    /// channel — 2.5, 5, 10, 20, 40 or 60 ms). Returns the packet length.
    pub fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize> {
        let frame = frame_size("opus_encode", input.len(), self.channels)?;
        let max = byte_len("opus_encode", output.len())?;
        // SAFETY: the pointers and lengths describe live slices; the frame size matches
        // `input` and the encoder was created with `self.channels`.
        let ret = unsafe {
            ffi::opus_encode(
                self.ptr.as_ptr(),
                input.as_ptr(),
                frame,
                output.as_mut_ptr(),
                max,
            )
        };
        check("opus_encode", ret).map(|n| n as usize)
    }

    /// [`Self::encode`] for float PCM in `-1.0..=1.0`.
    pub fn encode_float(&mut self, input: &[f32], output: &mut [u8]) -> Result<usize> {
        let frame = frame_size("opus_encode_float", input.len(), self.channels)?;
        let max = byte_len("opus_encode_float", output.len())?;
        // SAFETY: as in `encode`.
        let ret = unsafe {
            ffi::opus_encode_float(
                self.ptr.as_ptr(),
                input.as_ptr(),
                frame,
                output.as_mut_ptr(),
                max,
            )
        };
        check("opus_encode_float", ret).map(|n| n as usize)
    }

    /// [`Self::encode`] into a freshly allocated packet of at most `max_size` bytes.
    pub fn encode_vec(&mut self, input: &[i16], max_size: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; max_size];
        let n = self.encode(input, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    fn ctl_set(&mut self, function: &'static str, request: c_int, value: c_int) -> Result<()> {
        // SAFETY: every `OPUS_SET_*` request used here takes exactly one `opus_int32`.
        let ret = unsafe { ffi::opus_encoder_ctl(self.ptr.as_ptr(), request, value) };
        check(function, ret).map(|_| ())
    }

    fn ctl_get(&mut self, function: &'static str, request: c_int) -> Result<c_int> {
        let mut value: c_int = 0;
        // SAFETY: every `OPUS_GET_*` request used here takes exactly one `opus_int32*`.
        let ret = unsafe { ffi::opus_encoder_ctl(self.ptr.as_ptr(), request, &mut value) };
        check(function, ret).map(|_| value)
    }

    pub fn reset_state(&mut self) -> Result<()> {
        // SAFETY: `OPUS_RESET_STATE` takes no argument.
        let ret = unsafe { ffi::opus_encoder_ctl(self.ptr.as_ptr(), ffi::OPUS_RESET_STATE) };
        check("opus_encoder_ctl(OPUS_RESET_STATE)", ret).map(|_| ())
    }

    pub fn set_bitrate(&mut self, value: Bitrate) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_BITRATE)",
            ffi::OPUS_SET_BITRATE_REQUEST,
            value.to_int(),
        )
    }

    pub fn get_bitrate(&mut self) -> Result<Bitrate> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_BITRATE)",
            ffi::OPUS_GET_BITRATE_REQUEST,
        )
        .map(Bitrate::from_int)
    }

    pub fn set_vbr(&mut self, on: bool) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_VBR)",
            ffi::OPUS_SET_VBR_REQUEST,
            c_int::from(on),
        )
    }

    pub fn get_vbr(&mut self) -> Result<bool> {
        self.ctl_get("opus_encoder_ctl(OPUS_GET_VBR)", ffi::OPUS_GET_VBR_REQUEST)
            .map(|v| v != 0)
    }

    pub fn set_vbr_constraint(&mut self, on: bool) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_VBR_CONSTRAINT)",
            ffi::OPUS_SET_VBR_CONSTRAINT_REQUEST,
            c_int::from(on),
        )
    }

    pub fn get_vbr_constraint(&mut self) -> Result<bool> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_VBR_CONSTRAINT)",
            ffi::OPUS_GET_VBR_CONSTRAINT_REQUEST,
        )
        .map(|v| v != 0)
    }

    /// Encoder complexity `0..=10`.
    pub fn set_complexity(&mut self, value: i32) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_COMPLEXITY)",
            ffi::OPUS_SET_COMPLEXITY_REQUEST,
            value,
        )
    }

    pub fn get_complexity(&mut self) -> Result<i32> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_COMPLEXITY)",
            ffi::OPUS_GET_COMPLEXITY_REQUEST,
        )
    }

    pub fn set_max_bandwidth(&mut self, value: Bandwidth) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_MAX_BANDWIDTH)",
            ffi::OPUS_SET_MAX_BANDWIDTH_REQUEST,
            value.to_int(),
        )
    }

    pub fn get_max_bandwidth(&mut self) -> Result<Bandwidth> {
        const F: &str = "opus_encoder_ctl(OPUS_GET_MAX_BANDWIDTH)";
        let v = self.ctl_get(F, ffi::OPUS_GET_MAX_BANDWIDTH_REQUEST)?;
        Bandwidth::from_int(F, v)
    }

    pub fn set_signal(&mut self, value: Signal) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_SIGNAL)",
            ffi::OPUS_SET_SIGNAL_REQUEST,
            value.to_int(),
        )
    }

    pub fn get_signal(&mut self) -> Result<Signal> {
        const F: &str = "opus_encoder_ctl(OPUS_GET_SIGNAL)";
        let v = self.ctl_get(F, ffi::OPUS_GET_SIGNAL_REQUEST)?;
        Signal::from_int(F, v)
    }

    /// In-band FEC: every SILK packet also carries a low-bitrate copy of the previous frame
    /// once the expected loss ([`Self::set_packet_loss_perc`]) is above zero.
    pub fn set_inband_fec(&mut self, on: bool) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_INBAND_FEC)",
            ffi::OPUS_SET_INBAND_FEC_REQUEST,
            c_int::from(on),
        )
    }

    pub fn get_inband_fec(&mut self) -> Result<bool> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_INBAND_FEC)",
            ffi::OPUS_GET_INBAND_FEC_REQUEST,
        )
        .map(|v| v != 0)
    }

    /// Expected packet loss `0..=100` %; drives how many bits FEC and DRED get.
    pub fn set_packet_loss_perc(&mut self, value: i32) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_PACKET_LOSS_PERC)",
            ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            value,
        )
    }

    pub fn get_packet_loss_perc(&mut self) -> Result<i32> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_PACKET_LOSS_PERC)",
            ffi::OPUS_GET_PACKET_LOSS_PERC_REQUEST,
        )
    }

    pub fn set_dtx(&mut self, on: bool) -> Result<()> {
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_DTX)",
            ffi::OPUS_SET_DTX_REQUEST,
            c_int::from(on),
        )
    }

    pub fn get_dtx(&mut self) -> Result<bool> {
        self.ctl_get("opus_encoder_ctl(OPUS_GET_DTX)", ffi::OPUS_GET_DTX_REQUEST)
            .map(|v| v != 0)
    }

    /// Deep REDundancy: embed up to `frames` × 10 ms of neural redundancy for the preceding
    /// audio in every packet (`0` disables, cap [`MAX_DRED_FRAMES`]). The redundancy shares
    /// the target bitrate with the primary frame and is only produced in SILK/hybrid mode
    /// with a non-zero expected loss and a bitrate comfortably above ~20 kbit/s.
    pub fn set_dred_duration(&mut self, frames: u32) -> Result<()> {
        let frames = c_int::try_from(frames.min(MAX_DRED_FRAMES)).unwrap_or(0);
        self.ctl_set(
            "opus_encoder_ctl(OPUS_SET_DRED_DURATION)",
            ffi::OPUS_SET_DRED_DURATION_REQUEST,
            frames,
        )
    }

    pub fn get_dred_duration(&mut self) -> Result<u32> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_DRED_DURATION)",
            ffi::OPUS_GET_DRED_DURATION_REQUEST,
        )
        .map(|v| v.max(0) as u32)
    }

    /// Algorithmic delay of the encoder in samples.
    pub fn get_lookahead(&mut self) -> Result<i32> {
        self.ctl_get(
            "opus_encoder_ctl(OPUS_GET_LOOKAHEAD)",
            ffi::OPUS_GET_LOOKAHEAD_REQUEST,
        )
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_encoder_create` and is destroyed exactly once.
        unsafe { ffi::opus_encoder_destroy(self.ptr.as_ptr()) }
    }
}

/// An Opus decoder.
pub struct Decoder {
    ptr: NonNull<ffi::OpusDecoder>,
    channels: Channels,
}

// SAFETY: see `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(sample_rate: u32, channels: Channels) -> Result<Self> {
        let mut error = 0;
        // SAFETY: plain FFI call; the error slot is a valid out-pointer.
        let ptr =
            unsafe { ffi::opus_decoder_create(sample_rate as i32, channels as c_int, &mut error) };
        match NonNull::new(ptr) {
            Some(ptr) if error == ffi::OPUS_OK => Ok(Self { ptr, channels }),
            _ => Err(Error::new("opus_decoder_create", error)),
        }
    }

    pub fn channels(&self) -> Channels {
        self.channels
    }

    /// Decode `input` into interleaved 16-bit PCM; `output.len() / channels` is the frame
    /// size. An empty `input` runs packet-loss concealment for one frame. With `fec = true`
    /// the *previous* (lost) frame is reconstructed from the in-band redundancy of `input`
    /// (or concealed when it carries none); the frame size must equal the lost frame's.
    /// Returns samples per channel.
    pub fn decode(&mut self, input: &[u8], output: &mut [i16], fec: bool) -> Result<usize> {
        let frame = frame_size("opus_decode", output.len(), self.channels)?;
        let (data, len) = packet_ptr("opus_decode", input)?;
        // SAFETY: `data`/`len` describe `input` (or a NULL PLC request); `output` holds
        // `frame * channels` samples and the decoder was created with `self.channels`.
        let ret = unsafe {
            ffi::opus_decode(
                self.ptr.as_ptr(),
                data,
                len,
                output.as_mut_ptr(),
                frame,
                c_int::from(fec),
            )
        };
        check("opus_decode", ret).map(|n| n as usize)
    }

    /// [`Self::decode`] with float output.
    pub fn decode_float(&mut self, input: &[u8], output: &mut [f32], fec: bool) -> Result<usize> {
        let frame = frame_size("opus_decode_float", output.len(), self.channels)?;
        let (data, len) = packet_ptr("opus_decode_float", input)?;
        // SAFETY: as in `decode`.
        let ret = unsafe {
            ffi::opus_decode_float(
                self.ptr.as_ptr(),
                data,
                len,
                output.as_mut_ptr(),
                frame,
                c_int::from(fec),
            )
        };
        check("opus_decode_float", ret).map(|n| n as usize)
    }

    /// Reconstruct one lost frame from DRED parsed out of a later packet. `offset` is the
    /// distance of the lost frame from the start of that later packet, in samples (at the
    /// decoder's rate) — `frame_size` for the frame right before it, `2 * frame_size` for the
    /// one before that, and so on up to what [`DredDecoder::parse`] reported available.
    pub fn dred_decode_float(
        &mut self,
        dred: &Dred,
        offset: usize,
        output: &mut [f32],
    ) -> Result<usize> {
        const F: &str = "opus_decoder_dred_decode_float";
        let frame = frame_size(F, output.len(), self.channels)?;
        let offset = c_int::try_from(offset).map_err(|_| Error::new(F, ffi::OPUS_BAD_ARG))?;
        // SAFETY: `dred` is a live parsed state; `output` holds `frame * channels` samples.
        let ret = unsafe {
            ffi::opus_decoder_dred_decode_float(
                self.ptr.as_ptr(),
                dred.ptr.as_ptr(),
                offset,
                output.as_mut_ptr(),
                frame,
            )
        };
        check(F, ret).map(|n| n as usize)
    }

    /// [`Self::dred_decode_float`] with 16-bit output.
    pub fn dred_decode(&mut self, dred: &Dred, offset: usize, output: &mut [i16]) -> Result<usize> {
        const F: &str = "opus_decoder_dred_decode";
        let frame = frame_size(F, output.len(), self.channels)?;
        let offset = c_int::try_from(offset).map_err(|_| Error::new(F, ffi::OPUS_BAD_ARG))?;
        // SAFETY: as in `dred_decode_float`.
        let ret = unsafe {
            ffi::opus_decoder_dred_decode(
                self.ptr.as_ptr(),
                dred.ptr.as_ptr(),
                offset,
                output.as_mut_ptr(),
                frame,
            )
        };
        check(F, ret).map(|n| n as usize)
    }

    /// Samples per channel `packet` would decode to with this decoder.
    pub fn get_nb_samples(&self, packet: &[u8]) -> Result<usize> {
        const F: &str = "opus_decoder_get_nb_samples";
        let len = byte_len(F, packet.len())?;
        // SAFETY: `packet` is a live slice of `len` bytes.
        let ret =
            unsafe { ffi::opus_decoder_get_nb_samples(self.ptr.as_ptr(), packet.as_ptr(), len) };
        check(F, ret).map(|n| n as usize)
    }

    fn ctl_set(&mut self, function: &'static str, request: c_int, value: c_int) -> Result<()> {
        // SAFETY: every `OPUS_SET_*` request used here takes exactly one `opus_int32`.
        let ret = unsafe { ffi::opus_decoder_ctl(self.ptr.as_ptr(), request, value) };
        check(function, ret).map(|_| ())
    }

    fn ctl_get(&mut self, function: &'static str, request: c_int) -> Result<c_int> {
        let mut value: c_int = 0;
        // SAFETY: every `OPUS_GET_*` request used here takes exactly one `opus_int32*`.
        let ret = unsafe { ffi::opus_decoder_ctl(self.ptr.as_ptr(), request, &mut value) };
        check(function, ret).map(|_| value)
    }

    pub fn reset_state(&mut self) -> Result<()> {
        // SAFETY: `OPUS_RESET_STATE` takes no argument.
        let ret = unsafe { ffi::opus_decoder_ctl(self.ptr.as_ptr(), ffi::OPUS_RESET_STATE) };
        check("opus_decoder_ctl(OPUS_RESET_STATE)", ret).map(|_| ())
    }

    /// Decoder complexity `0..=10`: `>= 5` switches concealment to the neural PLC, `>= 6`
    /// enables OSCE LACE speech enhancement, `>= 7` NoLACE (highest quality, ~4× the CPU).
    pub fn set_complexity(&mut self, value: i32) -> Result<()> {
        self.ctl_set(
            "opus_decoder_ctl(OPUS_SET_COMPLEXITY)",
            ffi::OPUS_SET_COMPLEXITY_REQUEST,
            value,
        )
    }

    pub fn get_complexity(&mut self) -> Result<i32> {
        self.ctl_get(
            "opus_decoder_ctl(OPUS_GET_COMPLEXITY)",
            ffi::OPUS_GET_COMPLEXITY_REQUEST,
        )
    }

    /// OSCE bandwidth extension: a 48 kHz decoder at complexity `>= 4` widens wideband SILK
    /// speech (16 kHz internal) to fullband with a neural model.
    pub fn set_osce_bwe(&mut self, on: bool) -> Result<()> {
        self.ctl_set(
            "opus_decoder_ctl(OPUS_SET_OSCE_BWE)",
            ffi::OPUS_SET_OSCE_BWE_REQUEST,
            i32::from(on),
        )
    }

    pub fn get_osce_bwe(&mut self) -> Result<bool> {
        self.ctl_get(
            "opus_decoder_ctl(OPUS_GET_OSCE_BWE)",
            ffi::OPUS_GET_OSCE_BWE_REQUEST,
        )
        .map(|v| v != 0)
    }

    /// Duration of the last decoded packet in samples per channel (the frame size a PLC or
    /// FEC call for the following gap has to use).
    pub fn get_last_packet_duration(&mut self) -> Result<usize> {
        self.ctl_get(
            "opus_decoder_ctl(OPUS_GET_LAST_PACKET_DURATION)",
            ffi::OPUS_GET_LAST_PACKET_DURATION_REQUEST,
        )
        .map(|v| v.max(0) as usize)
    }

    /// Decoder gain in Q8 dB (`256` = +1 dB).
    pub fn set_gain(&mut self, q8_db: i32) -> Result<()> {
        self.ctl_set(
            "opus_decoder_ctl(OPUS_SET_GAIN)",
            ffi::OPUS_SET_GAIN_REQUEST,
            q8_db,
        )
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_decoder_create` and is destroyed exactly once.
        unsafe { ffi::opus_decoder_destroy(self.ptr.as_ptr()) }
    }
}

fn packet_ptr(function: &'static str, input: &[u8]) -> Result<(*const u8, i32)> {
    if input.is_empty() {
        Ok((std::ptr::null(), 0))
    } else {
        Ok((input.as_ptr(), byte_len(function, input.len())?))
    }
}

/// Parsed DRED redundancy of one packet; feed to [`Decoder::dred_decode_float`].
pub struct Dred {
    ptr: NonNull<ffi::OpusDRED>,
}

// SAFETY: see `Encoder`.
unsafe impl Send for Dred {}

impl Dred {
    pub fn new() -> Result<Self> {
        let mut error = 0;
        // SAFETY: plain FFI call; the error slot is a valid out-pointer.
        let ptr = unsafe { ffi::opus_dred_alloc(&mut error) };
        match NonNull::new(ptr) {
            Some(ptr) if error == ffi::OPUS_OK => Ok(Self { ptr }),
            _ => Err(Error::new("opus_dred_alloc", error)),
        }
    }
}

impl Drop for Dred {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_dred_alloc` and is freed exactly once.
        unsafe { ffi::opus_dred_free(self.ptr.as_ptr()) }
    }
}

/// Extracts DRED from packets. One per stream is enough; it is independent of the
/// [`Decoder`] that renders the recovered frames.
pub struct DredDecoder {
    ptr: NonNull<ffi::OpusDREDDecoder>,
}

// SAFETY: see `Encoder`.
unsafe impl Send for DredDecoder {}

impl DredDecoder {
    pub fn new() -> Result<Self> {
        let mut error = 0;
        // SAFETY: plain FFI call; the error slot is a valid out-pointer.
        let ptr = unsafe { ffi::opus_dred_decoder_create(&mut error) };
        match NonNull::new(ptr) {
            Some(ptr) if error == ffi::OPUS_OK => Ok(Self { ptr }),
            _ => Err(Error::new("opus_dred_decoder_create", error)),
        }
    }

    /// Parse the DRED extension of `packet` into `dred`, decoding at most `max_samples`
    /// (at `sample_rate`) of history — the length of the gap to fill. Returns how many
    /// samples before the packet's own audio are recoverable: `0` when the packet carries no
    /// DRED (or less than one frame of it).
    pub fn parse(
        &mut self,
        dred: &mut Dred,
        packet: &[u8],
        max_samples: usize,
        sample_rate: u32,
    ) -> Result<usize> {
        const F: &str = "opus_dred_parse";
        if packet.is_empty() {
            return Ok(0);
        }
        let len = byte_len(F, packet.len())?;
        let max = c_int::try_from(max_samples).map_err(|_| Error::new(F, ffi::OPUS_BAD_ARG))?;
        let mut dred_end: c_int = 0;
        // SAFETY: `packet` is a live slice of `len` bytes; both states are live; `dred_end`
        // is a valid out-pointer.
        let ret = unsafe {
            ffi::opus_dred_parse(
                self.ptr.as_ptr(),
                dred.ptr.as_ptr(),
                packet.as_ptr(),
                len,
                max,
                sample_rate as i32,
                &mut dred_end,
                0,
            )
        };
        check(F, ret).map(|n| n as usize)
    }
}

impl Drop for DredDecoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_dred_decoder_create` and is destroyed once.
        unsafe { ffi::opus_dred_decoder_destroy(self.ptr.as_ptr()) }
    }
}

/// Stateful soft clipper (`opus_pcm_soft_clip`) that keeps float PCM within `-1.0..=1.0`
/// without hard wrapping; carries per-channel state across calls.
pub struct SoftClip {
    channels: Channels,
    memory: [f32; 2],
}

impl SoftClip {
    pub fn new(channels: Channels) -> Self {
        Self {
            channels,
            memory: [0.0; 2],
        }
    }

    /// Clip `signal` (interleaved, `len / channels` frames) in place.
    pub fn apply(&mut self, signal: &mut [f32]) {
        let n = signal.len() / self.channels.count();
        let Ok(frames) = c_int::try_from(n) else {
            return;
        };
        // SAFETY: `signal` holds `frames * channels` floats and `memory` has one slot per
        // channel (at most two).
        unsafe {
            ffi::opus_pcm_soft_clip(
                signal.as_mut_ptr(),
                frames,
                self.channels as c_int,
                self.memory.as_mut_ptr(),
            )
        }
    }
}

/// Stateless inspection of Opus packets.
pub mod packet {
    use super::*;

    /// Channels announced by the TOC byte.
    pub fn get_nb_channels(packet: &[u8]) -> Result<Channels> {
        const F: &str = "opus_packet_get_nb_channels";
        if packet.is_empty() {
            return Err(Error::new(F, ffi::OPUS_BAD_ARG));
        }
        // SAFETY: only the first byte (which exists) is read.
        let ret = unsafe { ffi::opus_packet_get_nb_channels(packet.as_ptr()) };
        match check(F, ret)? {
            1 => Ok(Channels::Mono),
            _ => Ok(Channels::Stereo),
        }
    }

    /// Coded bandwidth announced by the TOC byte.
    pub fn get_bandwidth(packet: &[u8]) -> Result<Bandwidth> {
        const F: &str = "opus_packet_get_bandwidth";
        if packet.is_empty() {
            return Err(Error::new(F, ffi::OPUS_BAD_ARG));
        }
        // SAFETY: only the first byte (which exists) is read.
        let ret = unsafe { ffi::opus_packet_get_bandwidth(packet.as_ptr()) };
        Bandwidth::from_int(F, check(F, ret)?)
    }

    /// Samples per channel at `sample_rate` the packet decodes to.
    pub fn get_nb_samples(packet: &[u8], sample_rate: u32) -> Result<usize> {
        const F: &str = "opus_packet_get_nb_samples";
        let len = byte_len(F, packet.len())?;
        if len == 0 {
            return Err(Error::new(F, ffi::OPUS_BAD_ARG));
        }
        // SAFETY: `packet` is a live slice of `len` bytes.
        let ret =
            unsafe { ffi::opus_packet_get_nb_samples(packet.as_ptr(), len, sample_rate as i32) };
        check(F, ret).map(|n| n as usize)
    }

    /// Whether the packet carries in-band FEC (LBRR) for the previous frame.
    pub fn has_lbrr(packet: &[u8]) -> Result<bool> {
        const F: &str = "opus_packet_has_lbrr";
        let len = byte_len(F, packet.len())?;
        if len == 0 {
            return Err(Error::new(F, ffi::OPUS_BAD_ARG));
        }
        // SAFETY: `packet` is a live slice of `len` bytes.
        let ret = unsafe { ffi::opus_packet_has_lbrr(packet.as_ptr(), len) };
        check(F, ret).map(|v| v != 0)
    }
}

/// Synthetic signals for codec tests across the workspace: SILK keeps classifying them as
/// speech, so LBRR (in-band FEC) and DRED are produced in steady state where a stationary tone
/// would be adapted away as background noise.
#[doc(hidden)]
pub mod testing {
    /// Syllable-like bursts (glottal-pulse harmonics with moving formant emphasis) separated
    /// by short pauses, `frames` × 20 ms at 48 kHz, full scale about ±0.3.
    pub fn speech_like_f32(frames: usize) -> Vec<f32> {
        let mut seed = 0x1234_5678u32;
        (0..frames * 960)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let syllable = (t / 0.18).floor();
                let phase = (t % 0.18) / 0.18;
                let voiced = phase < 0.7;
                let f0 = 110.0 + 25.0 * (syllable * 0.9).sin() + 15.0 * (t * 4.0).sin();
                let formant = 500.0 + 400.0 * ((syllable * 1.7).sin() + 1.0);
                let env = if voiced {
                    (phase / 0.1).min(1.0) * ((0.7 - phase) / 0.1).min(1.0)
                } else {
                    0.0
                };
                let mut s = 0.0;
                for h in 1..=12 {
                    let f = f0 * h as f32;
                    let weight = 1.0 / (1.0 + ((f - formant) / 300.0).powi(2));
                    s += (2.0 * std::f32::consts::PI * f * t).sin() * weight;
                }
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 1.0) * 0.02;
                (s * 0.4 * env + noise) * 0.27
            })
            .collect()
    }

    /// [`speech_like_f32`] scaled to i16 (about ±9000).
    pub fn speech_like_i16(frames: usize) -> Vec<i16> {
        speech_like_f32(frames)
            .into_iter()
            .map(|s| (s / 0.27 * 9_000.0) as i16)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const FRAME: usize = 960;

    fn speech_like(frames: usize) -> Vec<i16> {
        testing::speech_like_i16(frames)
    }

    fn voip_encoder(bitrate: i32) -> Encoder {
        let mut e = Encoder::new(RATE, Channels::Mono, Application::Voip).unwrap();
        e.set_bitrate(Bitrate::Bits(bitrate)).unwrap();
        e.set_signal(Signal::Voice).unwrap();
        e.set_max_bandwidth(Bandwidth::Wideband).unwrap();
        e.set_complexity(10).unwrap();
        e
    }

    fn energy(pcm: &[i16]) -> f64 {
        pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len().max(1) as f64
    }

    /// Best normalized cross-correlation of `decoded` against `original` over the codec's
    /// possible delays (0..400 samples).
    fn similarity(decoded: &[i16], original: &[i16]) -> f64 {
        let n = decoded.len().min(original.len()) - 400;
        (0..400)
            .map(|lag| {
                let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
                for i in 0..n {
                    let x = decoded[i] as f64;
                    let y = original[i + lag] as f64;
                    xy += x * y;
                    xx += x * x;
                    yy += y * y;
                }
                xy / (xx * yy).sqrt().max(1.0)
            })
            .fold(f64::MIN, f64::max)
    }

    #[test]
    fn reports_libopus_1_6() {
        let v = version();
        assert!(v.starts_with("libopus 1."), "{v}");
        let minor: u32 = v
            .trim_start_matches("libopus 1.")
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap();
        assert!(minor >= 5, "DRED/OSCE need libopus >= 1.5: {v}");
    }

    #[test]
    fn encode_decode_round_trip_and_controls() {
        let mut e = voip_encoder(32_000);
        e.set_vbr(true).unwrap();
        e.set_vbr_constraint(true).unwrap();
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(10).unwrap();
        e.set_dtx(false).unwrap();
        assert_eq!(e.get_bitrate().unwrap(), Bitrate::Bits(32_000));
        assert_eq!(e.get_signal().unwrap(), Signal::Voice);
        assert_eq!(e.get_max_bandwidth().unwrap(), Bandwidth::Wideband);
        assert_eq!(e.get_complexity().unwrap(), 10);
        assert!(e.get_vbr().unwrap());
        assert!(e.get_vbr_constraint().unwrap());
        assert!(e.get_inband_fec().unwrap());
        assert_eq!(e.get_packet_loss_perc().unwrap(), 10);
        assert!(!e.get_dtx().unwrap());
        assert!(e.get_lookahead().unwrap() > 0);

        let pcm = speech_like(10);
        let mut d = Decoder::new(RATE, Channels::Mono).unwrap();
        let mut out = vec![0i16; FRAME];
        for frame in pcm.chunks(FRAME) {
            let packet = e.encode_vec(frame, 1275).unwrap();
            assert_eq!(packet::get_nb_channels(&packet).unwrap(), Channels::Mono);
            assert_eq!(packet::get_nb_samples(&packet, RATE).unwrap(), FRAME);
            assert_eq!(d.get_nb_samples(&packet).unwrap(), FRAME);
            assert_eq!(d.decode(&packet, &mut out, false).unwrap(), FRAME);
            assert_eq!(d.get_last_packet_duration().unwrap(), FRAME);
        }
        assert!(energy(&out) > 1.0e5, "decoded audio is silent");
        let mut f = vec![0f32; FRAME];
        assert_eq!(d.decode_float(&[], &mut f, false).unwrap(), FRAME);
    }

    #[test]
    fn rejects_bad_frame_sizes_and_reports_functions() {
        let mut e = voip_encoder(24_000);
        let mut out = [0u8; 400];
        let err = e.encode(&[0i16; 7], &mut out).unwrap_err();
        assert_eq!(err.code(), ErrorCode::BadArg);
        assert_eq!(err.to_string(), "opus_encode: invalid argument");
        let mut d = Decoder::new(RATE, Channels::Stereo).unwrap();
        let mut pcm = [0i16; 961];
        assert_eq!(
            d.decode(&[], &mut pcm, false).unwrap_err().code(),
            ErrorCode::BadArg
        );
        let mut pcm = [0i16; 2 * FRAME];
        assert_eq!(
            d.decode(&[0xff, 0xff, 0xff], &mut pcm, false)
                .unwrap_err()
                .code(),
            ErrorCode::InvalidPacket
        );
        assert!(d.set_complexity(11).is_err());
        assert!(e.set_dred_duration(MAX_DRED_FRAMES + 50).is_ok());
        assert_eq!(e.get_dred_duration().unwrap(), MAX_DRED_FRAMES);
    }

    #[test]
    fn inband_fec_recovers_the_previous_frame() {
        let mut e = voip_encoder(32_000);
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(20).unwrap();
        let pcm = speech_like(30);
        let packets: Vec<Vec<u8>> = pcm
            .chunks(FRAME)
            .map(|f| e.encode_vec(f, 1275).unwrap())
            .collect();
        let with_lbrr = packets
            .iter()
            .filter(|p| packet::has_lbrr(p).unwrap())
            .count();
        assert!(
            with_lbrr * 2 > packets.len(),
            "LBRR in {with_lbrr}/{} packets",
            packets.len()
        );
        // Lose a frame whose successor carries redundancy for it.
        let lost = (12..packets.len() - 1)
            .find(|&k| packet::has_lbrr(&packets[k + 1]).unwrap())
            .expect("a packet with LBRR");
        let mut d = Decoder::new(RATE, Channels::Mono).unwrap();
        let mut out = vec![0i16; FRAME];
        for p in &packets[..lost] {
            d.decode(p, &mut out, false).unwrap();
        }
        let mut fec = vec![0i16; FRAME];
        assert_eq!(d.decode(&packets[lost + 1], &mut fec, true).unwrap(), FRAME);
        d.decode(&packets[lost + 1], &mut out, false).unwrap();
        // The encoder delays its output by the lookahead, so compare against the original
        // from the previous frame on and let `similarity` find the lag.
        let original = &pcm[(lost - 1) * FRAME..(lost + 1) * FRAME];
        let sim = similarity(&fec, &original[FRAME - 400..]);
        assert!(sim > 0.6, "FEC frame correlates {sim:.2} with the original");
        let e_fec = energy(&fec);
        let e_orig = energy(&original[FRAME..]);
        assert!(
            e_fec > e_orig * 0.2 && e_fec < e_orig * 5.0,
            "FEC frame energy {e_fec} vs original {e_orig}"
        );
    }

    #[test]
    fn dred_reconstructs_a_burst_of_lost_frames() {
        let mut e = voip_encoder(48_000);
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(30).unwrap();
        e.set_dred_duration(50).unwrap();
        assert_eq!(e.get_dred_duration().unwrap(), 50);
        let pcm = speech_like(60);
        let packets: Vec<Vec<u8>> = pcm
            .chunks(FRAME)
            .map(|f| e.encode_vec(f, 1275).unwrap())
            .collect();

        let mut d = Decoder::new(RATE, Channels::Mono).unwrap();
        d.set_complexity(DEEP_PLC_COMPLEXITY).unwrap();
        let mut dred_dec = DredDecoder::new().unwrap();
        let mut dred = Dred::new().unwrap();
        let mut out = vec![0i16; FRAME];
        for p in &packets[..30] {
            d.decode(p, &mut out, false).unwrap();
        }
        // Frames 30..=39 are lost; packet 40 arrives.
        let lost = 10;
        let available = dred_dec
            .parse(&mut dred, &packets[40], lost * FRAME, RATE)
            .unwrap();
        assert!(
            available >= lost * FRAME,
            "DRED covers {available} samples, need {}",
            lost * FRAME
        );
        let mut recovered = Vec::with_capacity(lost * FRAME);
        let mut f = vec![0f32; FRAME];
        for k in 0..lost {
            let offset = (lost - k) * FRAME;
            assert_eq!(d.dred_decode_float(&dred, offset, &mut f).unwrap(), FRAME);
            recovered.extend(f.iter().map(|s| (s * 32_767.0) as i16));
        }
        d.decode(&packets[40], &mut out, false).unwrap();
        let original = &pcm[30 * FRAME..40 * FRAME];
        let e_rec = energy(&recovered);
        let e_orig = energy(original);
        assert!(
            e_rec > e_orig * 0.1 && e_rec < e_orig * 10.0,
            "DRED energy {e_rec} vs original {e_orig}"
        );
        // A packet without DRED parses to zero rather than failing.
        let mut plain = voip_encoder(32_000);
        let p = plain.encode_vec(&pcm[..FRAME], 1275).unwrap();
        assert_eq!(dred_dec.parse(&mut dred, &p, FRAME, RATE).unwrap(), 0);
    }

    #[test]
    fn decoder_complexity_selects_neural_paths() {
        let mut d = Decoder::new(RATE, Channels::Mono).unwrap();
        assert_eq!(d.get_complexity().unwrap(), 0);
        for c in [DEEP_PLC_COMPLEXITY, OSCE_COMPLEXITY, 7, 10] {
            d.set_complexity(c).unwrap();
            assert_eq!(d.get_complexity().unwrap(), c);
        }
        assert!(!d.get_osce_bwe().unwrap());
        d.set_osce_bwe(true).unwrap();
        assert!(d.get_osce_bwe().unwrap());
        let mut e = voip_encoder(24_000);
        let pcm = speech_like(20);
        let mut out = vec![0i16; FRAME];
        for f in pcm.chunks(FRAME) {
            let p = e.encode_vec(f, 1275).unwrap();
            d.decode(&p, &mut out, false).unwrap();
        }
        // Neural PLC on a gap after real speech produces non-silent output.
        d.decode(&[], &mut out, false).unwrap();
        assert!(energy(&out) > 1.0e3);
        d.reset_state().unwrap();
    }
}
