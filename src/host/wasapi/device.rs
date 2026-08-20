use std::{
    ffi::OsString,
    fmt,
    hash::Hash,
    mem,
    os::windows::ffi::OsStringExt,
    ptr, slice,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64},
    },
    time::Duration,
};

use crate::{
    BufferSize, COMMON_SAMPLE_RATES, CallbackInfo, Data, DeviceDescription,
    DeviceDescriptionBuilder, DeviceDirection, DeviceId, DeviceType, Error, ErrorKind, FrameCount,
    InterfaceType, SampleFormat, SampleRate, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
    error::ResultExt,
    host::{ErrorCallbackArc, com::ComString, container_align},
};

use windows::{
    Win32::{
        Devices::Properties,
        Foundation::{ERROR_TIMEOUT, PROPERTYKEY},
        Media::{Audio, Audio::IAudioRenderClient, KernelStreaming, Multimedia},
        System::{
            Com,
            Com::{STGM_READ, StructuredStorage},
            Threading,
            Variant::{VT_LPWSTR, VT_UI4},
        },
        UI::Shell::PropertiesSystem::IPropertyStore,
    },
    core::{GUID, Interface},
};

use super::{
    ShareMode,
    stream::{AudioClientFlow, DefaultDeviceMonitor, Stream, StreamInner},
};
pub use crate::iter::{SupportedInputConfigs, SupportedOutputConfigs};
use crate::{host::com, traits::DeviceTrait};

// PKEY_AudioEndpoint properties not yet in windows-rs

/// PKEY_AudioEndpoint_FormFactor (PID 0) - VT_UI4 containing EndpointFormFactor enum
const PKEY_AUDIOENDPOINT_FORMFACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// PKEY_AudioEndpoint_JackSubType (PID 8) - VT_LPWSTR containing KS node type GUID
const PKEY_AUDIOENDPOINT_JACKSUBTYPE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 8,
};

const DEFAULT_FLAGS: u32 = Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

/// Wrapper because of that stupid decision to remove `Send` and `Sync` from raw pointers.
#[derive(Clone)]
struct IAudioClientWrapper(Audio::IAudioClient);
unsafe impl Send for IAudioClientWrapper {}
unsafe impl Sync for IAudioClientWrapper {}

/// Distinguishes how a `Device` was obtained so streams know which activation path to use.
#[derive(Clone, Debug)]
enum DeviceHandle {
    DefaultOutput,
    DefaultInput,
    Specific(Audio::IMMDevice),
}

/// An opaque type that identifies an end point.
#[derive(Clone)]
pub struct Device {
    device: DeviceHandle,
    /// We cache an uninitialized `IAudioClient` so that we can call functions from it without
    /// having to create/destroy audio clients all the time.
    future_audio_client: Arc<Mutex<Option<IAudioClientWrapper>>>, // TODO: add NonZero around the ptr
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn description(&self) -> Result<DeviceDescription, Error> {
        Device::description(self)
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Self::id(self)
    }

    fn supports_input(&self) -> bool {
        self.data_flow() == Audio::eCapture
    }

    fn supports_output(&self) -> bool {
        self.data_flow() == Audio::eRender
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        Self::supported_input_configs(self)
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        Self::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        Self::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Self::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_input_stream_raw_for(
            config,
            sample_format,
            ShareMode::Shared,
            data_callback,
            error_callback,
            timeout,
        )
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_output_stream_raw_for(
            config,
            sample_format,
            ShareMode::Shared,
            data_callback,
            error_callback,
            timeout,
        )
    }
}

struct Endpoint {
    endpoint: Audio::IMMEndpoint,
}

// Use RAII to make sure CoTaskMemFree is called when we are responsible for freeing.
struct WaveFormatExPtr(*mut Audio::WAVEFORMATEX);

impl Drop for WaveFormatExPtr {
    fn drop(&mut self) {
        unsafe {
            Com::CoTaskMemFree(Some(self.0 as *mut _));
        }
    }
}

unsafe fn immendpoint_from_immdevice(device: Audio::IMMDevice) -> Audio::IMMEndpoint {
    device
        .cast::<Audio::IMMEndpoint>()
        .expect("could not query IMMDevice interface for IMMEndpoint")
}

unsafe fn data_flow_from_immendpoint(endpoint: &Audio::IMMEndpoint) -> Audio::EDataFlow {
    unsafe { endpoint.GetDataFlow() }.expect("could not get endpoint data_flow")
}

/// Translates the public share mode into the WASAPI constant.
fn to_winapi_share_mode(share_mode: ShareMode) -> Audio::AUDCLNT_SHAREMODE {
    match share_mode {
        ShareMode::Shared => Audio::AUDCLNT_SHAREMODE_SHARED,
        ShareMode::Exclusive => Audio::AUDCLNT_SHAREMODE_EXCLUSIVE,
    }
}

// Given the audio client and format, returns whether the device supports it natively in
// `share_mode`, without format conversion.
pub unsafe fn is_format_supported(
    client: &Audio::IAudioClient,
    share_mode: ShareMode,
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
) -> Result<bool, Error> {
    let hr = match share_mode {
        ShareMode::Shared => {
            let mut closest_match: *mut Audio::WAVEFORMATEX = ptr::null_mut();
            let hr = unsafe {
                client.IsFormatSupported(
                    Audio::AUDCLNT_SHAREMODE_SHARED,
                    waveformatex_ptr,
                    Some(&mut closest_match),
                )
            };
            if !closest_match.is_null() {
                let _free = WaveFormatExPtr(closest_match);
            }
            hr
        }
        // Exclusive mode has no closest match to report: the endpoint either accepts the format
        // or it does not, and the out-parameter is documented as taking NULL here.
        ShareMode::Exclusive => unsafe {
            client.IsFormatSupported(Audio::AUDCLNT_SHAREMODE_EXCLUSIVE, waveformatex_ptr, None)
        },
    };

    // S_OK (hr.0 == 0): format is natively supported, Initialize will accept it without conversion.
    // S_FALSE (hr.0 == 1): shared mode only, the engine proposed a closest match; that is usable
    // only when AUTOCONVERTPCM is set (output). Exclusive mode reports
    // AUDCLNT_E_UNSUPPORTED_FORMAT rather than S_FALSE.
    Ok(hr.0 == 0)
}

// Get a cpal Format from a WAVEFORMATEX.
unsafe fn format_from_waveformatex_ptr(
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
    audio_client: &Audio::IAudioClient,
) -> Option<SupportedStreamConfig> {
    fn cmp_guid(a: &GUID, b: &GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }
    let sample_format = match unsafe {
        (
            (*waveformatex_ptr).wBitsPerSample,
            (*waveformatex_ptr).wFormatTag as u32,
        )
    } {
        (8, Audio::WAVE_FORMAT_PCM) => SampleFormat::U8,
        (16, Audio::WAVE_FORMAT_PCM) => SampleFormat::I16,
        (32, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F32,
        (64, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F64,
        (n_bits, KernelStreaming::WAVE_FORMAT_EXTENSIBLE) => {
            let waveformatextensible_ptr = waveformatex_ptr as *const Audio::WAVEFORMATEXTENSIBLE;
            let sub = unsafe { (*waveformatextensible_ptr).SubFormat };
            let valid_bits = unsafe { (*waveformatextensible_ptr).Samples.wValidBitsPerSample };

            if cmp_guid(&sub, &KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM) {
                match n_bits {
                    8 => SampleFormat::U8,
                    16 => SampleFormat::I16,
                    24 => SampleFormat::I24,
                    32 if valid_bits == 24 => SampleFormat::I24,
                    32 => SampleFormat::I32,
                    64 => SampleFormat::I64,
                    _ => return None,
                }
            } else if cmp_guid(&sub, &Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
                match n_bits {
                    32 => SampleFormat::F32,
                    64 => SampleFormat::F64,
                    _ => return None,
                }
            } else {
                return None;
            }
        }
        // Unknown data format returned by GetMixFormat.
        _ => return None,
    };

    let sample_rate = unsafe { (*waveformatex_ptr).nSamplesPerSec };

    // GetBufferSizeLimits is only used for Hardware-Offloaded Audio
    // Processing, which was added in Windows 8, which places hardware
    // limits on the size of the audio buffer. If the sound system
    // *isn't* using offloaded audio, we're using a software audio
    // processing stack and have pretty much free rein to set buffer
    // size.
    //
    // In software audio stacks GetBufferSizeLimits returns
    // AUDCLNT_E_OFFLOAD_MODE_ONLY.
    //
    // https://docs.microsoft.com/en-us/windows-hardware/drivers/audio/hardware-offloaded-audio-processing
    let (mut min_buffer_duration, mut max_buffer_duration) = (0, 0);
    let buffer_size_is_limited = audio_client
        .cast::<Audio::IAudioClient2>()
        .and_then(|audio_client| unsafe {
            audio_client.GetBufferSizeLimits(
                waveformatex_ptr,
                true,
                &mut min_buffer_duration,
                &mut max_buffer_duration,
            )
        })
        .is_ok();
    let buffer_size = if buffer_size_is_limited {
        SupportedBufferSize::Range {
            min: buffer_duration_to_frames(min_buffer_duration, sample_rate),
            max: buffer_duration_to_frames(max_buffer_duration, sample_rate),
        }
    } else {
        // Software audio stack: no hardware buffer constraint to report.
        SupportedBufferSize::Unknown
    };

    let format = SupportedStreamConfig {
        channels: unsafe { (*waveformatex_ptr).nChannels } as _,
        sample_rate,
        buffer_size,
        sample_format,
    };
    Some(format)
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// Maps PKEY_AudioEndpoint_JackSubType GUID to InterfaceType.
///
/// The JackSubType property contains a KS node type GUID string from Ksmedia.h
/// that specifies the physical connector type.
fn jacksubtype_to_interface_type(guid_str: &str) -> Option<InterfaceType> {
    let guid_upper = guid_str.to_uppercase();
    let typ = match guid_upper.as_str() {
        "{D9E55EA0-0C89-4692-84FF-EB3C4B0D172F}" => InterfaceType::Hdmi,
        "{E47E4031-3EA6-418D-8F9B-B73843CCB2AD}" => InterfaceType::DisplayPort,
        "{DFF21CE1-F70F-11D0-B917-00A0C9223196}" => InterfaceType::Spdif,
        _ => return None,
    };

    Some(typ)
}

/// Maps WASAPI FormFactor values to DeviceType and optionally InterfaceType.
fn form_factor_to_types(form_factor: u32) -> (DeviceType, Option<InterfaceType>) {
    match form_factor {
        0 => (DeviceType::Unknown, Some(InterfaceType::Network)), // RemoteNetworkDevice
        1 => (DeviceType::Speaker, None),                         // Speakers
        2 => (DeviceType::Unknown, Some(InterfaceType::Line)),    // LineLevel
        3 => (DeviceType::Headphones, None),                      // Headphones
        4 => (DeviceType::Microphone, None),                      // Microphone
        5 => (DeviceType::Headset, None),                         // Headset
        6 => (DeviceType::Handset, None),                         // Handset
        7 => (DeviceType::Unknown, None),                         // UnknownDigitalPassthrough
        8 => (DeviceType::Unknown, Some(InterfaceType::Spdif)),   // SPDIF
        9 => (DeviceType::Unknown, Some(InterfaceType::Hdmi)),    // DigitalAudioDisplayDevice
        _ => (DeviceType::Unknown, None), // UnknownFormFactor or future values
    }
}

/// Maps WASAPI EnumeratorName to InterfaceType.
fn enumerator_to_interface_type(enumerator: &str) -> Option<InterfaceType> {
    let typ = match enumerator.to_uppercase().as_str() {
        "HDAUDIO" => InterfaceType::BuiltIn,
        "USB" => InterfaceType::Usb,
        "BTHENUM" => InterfaceType::Bluetooth,
        "MMDEVAPI" | "SW" => InterfaceType::Virtual,
        _ => return None,
    };
    Some(typ)
}

/// Activates an `IAudioClient` via `ActivateAudioInterfaceAsync` synchronously.
///
/// Used for virtual default-device GUIDs (`DEVINTERFACE_AUDIO_RENDER`/`DEVINTERFACE_AUDIO_CAPTURE`)
/// so that the Windows audio engine automatically reroutes the stream when the system default
/// device changes.
unsafe fn activate_audio_interface_sync(
    device_interface_path: windows::core::PWSTR,
    activation_timeout: Option<Duration>,
) -> windows::core::Result<Audio::IAudioClient> {
    use windows::core::IUnknown;

    #[windows::core::implement(Audio::IActivateAudioInterfaceCompletionHandler)]
    struct CompletionHandler(std::sync::mpsc::Sender<windows::core::Result<IUnknown>>);

    fn retrieve_result(
        operation: &Audio::IActivateAudioInterfaceAsyncOperation,
    ) -> windows::core::Result<IUnknown> {
        let mut result = windows::core::HRESULT::default();
        let mut interface: Option<IUnknown> = None;
        unsafe {
            operation.GetActivateResult(&mut result, &mut interface)?;
        }
        result.ok()?;
        interface.ok_or_else(|| {
            windows::core::Error::new(
                Audio::AUDCLNT_E_DEVICE_INVALIDATED,
                "audio interface not available after activation",
            )
        })
    }

    impl Audio::IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
        fn ActivateCompleted(
            &self,
            operation: windows::core::Ref<'_, Audio::IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let result = operation.ok().and_then(retrieve_result);
            let _ = self.0.send(result);
            Ok(())
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let handler: Audio::IActivateAudioInterfaceCompletionHandler = CompletionHandler(tx).into();
    unsafe {
        Audio::ActivateAudioInterfaceAsync(
            device_interface_path,
            &Audio::IAudioClient::IID,
            None,
            &handler,
        )?;
    }
    // If a timeout was given use it; otherwise block until Windows calls ActivateCompleted.
    // `handler` holds the sender and remains live on the stack, so `recv` cannot fail.
    let result = if let Some(dur) = activation_timeout {
        rx.recv_timeout(dur).map_err(|_| {
            windows::core::Error::new(
                ERROR_TIMEOUT.to_hresult(),
                "timeout waiting for audio interface activation",
            )
        })?
    } else {
        rx.recv().expect("activation channel closed; this is a bug")
    };
    result?.cast()
}

impl Device {
    pub fn description(&self) -> Result<DeviceDescription, Error> {
        let device = self.immdevice().ok_or_else(|| {
            Error::with_message(ErrorKind::DeviceNotAvailable, "Default device not found")
        })?;
        unsafe {
            // Open the device's property store.
            let property_store = device
                .OpenPropertyStore(STGM_READ)
                .expect("could not open property store");

            // Query all available properties
            let friendly_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const _,
            );

            let device_desc = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_DeviceDesc as *const _ as *const _,
            );

            let interface_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_DeviceInterface_FriendlyName as *const _ as *const _,
            );

            let enumerator_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_EnumeratorName as *const _ as *const _,
            );

            let form_factor = get_property_u32(
                &property_store,
                &PKEY_AUDIOENDPOINT_FORMFACTOR as *const _ as *const _,
            );

            let jack_subtype = get_property_string(
                &property_store,
                &PKEY_AUDIOENDPOINT_JACKSUBTYPE as *const _ as *const _,
            );

            // Prefer FriendlyName for name (e.g., "Speakers (XYZ Audio Adapter)"), fall back to DeviceDesc
            let name = friendly_name.or(device_desc).ok_or_else(|| {
                Error::with_message(
                    ErrorKind::DeviceNotAvailable,
                    "Failed to retrieve device name",
                )
            })?;

            // Get direction from data flow (eCapture = Input, eRender = Output)
            let direction = self.data_flow().into();

            // Determine device_type and initial interface_type from FormFactor
            let (device_type, mut interface_type) = form_factor
                .map(form_factor_to_types)
                .unwrap_or((DeviceType::Unknown, None));

            // Override interface_type from EnumeratorName if available
            if let Some(ref enumerator) = enumerator_name {
                if let Some(itype) = enumerator_to_interface_type(enumerator) {
                    interface_type = Some(itype);
                }
            }

            // JackSubType has highest priority for interface_type
            if let Some(ref jack_guid) = jack_subtype {
                if let Some(itype) = jacksubtype_to_interface_type(jack_guid) {
                    interface_type = Some(itype);
                }
            }

            let mut builder = DeviceDescriptionBuilder::new(name)
                .direction(direction)
                .device_type(device_type);

            if let Some(itype) = interface_type {
                builder = builder.interface_type(itype);
            }

            // Add interface name to driver field if available
            if let Some(iface_name) = interface_name {
                builder = builder.driver(iface_name);
            }

            Ok(builder.build())
        }
    }

    fn id(&self) -> Result<DeviceId, Error> {
        let device = self.immdevice().ok_or_else(|| {
            Error::with_message(ErrorKind::DeviceNotAvailable, "Default device not found")
        })?;
        unsafe {
            match device.GetId() {
                Ok(pwstr) => match pwstr.to_string() {
                    Ok(id_str) => Ok(DeviceId::new(crate::platform::HostId::Wasapi, id_str)),
                    Err(e) => Err(Error::with_message(
                        ErrorKind::BackendError,
                        format!("Failed to convert device ID to string: {e}"),
                    )),
                },
                Err(e) => Err(Error::from(e)),
            }
        }
    }

    fn from_immdevice(device: Audio::IMMDevice) -> Self {
        Device {
            device: DeviceHandle::Specific(device),
            future_audio_client: Arc::new(Mutex::new(None)),
        }
    }

    fn default_output() -> Self {
        Device {
            device: DeviceHandle::DefaultOutput,
            future_audio_client: Arc::new(Mutex::new(None)),
        }
    }

    fn default_input() -> Self {
        Device {
            device: DeviceHandle::DefaultInput,
            future_audio_client: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the underlying `IMMDevice`, resolving the current one for default devices.
    pub fn immdevice(&self) -> Option<Audio::IMMDevice> {
        match &self.device {
            DeviceHandle::DefaultOutput => current_default_endpoint(Audio::eRender),
            DeviceHandle::DefaultInput => current_default_endpoint(Audio::eCapture),
            DeviceHandle::Specific(device) => Some(device.clone()),
        }
    }

    /// Creates a `DefaultDeviceMonitor` for default-device streams, or `None` for specific devices.
    fn default_device_monitor(&self) -> Result<Option<DefaultDeviceMonitor>, Error> {
        let flow = match &self.device {
            DeviceHandle::DefaultOutput => Audio::eRender,
            DeviceHandle::DefaultInput => Audio::eCapture,
            DeviceHandle::Specific(_) => return Ok(None),
        };
        let enumerator = get_enumerator()
            .context("Failed to get device enumerator")?
            .0
            .clone();
        DefaultDeviceMonitor::new(enumerator, flow).map(Some)
    }

    /// Ensures that `future_audio_client` contains a `Some` and returns a locked mutex to it.
    fn ensure_future_audio_client(
        &self,
        activation_timeout: Option<Duration>,
    ) -> Result<MutexGuard<'_, Option<IAudioClientWrapper>>, Error> {
        let mut lock = self.future_audio_client.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "Stream lock poisoned")
        })?;
        if lock.is_some() {
            return Ok(lock);
        }

        let audio_client: Audio::IAudioClient = unsafe {
            match &self.device {
                DeviceHandle::DefaultOutput => {
                    let path = Com::StringFromIID(&Audio::DEVINTERFACE_AUDIO_RENDER)
                        .map_err(Error::from)?;
                    let _guard = ComString(path);
                    activate_audio_interface_sync(path, activation_timeout).map_err(Error::from)?
                }
                DeviceHandle::DefaultInput => {
                    let path = Com::StringFromIID(&Audio::DEVINTERFACE_AUDIO_CAPTURE)
                        .map_err(Error::from)?;
                    let _guard = ComString(path);
                    activate_audio_interface_sync(path, activation_timeout).map_err(Error::from)?
                }
                DeviceHandle::Specific(device) => {
                    // can fail if the device has been disconnected since we enumerated it, or if
                    // the device doesn't support playback for some reason
                    device
                        .Activate(Com::CLSCTX_ALL, None)
                        .map_err(Error::from)?
                }
            }
        };

        *lock = Some(IAudioClientWrapper(audio_client));
        Ok(lock)
    }

    /// Returns an uninitialized `IAudioClient`.
    pub(crate) fn build_audioclient(
        &self,
        activation_timeout: Option<Duration>,
    ) -> Result<Audio::IAudioClient, Error> {
        let mut lock = self.ensure_future_audio_client(activation_timeout)?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        Ok(lock.take().unwrap().0)
    }

    // There is no way to query the list of all formats that are supported by the
    // audio processor, so instead we just trial some commonly supported formats.
    //
    // Common formats are trialed by first getting the default format (returned via
    // `GetMixFormat`) and then mutating that format with common sample rates and
    // querying them via `IsFormatSupported`.
    //
    // When calling `IsFormatSupported` with the shared-mode audio engine, only the default
    // number of channels seems to be supported. Any, more or less returns an invalid
    // parameter error. Thus, we just assume that the default number of channels is the only
    // number supported.
    fn supported_formats(&self, share_mode: ShareMode) -> Result<SupportedInputConfigs, Error> {
        // initializing COM because we call `CoTaskMemFree` to release the format.
        com::com_initialized();

        // Retrieve the `IAudioClient`.
        let lock = self
            .ensure_future_audio_client(None)
            .context("Failed to get audio client")?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        let client = &lock.as_ref().unwrap().0;

        unsafe {
            // Retrieve the pointer to the default WAVEFORMATEX.
            let default_waveformatex_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .context("Failed to get mix format")?;

            // If the default format can't succeed we have no hope of finding other formats.
            //
            // Only meaningful in shared mode: `GetMixFormat` describes the engine's mixing
            // format, which an endpoint routinely refuses in exclusive mode (it is typically
            // 32-bit float where the hardware is 16- or 24-bit integer). Applying this check to
            // exclusive mode would reject devices that support exclusive mode perfectly well.
            if share_mode == ShareMode::Shared
                && !is_format_supported(client, share_mode, default_waveformatex_ptr.0)?
            {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedConfig,
                    "Could not determine support for default audio format",
                ));
            }

            let format = match format_from_waveformatex_ptr(default_waveformatex_ptr.0, client) {
                Some(fmt) => fmt,
                None => {
                    return Err(Error::with_message(
                        ErrorKind::UnsupportedConfig,
                        "Default audio format could not be mapped to a supported configuration",
                    ));
                }
            };

            // Shared-mode output streams use AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM so Initialize
            // accepts any format regardless of what IsFormatSupported returns. Capture streams
            // do not, and neither does exclusive mode in either direction: there only native
            // formats will work.
            let assume_convertible =
                share_mode == ShareMode::Shared && self.data_flow() == Audio::eRender;

            // For convertible output, restrict to rates the MF Resampler can handle. Exclusive
            // mode pays a blocking driver round-trip per probe, so it stops at the rates PCM
            // hardware runs. Shared-mode capture is answered by the engine, so it probes all.
            let mut sample_rates: Vec<SampleRate> = COMMON_SAMPLE_RATES
                .iter()
                .copied()
                .filter(|&r| {
                    if assume_convertible {
                        (OUTPUT_MIN_SAMPLE_RATE..=OUTPUT_MAX_SAMPLE_RATE).contains(&r)
                    } else if share_mode == ShareMode::Exclusive {
                        (EXCLUSIVE_MIN_SAMPLE_RATE..=EXCLUSIVE_MAX_SAMPLE_RATE).contains(&r)
                    } else {
                        true
                    }
                })
                .collect();
            // The endpoint's own rate is probed whether or not it falls inside those bounds.
            if !sample_rates.contains(&format.sample_rate) {
                sample_rates.push(format.sample_rate);
            }

            let sample_formats: &[SampleFormat] = match share_mode {
                ShareMode::Shared => &WAVEFORMATEXTENSIBLE_SAMPLE_FORMATS,
                ShareMode::Exclusive => &EXCLUSIVE_SAMPLE_FORMATS,
            };

            let device_periods_hns = device_periods_hns(client);

            let mut supported_formats = Vec::new();
            for sample_rate in sample_rates {
                let buffer_size = match format.buffer_size {
                    // Software stacks: substitute what the device period allows at this rate.
                    SupportedBufferSize::Unknown => device_periods_hns
                        .map(|periods| period_buffer_size(periods, share_mode, sample_rate))
                        .unwrap_or(SupportedBufferSize::Unknown),
                    // Hardware stacks: report the hardware buffer size limits as-is.
                    other => other,
                };

                for sample_format in sample_formats.iter().copied() {
                    if let Some((waveformat, _)) = config_to_waveformatextensible(
                        StreamConfig {
                            channels: format.channels,
                            sample_rate,
                            buffer_size: BufferSize::Default,
                        },
                        sample_format,
                        share_mode,
                    ) {
                        let usable = assume_convertible
                            || is_format_supported(
                                client,
                                share_mode,
                                &waveformat.Format as *const Audio::WAVEFORMATEX,
                            )?;
                        if usable {
                            supported_formats.push(SupportedStreamConfigRange {
                                channels: format.channels,
                                min_sample_rate: sample_rate,
                                max_sample_rate: sample_rate,
                                buffer_size,
                                sample_format,
                            });
                        }
                    }
                }
            }
            Ok(supported_formats.into_iter())
        }
    }

    pub fn supported_input_configs(&self) -> Result<SupportedInputConfigs, Error> {
        self.supported_input_configs_for(ShareMode::Shared)
    }

    pub(crate) fn supported_input_configs_for(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedInputConfigs, Error> {
        if self.data_flow() == Audio::eCapture {
            self.supported_formats(share_mode)
        // If it's an output device, assume no input formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    pub fn supported_output_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        self.supported_output_configs_for(ShareMode::Shared)
    }

    pub(crate) fn supported_output_configs_for(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedOutputConfigs, Error> {
        if self.data_flow() == Audio::eRender {
            self.supported_formats(share_mode)
        // If it's an input device, assume no output formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    // In shared mode all samples go through an audio processor to mix them together, and one
    // format is guaranteed to be supported: the one returned by `GetMixFormat`.
    //
    // In exclusive mode there is no mixer, so the mix format carries no such guarantee — it
    // describes the engine, not the endpoint. There the device's channel count and sample rate
    // are taken from the mix format but the sample format is probed for, most preferred first.
    fn default_format(&self, share_mode: ShareMode) -> Result<SupportedStreamConfig, Error> {
        // initializing COM because we call `CoTaskMemFree`
        com::com_initialized();

        let lock = self
            .ensure_future_audio_client(None)
            .context("Failed to get audio client")?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        let client = &lock.as_ref().unwrap().0;

        unsafe {
            let format_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .context("Failed to get mix format")?;

            let mut config = match share_mode {
                ShareMode::Shared => format_from_waveformatex_ptr(format_ptr.0, client)
                    .ok_or_else(|| {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Device audio format could not be mapped to a supported format",
                        )
                    })?,
                ShareMode::Exclusive => exclusive_default_format(client, format_ptr.0)?
                    .ok_or_else(|| {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Device supports no exclusive-mode format at its default channel \
                             count and sample rate",
                        )
                    })?,
            };

            if config.buffer_size == SupportedBufferSize::Unknown {
                if let Some(periods_hns) = device_periods_hns(client) {
                    config.buffer_size =
                        period_buffer_size(periods_hns, share_mode, config.sample_rate);
                }
            }
            Ok(config)
        }
    }

    pub(crate) fn data_flow(&self) -> Audio::EDataFlow {
        match &self.device {
            DeviceHandle::DefaultOutput => Audio::eRender,
            DeviceHandle::DefaultInput => Audio::eCapture,
            DeviceHandle::Specific(device) => {
                let endpoint = Endpoint::from(device.clone());
                endpoint.data_flow()
            }
        }
    }

    pub fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_input_config_for(ShareMode::Shared)
    }

    pub(crate) fn default_input_config_for(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedStreamConfig, Error> {
        if self.data_flow() == Audio::eCapture {
            self.default_format(share_mode)
        } else {
            Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Device does not support input",
            ))
        }
    }

    pub fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_output_config_for(ShareMode::Shared)
    }

    pub(crate) fn default_output_config_for(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedStreamConfig, Error> {
        let data_flow = self.data_flow();
        if data_flow == Audio::eRender {
            self.default_format(share_mode)
        } else {
            Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Device does not support output",
            ))
        }
    }

    /// `DeviceTrait::build_input_stream_raw` with an explicit share mode.
    pub(crate) fn build_input_stream_raw_for<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        share_mode: ShareMode,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        let stream_inner =
            self.build_input_stream_raw_inner(config, sample_format, timeout, share_mode)?;
        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let monitor = self.default_device_monitor()?;
        let stream = Stream::new_input(stream_inner, data_callback, error_callback, monitor)?;
        stream.signal_ready();
        Ok(stream)
    }

    /// `DeviceTrait::build_output_stream_raw` with an explicit share mode.
    pub(crate) fn build_output_stream_raw_for<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        share_mode: ShareMode,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        // Keep `playback` monotonic: an underrun can saturate `buffered` to zero, pulling
        // `playback` backward.
        let data_callback = crate::host::monotonic_output_callback(data_callback);
        let stream_inner =
            self.build_output_stream_raw_inner(config, sample_format, timeout, share_mode)?;
        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let monitor = self.default_device_monitor()?;
        let stream = Stream::new_output(stream_inner, data_callback, error_callback, monitor)?;
        stream.signal_ready();
        Ok(stream)
    }

    pub(crate) fn build_input_stream_raw_inner(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        activation_timeout: Option<Duration>,
        share_mode: ShareMode,
    ) -> Result<StreamInner, Error> {
        crate::validate_stream_config(&config)?;
        unsafe {
            // Making sure that COM is initialized.
            // It's not actually sure that this is required, but when in doubt do it.
            com::com_initialized();

            // Obtaining a `IAudioClient`.
            let audio_client = self
                .build_audioclient(activation_timeout)
                .context("Failed to build audio client")?;

            // Shared mode: no further range validation, IAudioClient::Initialize accepts any
            // positive duration. The callback period is always GetDevicePeriod() regardless of
            // what is requested here; the value only affects ring-buffer latency.
            //
            // Exclusive mode: this is also the periodicity, so zero is not a legal value and
            // `BufferSize::Default` resolves to the device's default period instead.
            let buffer_duration = buffer_duration_for(
                &audio_client,
                share_mode,
                &config.buffer_size,
                config.sample_rate,
            )?;

            let mut stream_flags = DEFAULT_FLAGS;

            if self.data_flow() == Audio::eRender {
                if share_mode == ShareMode::Exclusive {
                    // Loopback is a property of the shared-mode engine mixer, which exclusive
                    // mode bypasses. Initialize would fail with AUDCLNT_E_INVALID_STREAM_FLAG;
                    // say why instead.
                    return Err(Error::with_message(
                        ErrorKind::UnsupportedOperation,
                        "WASAPI exclusive mode does not support loopback capture from an output \
                         device",
                    ));
                }
                stream_flags |= Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;
            }

            // Computing the format and initializing the device.
            let (format_attempt, container_shift) =
                config_to_waveformatextensible(config, sample_format, share_mode).ok_or_else(
                    || {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Stream configuration could not be converted to a compatible format",
                        )
                    },
                )?;

            // Finally, initializing the audio client
            let audio_client = self.initialize_audio_client(
                audio_client,
                share_mode,
                stream_flags,
                buffer_duration,
                &format_attempt.Format,
                activation_timeout,
            )?;
            let waveformatex = format_attempt.Format;

            // obtaining the size of the samples buffer in number of frames
            let max_frames_in_buffer = audio_client
                .GetBufferSize()
                .context("Failed to get buffer size")?;

            let period_frames = stream_period_frames(
                &audio_client,
                share_mode,
                config.sample_rate,
                max_frames_in_buffer,
            );

            // Creating the event that will be signalled whenever we need to submit some samples.
            let event =
                Threading::CreateEventA(None, false, false, windows::core::PCSTR(ptr::null()))
                    .context("Failed to create event")?;

            audio_client
                .SetEventHandle(event)
                .context("Failed to set event handle")?;

            // Building a `IAudioCaptureClient` that will be used to read captured samples.
            let capture_client = audio_client
                .GetService::<Audio::IAudioCaptureClient>()
                .context("Failed to get capture client")?;

            // Once we built the `StreamInner`, we add a command that will be picked up by the
            // `run()` method and added to the `RunContext`.
            let client_flow = AudioClientFlow::Capture { capture_client };

            let audio_clock = audio_client
                .GetService::<Audio::IAudioClock>()
                .context("Failed to get audio clock")?;

            let stream_latency = {
                let hns = audio_client
                    .GetStreamLatency()
                    .context("Failed to get stream latency")?;
                Duration::from_nanos(hns.max(0) as u64 * 100)
            };

            // WASAPI lends the capture buffer to be read, so samples arriving left-justified are
            // shifted down into a staging buffer instead of in place. Sized once here, so the
            // callback allocates nothing, and left empty for formats needing no shift.
            //
            // `i32` rather than `u8` because it reaches the callback as a `Data`, whose
            // `as_slice` casts to the sample type, and a `Vec<u8>` guarantees no alignment.
            let capture_scratch = if container_shift == 0 {
                Vec::new()
            } else {
                let containers = max_frames_in_buffer as usize * waveformatex.nBlockAlign as usize
                    / mem::size_of::<i32>();
                vec![0i32; containers]
            };

            Ok(StreamInner {
                audio_client,
                audio_clock,
                client_flow,
                event,
                playing: false,
                max_frames_in_buffer,
                period_frames,
                bytes_per_frame: waveformatex.nBlockAlign,
                config,
                sample_format,
                share_mode,
                stream_latency,
                draining: Arc::new(AtomicBool::new(false)),
                fill_usec: Arc::new(AtomicU64::new(0)),
                container_shift,
                capture_scratch,
            })
        }
    }

    pub(crate) fn build_output_stream_raw_inner(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        activation_timeout: Option<Duration>,
        share_mode: ShareMode,
    ) -> Result<StreamInner, Error> {
        crate::validate_stream_config(&config)?;
        unsafe {
            // Making sure that COM is initialized.
            // It's not actually sure that this is required, but when in doubt do it.
            com::com_initialized();

            // Obtaining a `IAudioClient`.
            let audio_client = self
                .build_audioclient(activation_timeout)
                .context("Failed to build audio client")?;

            // See `build_input_stream_raw_inner` for why exclusive mode resolves
            // `BufferSize::Default` differently.
            let buffer_duration = buffer_duration_for(
                &audio_client,
                share_mode,
                &config.buffer_size,
                config.sample_rate,
            )?;

            // Shared-mode output asks the engine to resample and convert whatever the caller
            // hands over. Exclusive mode has no engine in the path: `Initialize` rejects both
            // flags outright, and the format must already be one the endpoint accepts.
            let stream_flags = match share_mode {
                ShareMode::Shared => {
                    DEFAULT_FLAGS
                        | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                        | Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                }
                ShareMode::Exclusive => DEFAULT_FLAGS,
            };

            // Computing the format and initializing the device.
            let (format_attempt, container_shift) =
                config_to_waveformatextensible(config, sample_format, share_mode).ok_or_else(
                    || {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Stream configuration could not be converted to a compatible format",
                        )
                    },
                )?;

            // Finally, initializing the audio client
            let audio_client = self.initialize_audio_client(
                audio_client,
                share_mode,
                stream_flags,
                buffer_duration,
                &format_attempt.Format,
                activation_timeout,
            )?;
            let waveformatex = format_attempt.Format;

            // Creating the event that will be signalled whenever we need to submit some samples.
            let event =
                Threading::CreateEventA(None, false, false, windows::core::PCSTR(ptr::null()))
                    .context("Failed to create event")?;

            audio_client
                .SetEventHandle(event)
                .context("Failed to set event handle")?;

            // obtaining the size of the samples buffer in number of frames
            let max_frames_in_buffer = audio_client
                .GetBufferSize()
                .context("Failed to get buffer size")?;

            let period_frames = stream_period_frames(
                &audio_client,
                share_mode,
                config.sample_rate,
                max_frames_in_buffer,
            );

            // Building a `IAudioRenderClient` that will be used to fill the samples buffer.
            let render_client = audio_client
                .GetService::<IAudioRenderClient>()
                .context("Failed to get render client")?;

            // Once we built the `StreamInner`, we add a command that will be picked up by the
            // `run()` method and added to the `RunContext`.
            let client_flow = AudioClientFlow::Render { render_client };

            let audio_clock = audio_client
                .GetService::<Audio::IAudioClock>()
                .context("Failed to get audio clock")?;

            let stream_latency = {
                let hns = audio_client
                    .GetStreamLatency()
                    .context("Failed to get stream latency")?;
                Duration::from_nanos(hns.max(0) as u64 * 100)
            };

            Ok(StreamInner {
                audio_client,
                audio_clock,
                client_flow,
                event,
                playing: false,
                max_frames_in_buffer,
                period_frames,
                bytes_per_frame: waveformatex.nBlockAlign,
                config,
                sample_format,
                share_mode,
                stream_latency,
                draining: Arc::new(AtomicBool::new(false)),
                fill_usec: Arc::new(AtomicU64::new(0)),
                container_shift,
                // Render writes into WASAPI's own buffer, which is the backend's to modify until
                // `ReleaseBuffer`, so the shift happens where the samples already are.
                capture_scratch: Vec::new(),
            })
        }
    }

    /// Calls `IAudioClient::Initialize`, handling the exclusive-mode buffer alignment retry.
    ///
    /// Returns the initialized client, which is *not* necessarily the one passed in: an
    /// exclusive-mode `Initialize` that fails with `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` leaves
    /// its client unusable, so the retry activates a fresh one. Everything else is passed
    /// straight through.
    ///
    /// # Safety
    ///
    /// `format` must point at a valid `WAVEFORMATEX` (with any trailing extension bytes it
    /// declares), and COM must be initialized on the calling thread.
    unsafe fn initialize_audio_client(
        &self,
        audio_client: Audio::IAudioClient,
        share_mode: ShareMode,
        stream_flags: u32,
        buffer_duration: i64,
        format: &Audio::WAVEFORMATEX,
        activation_timeout: Option<Duration>,
    ) -> Result<Audio::IAudioClient, Error> {
        // An event-driven exclusive-mode stream must be given a periodicity, and it must equal
        // the buffer duration. Shared mode requires zero.
        let periodicity = match share_mode {
            ShareMode::Shared => 0,
            ShareMode::Exclusive => buffer_duration,
        };
        let mode = to_winapi_share_mode(share_mode);

        let result = unsafe {
            audio_client.Initialize(
                mode,
                stream_flags,
                buffer_duration,
                periodicity,
                format,
                None,
            )
        };

        let err = match result {
            Ok(()) => return Ok(audio_client),
            Err(err) => err,
        };

        // Only exclusive, event-driven streams can hit this, and only they can recover from it.
        if share_mode != ShareMode::Exclusive
            || err.code() != Audio::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
        {
            return Err(Error::from(err)).context("Failed to initialize audio client");
        }

        // The failed client still reports the frame count the device would have accepted: the
        // next size up that satisfies the endpoint's alignment constraint.
        let aligned_frames =
            unsafe { audio_client.GetBufferSize() }.context("Failed to get aligned buffer size")?;
        if aligned_frames == 0 || format.nSamplesPerSec == 0 {
            return Err(Error::from(err)).context("Failed to initialize audio client");
        }

        // A client whose Initialize failed cannot be initialized again; release it and activate
        // a replacement before retrying with the aligned duration.
        drop(audio_client);
        let aligned_duration =
            buffer_size_to_duration(&BufferSize::Fixed(aligned_frames), format.nSamplesPerSec);
        let audio_client = self
            .build_audioclient(activation_timeout)
            .context("Failed to rebuild audio client for aligned buffer")?;
        unsafe {
            audio_client.Initialize(
                mode,
                stream_flags,
                aligned_duration,
                aligned_duration,
                format,
                None,
            )
        }
        .context("Failed to initialize audio client with an aligned buffer")?;
        Ok(audio_client)
    }
}

/// Compares the endpoint IDs of two `IMMDevice` objects.
///
/// # Safety
///
/// Both devices must be valid, live `IMMDevice` COM objects.
unsafe fn endpoint_ids_equal(a: &Audio::IMMDevice, b: &Audio::IMMDevice) -> bool {
    let id_a = unsafe { a.GetId() }.expect("cpal: GetId failure");
    let id_b = unsafe { b.GetId() }.expect("cpal: GetId failure");
    let _ga = ComString(id_a);
    let _gb = ComString(id_b);
    let mut off = 0isize;
    loop {
        let wa = unsafe { *id_a.0.offset(off) };
        let wb = unsafe { *id_b.0.offset(off) };
        if wa != wb {
            return false;
        }
        if wa == 0 {
            return true;
        }
        off += 1;
    }
}

/// Hashes the endpoint ID of an `IMMDevice` into `state` without allocating.
///
/// # Safety
/// `device` must be a valid, live `IMMDevice` COM object.
unsafe fn hash_endpoint_id<H: std::hash::Hasher>(device: &Audio::IMMDevice, state: &mut H) {
    let id = unsafe { device.GetId() }.expect("cpal: GetId failure");
    let _g = ComString(id);
    let mut off = 0isize;
    loop {
        let w = unsafe { *id.0.offset(off) };
        if w == 0 {
            break;
        }
        w.hash(state);
        off += 1;
    }
}

// Equality and hashing use stable identifiers only.
impl PartialEq for Device {
    fn eq(&self, other: &Device) -> bool {
        match (&self.device, &other.device) {
            (DeviceHandle::DefaultOutput, DeviceHandle::DefaultOutput)
            | (DeviceHandle::DefaultInput, DeviceHandle::DefaultInput) => true,
            (DeviceHandle::Specific(a), DeviceHandle::Specific(b)) => {
                // SAFETY: both IMMDevice handles are valid for the lifetime of their Device.
                unsafe { endpoint_ids_equal(a, b) }
            }
            _ => false,
        }
    }
}

impl Eq for Device {}

impl std::hash::Hash for Device {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.device {
            DeviceHandle::DefaultOutput | DeviceHandle::DefaultInput => {
                mem::discriminant(&self.device).hash(state);
            }
            DeviceHandle::Specific(device) => {
                // SAFETY: the IMMDevice handle is valid for the Device's lifetime.
                unsafe { hash_endpoint_id(device, state) }
            }
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let desc = self.description().map_err(|_| fmt::Error)?;
        f.write_str(desc.name())
    }
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Device")
            .field("device", &self.device)
            .field("description", &self.description())
            .finish()
    }
}

impl From<Audio::IMMDevice> for Endpoint {
    fn from(device: Audio::IMMDevice) -> Self {
        unsafe {
            let endpoint = immendpoint_from_immdevice(device);
            Endpoint { endpoint }
        }
    }
}

impl Endpoint {
    fn data_flow(&self) -> Audio::EDataFlow {
        unsafe { data_flow_from_immendpoint(&self.endpoint) }
    }
}

static ENUMERATOR: OnceLock<Result<Enumerator, windows::core::Error>> = OnceLock::new();

/// Returns the current default audio endpoint for `flow`, or `None` if none exists.
///
/// Shared by [`Device::immdevice`] and the stream-side `get_current_default` helper to
/// avoid duplicating the `GetDefaultAudioEndpoint` call.
pub(super) fn current_default_endpoint(flow: Audio::EDataFlow) -> Option<Audio::IMMDevice> {
    // Ensure COM is initialised on this thread — callers include notification callbacks on
    // threads that may not have called CoInitialize themselves.
    com::com_initialized();
    // SAFETY: `get_enumerator()` is a thread-safe singleton initialised at first use.
    unsafe {
        get_enumerator()
            .ok()?
            .0
            .GetDefaultAudioEndpoint(flow, Audio::eConsole)
            .ok()
    }
}

fn get_enumerator() -> Result<&'static Enumerator, windows::core::Error> {
    ENUMERATOR
        .get_or_init(|| {
            // COM initialization is thread local, but we only need to have COM initialized in the
            // thread we create the objects in
            com::com_initialized();

            // SAFETY: `MMDeviceEnumerator` is a well-known in-process COM class; the returned
            // interface pointer is only read through the safe `IMMDeviceEnumerator` wrapper.
            unsafe {
                Com::CoCreateInstance::<_, Audio::IMMDeviceEnumerator>(
                    &Audio::MMDeviceEnumerator,
                    None,
                    Com::CLSCTX_ALL,
                )
            }
            .map(Enumerator)
        })
        .as_ref()
        .map_err(Clone::clone)
}

// Helper function to query a DWORD property from a WASAPI device property store
unsafe fn get_property_u32(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<u32> {
    let mut property_value = unsafe { property_store.GetValue(property_key) }.ok()?;
    let prop_variant = unsafe { &property_value.Anonymous.Anonymous };

    // Check if it's a UI4 (unsigned 32-bit integer)
    if prop_variant.vt != VT_UI4 {
        return None;
    }

    let value = unsafe { *(&prop_variant.Anonymous as *const _ as *const u32) };

    // Clean up the property
    unsafe { StructuredStorage::PropVariantClear(&mut property_value) }.ok();

    Some(value)
}

// Helper function to query a string property from a WASAPI device property store
unsafe fn get_property_string(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<String> {
    let mut property_value = unsafe { property_store.GetValue(property_key) }.ok()?;
    let prop_variant = unsafe { &property_value.Anonymous.Anonymous };

    // Read the string from the union data field, expecting a *const u16.
    if prop_variant.vt != VT_LPWSTR {
        return None;
    }
    let ptr_utf16 = unsafe { *(&prop_variant.Anonymous as *const _ as *const *const u16) };

    // Find the length of the null-terminated string with a safety limit
    const MAX_STRING_LEN: usize = 32768; // 32K characters should be more than enough
    let mut len = 0;
    while len < MAX_STRING_LEN && unsafe { *ptr_utf16.add(len) } != 0 {
        len += 1;
    }

    // If we hit the limit, the string is likely malformed (not null-terminated)
    if len >= MAX_STRING_LEN {
        return None;
    }

    // Create the utf16 slice and convert it into a string.
    let string_slice = unsafe { slice::from_raw_parts(ptr_utf16, len) };
    let os_string: OsString = OsStringExt::from_wide(string_slice);
    let result = match os_string.into_string() {
        Ok(string) => Some(string),
        Err(os_string) => Some(os_string.to_string_lossy().into()),
    };

    // Clean up the property.
    unsafe { StructuredStorage::PropVariantClear(&mut property_value) }.ok();

    result
}

/// Send/Sync wrapper around `IMMDeviceEnumerator`.
struct Enumerator(Audio::IMMDeviceEnumerator);

unsafe impl Send for Enumerator {}
unsafe impl Sync for Enumerator {}

/// WASAPI implementation for `Devices`.
pub struct Devices {
    collection: Audio::IMMDeviceCollection,
    total_count: u32,
    next_item: u32,
}

impl Devices {
    pub fn new() -> Result<Self, Error> {
        unsafe {
            // can fail because of wrong parameters (should never happen) or out of memory
            let collection = get_enumerator()
                .context("Failed to get device enumerator")?
                .0
                .EnumAudioEndpoints(Audio::eAll, Audio::DEVICE_STATE_ACTIVE)
                .context("Failed to enumerate audio endpoints")?;

            let count = collection
                .GetCount()
                .context("Failed to get device count")?;

            Ok(Self {
                collection,
                total_count: count,
                next_item: 0,
            })
        }
    }
}

unsafe impl Send for Devices {}
unsafe impl Sync for Devices {}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_item >= self.total_count {
            return None;
        }

        unsafe {
            let device = self.collection.Item(self.next_item).unwrap();
            self.next_item += 1;
            Some(Self::Item::from_immdevice(device))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let num = self.total_count - self.next_item;
        let num = num as usize;
        (num, Some(num))
    }
}

pub fn default_input_device() -> Option<Device> {
    // Detect if a default input device exists before creating a `Device` for it.
    current_default_endpoint(Audio::eCapture).map(|_| Device::default_input())
}

pub fn default_output_device() -> Option<Device> {
    // Detect if a default output device exists before creating a `Device` for it.
    current_default_endpoint(Audio::eRender).map(|_| Device::default_output())
}

impl From<Audio::EDataFlow> for DeviceDirection {
    fn from(data_flow: Audio::EDataFlow) -> Self {
        if data_flow == Audio::eCapture {
            DeviceDirection::Input
        } else if data_flow == Audio::eRender {
            DeviceDirection::Output
        } else {
            DeviceDirection::Unknown
        }
    }
}

// Sample rate range supported by the Media Foundation Resampler MFT used by AUTOCONVERTPCM.
const OUTPUT_MIN_SAMPLE_RATE: SampleRate = 8_000;
const OUTPUT_MAX_SAMPLE_RATE: SampleRate = 384_000;

// Sample rate range probed in exclusive mode. Endpoint hardware runs between 8 kHz and the 768 kHz
// of the fastest PCM converters; the DSD rates above that in `COMMON_SAMPLE_RATES` have no PCM or
// IEEE-float encoding to ask `IsFormatSupported` about, so probing them only spends a driver
// round-trip per sample format to be refused.
const EXCLUSIVE_MIN_SAMPLE_RATE: SampleRate = 8_000;
const EXCLUSIVE_MAX_SAMPLE_RATE: SampleRate = 768_000;

// The longest buffer `IAudioClient::Initialize` accepts from an event-driven exclusive-mode
// client; longer is documented to fail with AUDCLNT_E_BUFFER_SIZE_ERROR.
const EXCLUSIVE_MAX_BUFFER_HNS: i64 = 5_000 * 10_000;

// Formats encodable as WAVEFORMATEXTENSIBLE. U8/I16 map to WAVE_FORMAT_PCM; the rest use
// WAVE_FORMAT_EXTENSIBLE. Unsigned formats wider than 8 bits are omitted: KSDATAFORMAT_SUBTYPE_PCM
// is always signed for 16-bit and wider, so submitting unsigned data would produce a DC offset.
const WAVEFORMATEXTENSIBLE_SAMPLE_FORMATS: [SampleFormat; 7] = [
    SampleFormat::U8,
    SampleFormat::I16,
    SampleFormat::I24,
    SampleFormat::I32,
    SampleFormat::I64,
    SampleFormat::F32,
    SampleFormat::F64,
];

// Standard speaker layouts, as documented for `KSAUDIO_CHANNEL_CONFIG`. The `windows` crate
// exports the individual `SPEAKER_*` bits but not these combinations, so they are spelled out.
//
// Four and six channels each have a second documented layout (`KSAUDIO_SPEAKER_SURROUND` 0x107 and
// `KSAUDIO_SPEAKER_5POINT1_SURROUND` 0x60F); the ones picked here are the two that nest inside the
// eight-channel layout below. Eight channels is deliberately not `KSAUDIO_SPEAKER_7POINT1` (0xFF),
// which the same page calls obsolete and no longer supported.
const KSAUDIO_SPEAKER_MONO: u32 = KernelStreaming::SPEAKER_FRONT_CENTER;
const KSAUDIO_SPEAKER_STEREO: u32 =
    KernelStreaming::SPEAKER_FRONT_LEFT | KernelStreaming::SPEAKER_FRONT_RIGHT;
const KSAUDIO_SPEAKER_QUAD: u32 = KSAUDIO_SPEAKER_STEREO
    | KernelStreaming::SPEAKER_BACK_LEFT
    | KernelStreaming::SPEAKER_BACK_RIGHT;
const KSAUDIO_SPEAKER_5POINT1: u32 = KSAUDIO_SPEAKER_QUAD
    | KernelStreaming::SPEAKER_FRONT_CENTER
    | KernelStreaming::SPEAKER_LOW_FREQUENCY;
const KSAUDIO_SPEAKER_7POINT1_SURROUND: u32 = KSAUDIO_SPEAKER_5POINT1
    | KernelStreaming::SPEAKER_SIDE_LEFT
    | KernelStreaming::SPEAKER_SIDE_RIGHT;

// The `dwChannelMask` to advertise for `share_mode`.
//
// Shared mode keeps `KSAUDIO_SPEAKER_DIRECTOUT` (0), which this backend has always sent: the
// audio engine accepts it and cpal does not care about speaker positions.
//
// Exclusive mode cannot keep it. The format goes to the driver rather than to the engine, and a
// driver may reject a zero mask with `AUDCLNT_E_UNSUPPORTED_FORMAT`, indistinguishable from "this
// endpoint has no exclusive mode at all". Measured on a PreSonus AudioBox 22VSL (USB, Windows 11):
// 24-bit-in-a-32-bit-container at 48 kHz stereo is refused with mask 0 and accepted with
// `SPEAKER_FRONT_LEFT|SPEAKER_FRONT_RIGHT`, every other field identical.
//
// Only documented layouts are emitted. Any other width falls back to `KSAUDIO_SPEAKER_DIRECTOUT`,
// documented as rendering the first channel to the first port on the device and so on, which is
// the truthful answer when there is no positional layout to name; inventing one risks a mask with
// reserved bits, which is refused for a reason that has nothing to do with the device.
//
// The same mask must be used when probing and when initializing, or a format that
// `IsFormatSupported` approved fails at `Initialize`. Both paths reach it through here.
//
// U8 and I16 go out as a plain `WAVE_FORMAT_PCM` header, whose `cbSize` of 0 stops the driver
// reading as far as `dwChannelMask`, so for those two the mask computed here never leaves the
// process. Spelling them `WAVE_FORMAT_EXTENSIBLE` instead would carry it, but Device Formats
// warns that a driver may accept one spelling and refuse the other, so that trade is not made
// blind.
fn channel_mask_for(share_mode: ShareMode, channels: u16) -> u32 {
    match share_mode {
        ShareMode::Shared => KernelStreaming::KSAUDIO_SPEAKER_DIRECTOUT,
        ShareMode::Exclusive => match channels {
            1 => KSAUDIO_SPEAKER_MONO,
            2 => KSAUDIO_SPEAKER_STEREO,
            4 => KSAUDIO_SPEAKER_QUAD,
            6 => KSAUDIO_SPEAKER_5POINT1,
            8 => KSAUDIO_SPEAKER_7POINT1_SURROUND,
            _ => KernelStreaming::KSAUDIO_SPEAKER_DIRECTOUT,
        },
    }
}

// Turns a `Format` into a `WAVEFORMATEXTENSIBLE`, paired with the shift its samples need to sit
// left-justified in the container it declares.
//
// Returns `None` if the WAVEFORMATEXTENSIBLE does not support the given format, or if the
// container it would ask for is padded in a way the backend cannot align.
fn config_to_waveformatextensible(
    config: StreamConfig,
    sample_format: SampleFormat,
    share_mode: ShareMode,
) -> Option<(Audio::WAVEFORMATEXTENSIBLE, u32)> {
    let format_tag = match sample_format {
        SampleFormat::U8 | SampleFormat::I16 => Audio::WAVE_FORMAT_PCM,

        SampleFormat::I24
        | SampleFormat::I32
        | SampleFormat::I64
        | SampleFormat::F32
        | SampleFormat::F64 => KernelStreaming::WAVE_FORMAT_EXTENSIBLE,

        _ => return None,
    };
    let channels = config.channels;
    let sample_rate = config.sample_rate;
    let sample_bytes = sample_format.sample_size() as u16;
    let avg_bytes_per_sec = u32::from(channels) * sample_rate * u32::from(sample_bytes);
    let block_align = channels * sample_bytes;
    // wBitsPerSample is the container word size; wValidBitsPerSample is the actual bit depth.
    // For I24 the container is 32 bits (sample_size() == 4) but only 24 bits are significant.
    let container_bits = 8 * sample_bytes;
    let valid_bits = sample_format.bits_per_sample() as u16;

    let cb_size = if format_tag == Audio::WAVE_FORMAT_PCM {
        0
    } else {
        let extensible_size = mem::size_of::<Audio::WAVEFORMATEXTENSIBLE>();
        let ex_size = mem::size_of::<Audio::WAVEFORMATEX>();
        (extensible_size - ex_size) as u16
    };

    let waveformatex = Audio::WAVEFORMATEX {
        wFormatTag: format_tag as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: container_bits,
        cbSize: cb_size,
    };

    let channel_mask = channel_mask_for(share_mode, channels);

    let sub_format = match sample_format {
        SampleFormat::U8
        | SampleFormat::I16
        | SampleFormat::I24
        | SampleFormat::I32
        | SampleFormat::I64 => KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,

        SampleFormat::F32 | SampleFormat::F64 => Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        _ => return None,
    };

    let waveformatextensible = Audio::WAVEFORMATEXTENSIBLE {
        Format: waveformatex,
        Samples: Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: channel_mask,
        SubFormat: sub_format,
    };

    let shift = container_shift(&waveformatextensible)?;

    Some((waveformatextensible, shift))
}

/// How far the negotiated format's samples must move up to sit left-justified in their container,
/// or `None` for a padded container the backend cannot align.
///
/// Read off the `WAVEFORMATEXTENSIBLE` handed to `Initialize` rather than off the `SampleFormat`,
/// so the answer comes from the format's own two bit counts and no format has to be named here.
fn container_shift(format: &Audio::WAVEFORMATEXTENSIBLE) -> Option<u32> {
    // A plain `WAVE_FORMAT_PCM` header carries no extension for the device to read, so its
    // `wValidBitsPerSample` means nothing and the container is full by definition.
    if format.Format.cbSize == 0 {
        return Some(0);
    }
    // SAFETY: `Samples` is a union of three `u16`s. `wValidBitsPerSample` is the member
    // `config_to_waveformatextensible` writes, and the one `WAVE_FORMAT_EXTENSIBLE` defines for
    // the PCM and IEEE-float subformats this backend emits.
    let valid_bits = unsafe { format.Samples.wValidBitsPerSample };
    container_align::padding_bits(format.Format.wBitsPerSample, valid_bits)
}

// Sample formats probed against the endpoint in exclusive mode, where `GetMixFormat` answers for
// the engine rather than for the device. I24 is included because 24-bit-in-a-32-bit-container is
// what many audio interfaces run natively.
//
// `WAVEFORMATEXTENSIBLE_SAMPLE_FORMATS` minus its 64-bit entries: endpoints expose 8- to 32-bit
// containers, cpal's own ranking treats 64-bit integers as accumulator types rather than stream
// formats, and every entry here costs a blocking driver round-trip per sample rate.
const EXCLUSIVE_SAMPLE_FORMATS: [SampleFormat; 5] = [
    SampleFormat::U8,
    SampleFormat::I16,
    SampleFormat::I24,
    SampleFormat::I32,
    SampleFormat::F32,
];

/// `EXCLUSIVE_SAMPLE_FORMATS`, most preferred first.
///
/// Ordered by `cmp_default_heuristics` rather than by hand, so the format `default_*_config_with`
/// settles on stays the one that ranking `supported_*_configs_with` would pick.
fn exclusive_sample_formats_by_preference() -> [SampleFormat; EXCLUSIVE_SAMPLE_FORMATS.len()] {
    fn ranked(sample_format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange {
            channels: 2,
            min_sample_rate: 48_000,
            max_sample_rate: 48_000,
            buffer_size: SupportedBufferSize::Unknown,
            sample_format,
        }
    }

    let mut formats = EXCLUSIVE_SAMPLE_FORMATS;
    formats.sort_unstable_by(|a, b| ranked(*b).cmp_default_heuristics(&ranked(*a)));
    formats
}

/// Finds the format the endpoint accepts in exclusive mode that cpal ranks highest, at the channel
/// count and sample rate of the mix format.
///
/// Returns `Ok(None)` when the device accepts none of them.
///
/// # Safety
///
/// `mix_format` must point at a valid `WAVEFORMATEX` obtained from `client`.
unsafe fn exclusive_default_format(
    client: &Audio::IAudioClient,
    mix_format: *const Audio::WAVEFORMATEX,
) -> Result<Option<SupportedStreamConfig>, Error> {
    // SAFETY: the caller guarantees `mix_format` points at a valid `WAVEFORMATEX`.
    let channels = unsafe { (*mix_format).nChannels };
    let sample_rate = unsafe { (*mix_format).nSamplesPerSec };

    for sample_format in exclusive_sample_formats_by_preference() {
        let Some((waveformat, _)) = config_to_waveformatextensible(
            StreamConfig {
                channels,
                sample_rate,
                buffer_size: BufferSize::Default,
            },
            sample_format,
            ShareMode::Exclusive,
        ) else {
            continue;
        };
        // Derived from the whole struct, not from `.Format`: `format_from_waveformatex_ptr` reads
        // the extension bytes back through this pointer, which a borrow of the 18-byte header
        // does not cover.
        let format_ptr = &waveformat as *const _ as *const Audio::WAVEFORMATEX;
        // SAFETY: `format_ptr` points at the `WAVEFORMATEXTENSIBLE` just built, which outlives
        // both calls, and `client` is the endpoint's own audio client.
        if unsafe { is_format_supported(client, ShareMode::Exclusive, format_ptr) }? {
            return Ok(unsafe { format_from_waveformatex_ptr(format_ptr, client) });
        }
    }

    Ok(None)
}

/// The endpoint's default and minimum device periods, in 100-nanosecond units.
fn device_periods_hns(audio_client: &Audio::IAudioClient) -> Option<(i64, i64)> {
    let mut default_period = 0i64;
    let mut minimum_period = 0i64;
    unsafe { audio_client.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period)) }
        .is_ok()
        .then_some((default_period, minimum_period))
}

/// The buffer sizes to advertise at `sample_rate` when the endpoint reports no hardware limits.
///
/// A shared-mode client is scheduled at the engine's default period and does not choose its own
/// size. An exclusive-mode client does, anywhere from the device's minimum period up to the
/// ceiling `Initialize` documents — a bound on the request, not a promise the endpoint has the
/// memory for it, which nothing short of calling `Initialize` will tell.
fn period_buffer_size(
    periods_hns: (i64, i64),
    share_mode: ShareMode,
    sample_rate: SampleRate,
) -> SupportedBufferSize {
    let (default_period, minimum_period) = periods_hns;
    match share_mode {
        ShareMode::Shared if default_period > 0 => {
            let frames = buffer_duration_to_frames(default_period, sample_rate);
            SupportedBufferSize::Range {
                min: frames,
                max: frames,
            }
        }
        ShareMode::Exclusive if minimum_period > 0 => SupportedBufferSize::Range {
            min: buffer_duration_to_frames(minimum_period, sample_rate),
            max: buffer_duration_to_frames(EXCLUSIVE_MAX_BUFFER_HNS, sample_rate),
        },
        _ => SupportedBufferSize::Unknown,
    }
}

/// The buffer duration, in 100-nanosecond units, to request from `IAudioClient::Initialize`.
///
/// In shared mode `BufferSize::Default` becomes 0, asking the engine for its default period.
/// Exclusive mode cannot use 0, since the same value is also the periodicity, so it resolves to
/// the device's default period; the minimum period is reachable through `BufferSize::Fixed`.
fn buffer_duration_for(
    audio_client: &Audio::IAudioClient,
    share_mode: ShareMode,
    buffer_size: &BufferSize,
    sample_rate: SampleRate,
) -> Result<i64, Error> {
    match (share_mode, buffer_size) {
        (ShareMode::Shared, _) | (ShareMode::Exclusive, BufferSize::Fixed(_)) => {
            Ok(buffer_size_to_duration(buffer_size, sample_rate))
        }
        (ShareMode::Exclusive, BufferSize::Default) => device_periods_hns(audio_client)
            .map(|(default_period, _)| default_period)
            .filter(|&period| period > 0)
            .ok_or_else(|| {
                Error::with_message(
                    ErrorKind::BackendError,
                    "Failed to get the device's default period",
                )
            }),
    }
}

/// Get the callback size in frames for a stream in `share_mode`.
fn stream_period_frames(
    audio_client: &Audio::IAudioClient,
    share_mode: ShareMode,
    sample_rate: SampleRate,
    max_frames_in_buffer: FrameCount,
) -> FrameCount {
    match share_mode {
        ShareMode::Shared => {
            shared_mode_period_frames(audio_client, sample_rate, max_frames_in_buffer)
        }
        // An event-driven exclusive-mode stream is handed the whole buffer on every event, so
        // the buffer size *is* the callback size — which is not the same as the period that was
        // requested whenever the endpoint rounded the request up to an aligned one.
        ShareMode::Exclusive => max_frames_in_buffer,
    }
}

/// Get the default device period in frames for a shared-mode stream.
fn shared_mode_period_frames(
    audio_client: &Audio::IAudioClient,
    sample_rate: SampleRate,
    max_frames_in_buffer: FrameCount,
) -> FrameCount {
    let mut default_period = 0i64;
    if unsafe { audio_client.GetDevicePeriod(Some(&mut default_period), None) }.is_ok()
        && default_period > 0
    {
        buffer_duration_to_frames(default_period, sample_rate)
    } else {
        max_frames_in_buffer
    }
}

fn buffer_size_to_duration(buffer_size: &BufferSize, sample_rate: SampleRate) -> i64 {
    match buffer_size {
        // Round: a frame count and its 100ns duration are not exact multiples, so truncating here
        // and again on the way back drops common sizes (512, 1024, ...) by one frame.
        BufferSize::Fixed(frames) => {
            let rate = sample_rate as i64;
            (*frames as i64 * (1_000_000_000 / 100) + rate / 2) / rate
        }
        BufferSize::Default => 0,
    }
}

fn buffer_duration_to_frames(buffer_duration: i64, sample_rate: SampleRate) -> FrameCount {
    ((buffer_duration * sample_rate as i64 * 100 + 500_000_000) / 1_000_000_000) as FrameCount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_shift_for(sample_format: SampleFormat) -> u32 {
        let config = StreamConfig {
            channels: 2,
            sample_rate: 48_000,
            buffer_size: BufferSize::Default,
        };
        config_to_waveformatextensible(config, sample_format, ShareMode::Shared)
            .expect("a format the backend encodes")
            .1
    }

    #[test]
    fn only_a_padded_container_is_shifted() {
        // I24 is the one format CPAL carries in a container wider than the sample: 24 valid bits
        // in four bytes.
        assert_eq!(container_shift_for(SampleFormat::I24), 8);

        for sample_format in [
            SampleFormat::U8,
            SampleFormat::I16,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::F32,
            SampleFormat::F64,
        ] {
            assert_eq!(container_shift_for(sample_format), 0, "{sample_format}");
        }
    }

    // Every bit `WAVEFORMATEXTENSIBLE` defines, SPEAKER_FRONT_LEFT through SPEAKER_TOP_BACK_RIGHT.
    // Anything outside is a channel location the documentation calls reserved.
    const DEFINED_SPEAKER_POSITIONS: u32 = 0x3_FFFF;

    #[test]
    fn shared_mode_always_asks_for_directout() {
        for channels in 0..=u16::MAX {
            assert_eq!(
                channel_mask_for(ShareMode::Shared, channels),
                0,
                "{channels}"
            );
        }
    }

    #[test]
    fn exclusive_mode_uses_the_documented_layouts() {
        for (channels, mask) in [
            (1, 0x4),   // KSAUDIO_SPEAKER_MONO
            (2, 0x3),   // KSAUDIO_SPEAKER_STEREO
            (4, 0x33),  // KSAUDIO_SPEAKER_QUAD
            (6, 0x3F),  // KSAUDIO_SPEAKER_5POINT1
            (8, 0x63F), // KSAUDIO_SPEAKER_7POINT1_SURROUND
        ] {
            assert_eq!(
                channel_mask_for(ShareMode::Exclusive, channels),
                mask,
                "{channels}"
            );
        }
    }

    #[test]
    fn exclusive_mode_falls_back_to_directout_for_undocumented_widths() {
        for channels in [0, 3, 5, 7, 9, 18, 19, 31, 32, 33, u16::MAX] {
            assert_eq!(
                channel_mask_for(ShareMode::Exclusive, channels),
                0,
                "{channels}"
            );
        }
    }

    #[test]
    fn exclusive_mode_masks_are_positional_and_never_reserved() {
        for channels in 0..=u16::MAX {
            let mask = channel_mask_for(ShareMode::Exclusive, channels);
            assert_eq!(mask & !DEFINED_SPEAKER_POSITIONS, 0, "{channels}");
            // A non-zero mask must name exactly as many positions as there are channels, since
            // the channels are interleaved in the order the set bits appear.
            if mask != 0 {
                assert_eq!(mask.count_ones(), u32::from(channels), "{channels}");
            }
        }
    }
}
