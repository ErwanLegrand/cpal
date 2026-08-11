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
//! The trait is implemented both for the WASAPI backend's own `Device` and for the
//! platform-dispatch [`crate::platform::Device`]; on the latter, a device that is not a WASAPI
//! device fails with [`ErrorKind::UnsupportedOperation`].
//!
//! [`StreamConfig`]: crate::StreamConfig
//! [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation

use std::{time::Duration, vec::IntoIter};

use crate::{
    Data, Error, ErrorKind, InputCallbackInfo, OutputCallbackInfo, SampleFormat, SizedSample,
    StreamConfig, SupportedStreamConfig, SupportedStreamConfigRange,
};

/// How a WASAPI stream shares its endpoint with the rest of the system.
///
/// See the [module documentation](self) for what the two modes mean in practice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShareMode {
    /// The Windows audio engine mixes this stream with other applications'. The default.
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

/// WASAPI-specific counterparts to [`DeviceTrait`](crate::traits::DeviceTrait)'s configuration
/// and stream-building methods, each taking a [`WasapiStreamOptions`].
///
/// Passing [`WasapiStreamOptions::default()`] to any of these is equivalent to calling the
/// cross-platform method it mirrors.
///
/// The supported-format set, the default format, and the buffer size a device reports all differ
/// between the two share modes, so query them with the same options the stream will be built
/// with.
pub trait WasapiDeviceExt {
    /// The stream type produced by this device's builders.
    type Stream;

    /// The default input stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`](crate::traits::DeviceTrait::default_input_config).
    /// In exclusive mode, [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig)
    /// if the device accepts none of the sample formats CPAL probes for.
    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The default output stream configuration for the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`](crate::traits::DeviceTrait::default_output_config).
    /// In exclusive mode, [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig)
    /// if the device accepts none of the sample formats CPAL probes for.
    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error>;

    /// The input stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_input_configs`](crate::traits::DeviceTrait::supported_input_configs).
    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error>;

    /// The output stream configurations supported by the device under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`](crate::traits::DeviceTrait::supported_output_configs).
    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error>;

    /// Create a dynamically typed input stream under `options`.
    ///
    /// # Errors
    ///
    /// As [`DeviceTrait::build_input_stream_raw`](crate::traits::DeviceTrait::build_input_stream_raw).
    /// Exclusive mode additionally reports
    /// [`ErrorKind::DeviceBusy`](crate::ErrorKind::DeviceBusy) when another application already
    /// holds the endpoint, and
    /// [`ErrorKind::UnsupportedConfig`](crate::ErrorKind::UnsupportedConfig) when the device does
    /// not accept `config`/`sample_format` natively — there is no engine to convert for it.
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
    /// As [`DeviceTrait::build_output_stream_raw`](crate::traits::DeviceTrait::build_output_stream_raw),
    /// with the same exclusive-mode additions as
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

impl WasapiDeviceExt for super::Device {
    type Stream = super::Stream;

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        super::Device::default_input_config_for(self, options.share_mode)
    }

    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        super::Device::default_output_config_for(self, options.share_mode)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        super::Device::supported_input_configs_for(self, options.share_mode)
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        super::Device::supported_output_configs_for(self, options.share_mode)
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
        super::Device::build_input_stream_raw_for(
            self,
            config,
            sample_format,
            options.share_mode,
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
        super::Device::build_output_stream_raw_for(
            self,
            config,
            sample_format,
            options.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }
}

/// The error reported when a WASAPI-specific method is called on a device from another host.
fn not_a_wasapi_device() -> Error {
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        "Device does not belong to the WASAPI host",
    )
}

/// Unwraps the platform-dispatch `Device` into the WASAPI backend's own.
fn as_wasapi(device: &crate::platform::Device) -> Result<&super::Device, Error> {
    #[allow(unreachable_patterns)]
    match device.as_inner() {
        crate::platform::DeviceInner::Wasapi(device) => Ok(device),
        _ => Err(not_a_wasapi_device()),
    }
}

impl WasapiDeviceExt for crate::platform::Device {
    type Stream = crate::platform::Stream;

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        as_wasapi(self)?.default_input_config_with(options)
    }

    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        as_wasapi(self)?.default_output_config_with(options)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        as_wasapi(self)?.supported_input_configs_with(options)
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<IntoIter<SupportedStreamConfigRange>, Error> {
        as_wasapi(self)?.supported_output_configs_with(options)
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
        as_wasapi(self)?
            .build_input_stream_raw_with(
                config,
                sample_format,
                options,
                data_callback,
                error_callback,
                timeout,
            )
            .map(Into::into)
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
        as_wasapi(self)?
            .build_output_stream_raw_with(
                config,
                sample_format,
                options,
                data_callback,
                error_callback,
                timeout,
            )
            .map(Into::into)
    }
}
