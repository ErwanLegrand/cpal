//! WASAPI-specific extensions to the cross-platform API.
//!
//! WASAPI can open an endpoint in one of two share modes. Everything CPAL does by default uses
//! *shared* mode, where the Windows audio engine mixes the stream with every other application's
//! and may resample or convert it on the way. *Exclusive* mode hands the endpoint to a single
//! client: no mixing, no format conversion, and typically a much smaller device period, at the
//! cost of the device becoming unavailable to everything else while the stream lives.
//!
//! Share mode is not part of [`StreamConfig`], because it is meaningless on every other backend.
//! It is requested through [`WasapiStreamOptions`], passed to the `*_with` methods of
//! [`WasapiDeviceExt`]:
//!
//! ```no_run
//! use cpal::platform::{ShareMode, WasapiDeviceExt, WasapiStreamOptions};
//! use cpal::traits::HostTrait;
//!
//! let device = cpal::default_host().default_output_device().unwrap();
//! let options = WasapiStreamOptions::default().with_share_mode(ShareMode::Exclusive);
//!
//! // Exclusive mode exposes a different set of formats, so negotiate with the same options
//! // the stream will be built with.
//! let config = device.default_output_config_with(options)?;
//! let stream = device.build_output_stream_with::<f32, _, _>(
//!     config.config(),
//!     options,
//!     move |data, _| data.fill(0.0),
//!     |err| eprintln!("{err}"),
//!     None,
//! )?;
//! # Ok::<(), cpal::Error>(())
//! ```
//!
//! # These names exist on every platform
//!
//! Deliberately: a caller writing cross-platform code should not need a conditional-compilation
//! attribute to say "use exclusive mode where this platform has it". The types and the trait are
//! compiled everywhere and [`WasapiDeviceExt`] is implemented for [`crate::Device`] everywhere;
//! only the WASAPI implementation behind them is Windows-only.
//!
//! When there is no WASAPI endpoint behind the device — a non-Windows build, or a Windows build
//! using another host such as ASIO or JACK — the methods behave as follows:
//!
//! - The configuration queries answer exactly as their [`DeviceTrait`] counterparts do. Share
//!   mode does not apply to such a device, so it is ignored rather than treated as an error.
//! - The stream builders accept [`ShareMode::Shared`], since that is what an ordinary stream
//!   already is, and reject [`ShareMode::Exclusive`] with
//!   [`ErrorKind::UnsupportedOperation`](crate::ErrorKind::UnsupportedOperation). A request for
//!   exclusive mode is never quietly downgraded to a shared stream; whether to fall back is the
//!   caller's decision to make.
//!
//! [`StreamConfig`]: crate::StreamConfig
//! [`DeviceTrait`]: crate::traits::DeviceTrait

use std::{time::Duration, vec::IntoIter};

use crate::{
    traits::DeviceTrait, Data, Error, ErrorKind, InputCallbackInfo, OutputCallbackInfo,
    SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig, SupportedStreamConfigRange,
};

/// How a WASAPI stream shares its endpoint with the rest of the system.
///
/// See the [module documentation](self) for what the two modes mean in practice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShareMode {
    /// The Windows audio engine mixes this stream with other applications'. The default, and the
    /// only mode any other backend has.
    #[default]
    Shared,
    /// The stream owns the endpoint outright, bypassing the engine's mixer and format
    /// conversion. Only one exclusive-mode stream can exist per endpoint, and the user must
    /// have left "Allow applications to take exclusive control of this device" enabled.
    Exclusive,
}

/// WASAPI-specific options for configuration queries and stream building.
///
/// Marked `#[non_exhaustive]`: construct it with [`WasapiStreamOptions::default`] (or
/// [`shared`](Self::shared)/[`exclusive`](Self::exclusive)) and adjust from there, so that
/// later additions do not break callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WasapiStreamOptions {
    /// The share mode to open the endpoint in. Defaults to [`ShareMode::Shared`], which is
    /// exactly what the cross-platform API does.
    pub share_mode: ShareMode,
}

impl WasapiStreamOptions {
    /// Options selecting shared mode — identical to `WasapiStreamOptions::default()`.
    pub fn shared() -> Self {
        Self {
            share_mode: ShareMode::Shared,
        }
    }

    /// Options selecting exclusive mode.
    pub fn exclusive() -> Self {
        Self {
            share_mode: ShareMode::Exclusive,
        }
    }

    /// Returns these options with `share_mode` replaced.
    pub fn with_share_mode(mut self, share_mode: ShareMode) -> Self {
        self.share_mode = share_mode;
        self
    }
}

/// WASAPI-specific counterparts to [`DeviceTrait`]'s configuration and stream-building methods,
/// each taking a [`WasapiStreamOptions`].
///
/// Passing [`WasapiStreamOptions::default()`] to any of these is equivalent to calling the
/// cross-platform method it mirrors, on every platform.
///
/// The supported-format set, the default format, and the buffer size a device reports all differ
/// between the two share modes, so query them with the same options the stream will be built
/// with.
///
/// See the [module documentation](self) for what these methods do when the device has no WASAPI
/// endpoint behind it.
pub trait WasapiDeviceExt {
    /// The stream type produced by this device's builders.
    type Stream;

    /// The default input stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`]. In exclusive mode,
    /// [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig) if the device
    /// accepts none of the sample formats CPAL probes for.
    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The default output stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`]. In exclusive mode,
    /// [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig) if the device
    /// accepts none of the sample formats CPAL probes for.
    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The input stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_input_configs`].
    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error>;

    /// The output stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`].
    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error>;

    /// Create a dynamically typed input stream under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::build_input_stream_raw`]. Exclusive mode additionally reports
    /// [`ErrorKind::DeviceBusy`](crate::ErrorKind::DeviceBusy) when another application already
    /// holds the endpoint, and
    /// [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig) when the device does
    /// not accept `config`/`sample_format` natively — there is no engine to convert for it.
    /// Requesting it of a device with no WASAPI endpoint reports
    /// [`ErrorKind::UnsupportedOperation`](crate::ErrorKind::UnsupportedOperation).
    fn build_input_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static;

    /// Create a dynamically typed output stream under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::build_output_stream_raw`], with the same additions as
    /// [`build_input_stream_raw_with`](Self::build_input_stream_raw_with).
    fn build_output_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static;

    /// Create an input stream of sample type `T` under `options`.
    ///
    /// # Errors
    ///
    /// As [`build_input_stream_raw_with`](Self::build_input_stream_raw_with).
    fn build_input_stream_with<T, D, E>(
        &self,
        config: StreamConfig,
        options: WasapiStreamOptions,
        mut data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        T: SizedSample,
        D: FnMut(&[T], &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_input_stream_raw_with(
            config,
            T::FORMAT,
            options,
            move |data, info| {
                data_callback(
                    data.as_slice()
                        .expect("host supplied incorrect sample type"),
                    info,
                )
            },
            error_callback,
            timeout,
        )
    }

    /// Create an output stream of sample type `T` under `options`.
    ///
    /// # Errors
    ///
    /// As [`build_output_stream_raw_with`](Self::build_output_stream_raw_with).
    fn build_output_stream_with<T, D, E>(
        &self,
        config: StreamConfig,
        options: WasapiStreamOptions,
        mut data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        T: SizedSample,
        D: FnMut(&mut [T], &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_output_stream_raw_with(
            config,
            T::FORMAT,
            options,
            move |data, info| {
                data_callback(
                    data.as_slice_mut()
                        .expect("host supplied incorrect sample type"),
                    info,
                )
            },
            error_callback,
            timeout,
        )
    }
}

/// The error reported when exclusive mode is asked of a device with no WASAPI endpoint.
fn exclusive_unsupported() -> Error {
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        "Exclusive mode requires a WASAPI device",
    )
}

/// The WASAPI backend's own device behind a platform-dispatch one, if that is what it is.
#[cfg(windows)]
fn wasapi_device(device: &super::Device) -> Option<&crate::host::wasapi::Device> {
    #[allow(unreachable_patterns)]
    match device.as_inner() {
        super::DeviceInner::Wasapi(device) => Some(device),
        _ => None,
    }
}

impl WasapiDeviceExt for super::Device {
    type Stream = super::Stream;

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_input_config_with(options);
        }
        // Not a WASAPI endpoint: it has exactly one mode, so answer for that one.
        let _ = options;
        DeviceTrait::default_input_config(self)
    }

    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_output_config_with(options);
        }
        let _ = options;
        DeviceTrait::default_output_config(self)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.supported_input_configs_with(options);
        }
        let _ = options;
        // Every backend's own iterator is already a `Vec` walk; collecting keeps this trait's
        // return type the same one the WASAPI backend hands back.
        Ok(DeviceTrait::supported_input_configs(self)?
            .collect::<Vec<_>>()
            .into_iter())
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.supported_output_configs_with(options);
        }
        let _ = options;
        Ok(DeviceTrait::supported_output_configs(self)?
            .collect::<Vec<_>>()
            .into_iter())
    }

    fn build_input_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .build_input_stream_raw_with(
                    config,
                    sample_format,
                    options,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        // A shared-mode request is what an ordinary stream already is; an exclusive-mode one is
        // refused rather than quietly downgraded.
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
        DeviceTrait::build_input_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }

    fn build_output_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .build_output_stream_raw_with(
                    config,
                    sample_format,
                    options,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
        DeviceTrait::build_output_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }
}
