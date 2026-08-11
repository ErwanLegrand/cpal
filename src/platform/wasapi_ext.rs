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
//! // Exclusive mode exposes a different set of formats, so negotiate with the same options the
//! // stream will be built with, and build in the sample format that came back: there is no engine
//! // to convert from any other.
//! let config = device.default_output_config_with(options)?;
//! let stream = device.build_output_stream_raw_with(
//!     config.config(),
//!     config.sample_format(),
//!     options,
//!     move |data, _| data.bytes_mut().fill(0),
//!     |err| eprintln!("{err}"),
//!     None,
//! )?;
//! # Ok::<(), cpal::Error>(())
//! ```
//!
//! # These names exist on every platform
//!
//! The types and the trait are compiled everywhere and [`WasapiDeviceExt`] is implemented for
//! [`crate::Device`] everywhere, so saying "use exclusive mode where this platform has it" costs
//! the caller no conditional-compilation attribute. Only the WASAPI implementation behind them is
//! Windows-only.
//!
//! With no WASAPI endpoint behind the device — a non-Windows build, or a Windows build using
//! another host such as ASIO or JACK — [`ShareMode::Shared`] behaves as the [`DeviceTrait`]
//! counterpart does, and [`ShareMode::Exclusive`] fails with
//! [`ErrorKind::UnsupportedOperation`]. That applies to the configuration queries as much as to
//! the stream builders: the queries are how a caller asks whether exclusive mode is available, so
//! answering them for shared mode would read as a yes. A request for exclusive mode is never
//! quietly downgraded.
//!
//! [`StreamConfig`]: crate::StreamConfig
//! [`DeviceTrait`]: crate::traits::DeviceTrait
//! [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation

use std::time::Duration;

use crate::{
    CallbackInfo, Data, Error, ErrorKind, SampleFormat, SizedSample, StreamConfig,
    SupportedStreamConfig, SupportedStreamConfigRange, traits::DeviceTrait,
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
    /// The iterator of supported input configurations produced by this device's queries.
    type SupportedInputConfigs: Iterator<Item = SupportedStreamConfigRange>;

    /// The iterator of supported output configurations produced by this device's queries.
    type SupportedOutputConfigs: Iterator<Item = SupportedStreamConfigRange>;

    /// The stream type produced by this device's builders.
    type Stream;

    /// The default input stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`], and additionally:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    ///   sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The default output stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`], and additionally:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    ///   sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The input stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_input_configs`], and additionally:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a
    ///   CPAL configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedInputConfigs, Error>;

    /// The output stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`], and additionally:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a
    ///   CPAL configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedOutputConfigs, Error>;

    /// Create a dynamically typed input stream under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::build_input_stream_raw`]. Exclusive mode adds no error kind to that set;
    /// three of them just arise in further circumstances:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint, or if this is exclusive-mode loopback capture from an output
    ///   device: loopback taps the engine mixer, which exclusive mode bypasses.
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::DeviceBusy`]: crate::ErrorKind::DeviceBusy
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
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
        D: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static;

    /// Create a dynamically typed output stream under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::build_output_stream_raw`]. Exclusive mode adds no error kind to that set;
    /// three of them just arise in further circumstances:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` ask for exclusive mode and the device
    ///   has no WASAPI endpoint.
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::DeviceBusy`]: crate::ErrorKind::DeviceBusy
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
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
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
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
        D: FnMut(&[T], &CallbackInfo) + Send + 'static,
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
        D: FnMut(&mut [T], &CallbackInfo) + Send + 'static,
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
    type SupportedInputConfigs = super::SupportedInputConfigs;
    type SupportedOutputConfigs = super::SupportedOutputConfigs;
    type Stream = super::Stream;

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_input_config_with(options);
        }
        // Not a WASAPI endpoint, so exclusive mode is refused rather than answered for shared.
        // See the module documentation.
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
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
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
        DeviceTrait::default_output_config(self)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedInputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .supported_input_configs_with(options)
                .map(super::SupportedInputConfigs::from_wasapi);
        }
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
        DeviceTrait::supported_input_configs(self)
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedOutputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .supported_output_configs_with(options)
                .map(super::SupportedOutputConfigs::from_wasapi);
        }
        if options.share_mode != ShareMode::Shared {
            return Err(exclusive_unsupported());
        }
        DeviceTrait::supported_output_configs(self)
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
        D: FnMut(&Data, &CallbackInfo) + Send + 'static,
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
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
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
