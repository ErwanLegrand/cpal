//! WASAPI-specific extensions to the cross-platform API.
//!
//! WASAPI can open an endpoint in one of two share modes. Everything CPAL does by default uses
//! *shared* mode, where the Windows audio engine mixes the stream with every other application's
//! and may resample or convert it on the way. *Exclusive* mode hands the endpoint to a single
//! client: no mixing, no format conversion, and typically a much smaller device period, at the
//! cost of the device becoming unavailable to everything else while the stream lives.
//!
//! Share mode is not part of [`StreamConfig`], because it is meaningless on every other backend;
//! instead it is bound to a device once through [`WasapiDeviceExt::with_options`]. The resulting
//! [`WasapiConfigured`] implements [`DeviceTrait`] itself, so the ordinary queries and builders
//! on it answer for the chosen mode rather than repeating it at every call.
//!
//! ```no_run
//! use cpal::platform::wasapi_ext::{ShareMode, WasapiDeviceExt};
//! use cpal::traits::{DeviceTrait, HostTrait};
//!
//! let device = cpal::default_host().default_output_device().unwrap();
//! let exclusive = device.with_options(ShareMode::Exclusive)?;
//!
//! // Exclusive mode exposes a different set of formats, so negotiate and build on the same
//! // configured device, in the sample format that came back.
//! let config = exclusive.default_output_config()?;
//! let stream = exclusive.build_output_stream_raw(
//!     config.config(),
//!     config.sample_format(),
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
//! counterpart does, and anything else fails with [`ErrorKind::UnsupportedOperation`]. That
//! refusal happens in [`with_options`](WasapiDeviceExt::with_options), before there is a
//! configured device to ask anything of: the configuration queries are how a caller asks whether
//! exclusive mode is available, so answering them for shared mode would read as a yes. A request
//! for exclusive mode is never quietly downgraded.
//!
//! [`StreamConfig`]: crate::StreamConfig
//! [`DeviceTrait`]: crate::traits::DeviceTrait
//! [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation

use std::{fmt, time::Duration};

use crate::{
    CallbackInfo, Data, DeviceDescription, DeviceId, DuplexCallbackInfo, DuplexStreamConfig, Error,
    ErrorKind, SampleFormat, StreamConfig, SupportedStreamConfig, traits::DeviceTrait,
};

// Re-exported so the historical `cpal::platform::wasapi_ext::ShareMode` path keeps working; the
// type itself lives at the crate root, outside this platform module.
pub use crate::ShareMode;

/// Binds a share mode to a device, for the WASAPI features the cross-platform API has no
/// vocabulary for.
///
/// This trait is sealed: it is implemented for [`crate::Device`] on every platform, and cannot
/// be implemented outside this crate.
pub trait WasapiDeviceExt: sealed::Sealed + Sized {
    /// Binds `share_mode` to this device, returning a [`WasapiConfigured`] whose [`DeviceTrait`]
    /// methods answer for the mode.
    ///
    /// The supported-format set, the default format and the buffer size a device reports all
    /// differ between the two share modes, so this is deliberately the only way to reach the
    /// exclusive-mode ones: negotiating and building happen on one value, carrying one mode.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `share_mode` is not [`ShareMode::Shared`] and
    ///   this device has no WASAPI endpoint behind it. Shared mode is accepted by every device
    ///   on every platform.
    ///
    /// This is the only place that refusal can happen; a `WasapiConfigured` that exists can be
    /// asked for the mode it carries.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    fn with_options(&self, share_mode: ShareMode) -> Result<WasapiConfigured<'_, Self>, Error> {
        if share_mode != ShareMode::Shared && !self.has_wasapi_endpoint() {
            return Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Exclusive mode requires a WASAPI device",
            ));
        }
        Ok(WasapiConfigured {
            device: self,
            share_mode,
        })
    }
}

/// A device with a share mode bound to it, from
/// [`with_options`](WasapiDeviceExt::with_options).
///
/// It implements [`DeviceTrait`], so the configuration queries and stream builders are the
/// cross-platform ones, answering for the mode it carries. With [`ShareMode::Shared`] it behaves
/// exactly as the device it borrows, on every platform.
///
/// # Configurations still carry no share mode
///
/// A [`SupportedStreamConfig`] negotiated here records nothing about the mode it came from, so
/// passing one to the *bare* device's builder still opens shared mode — where, on output, the
/// engine converts the format rather than reporting anything. Nothing prevents that; what the
/// wrapper changes is which call is the natural one to write, since the value the config was
/// negotiated on is also the value that builds the stream. Reaching back to the device is a
/// detour rather than the default.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WasapiConfigured<'a, D> {
    device: &'a D,
    share_mode: ShareMode,
}

impl<D> WasapiConfigured<'_, D> {
    /// Whether this device is configured with the mode every device honours, in which case every
    /// method forwards to the bare device unchanged.
    fn is_default(&self) -> bool {
        self.share_mode == ShareMode::Shared
    }
}

impl<D> Clone for WasapiConfigured<'_, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for WasapiConfigured<'_, D> {}

/// The device's own name, unchanged: this is the same endpoint, and [`DeviceTrait`] documents
/// `to_string()` as the way to get a device's name.
impl<D: fmt::Display> fmt::Display for WasapiConfigured<'_, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.device, f)
    }
}

// `Send` and `Sync` are the automatic ones: the only fields are a `&D`, which is both as long as
// `D: Sync`, and plain data. Every `D` here is a `DeviceTrait`, which requires `Send + Sync`.

impl<D: WasapiDeviceExt> DeviceTrait for WasapiConfigured<'_, D> {
    type SupportedInputConfigs = D::SupportedInputConfigs;
    type SupportedOutputConfigs = D::SupportedOutputConfigs;
    type Stream = D::Stream;

    /// As [`DeviceTrait::description`]: the endpoint is the same one whatever the share mode.
    fn description(&self) -> Result<DeviceDescription, Error> {
        self.device.description()
    }

    /// As [`DeviceTrait::id`]: the endpoint is the same one whatever the share mode.
    fn id(&self) -> Result<DeviceId, Error> {
        self.device.id()
    }

    /// As [`DeviceTrait::supports_input`]: the direction an endpoint carries audio in is a
    /// property of the endpoint, which no option here changes.
    ///
    /// Not answered by whether [`supported_input_configs`](Self::supported_input_configs) comes
    /// back non-empty: that would be a different, more expensive question — in exclusive mode,
    /// one blocking `IsFormatSupported` per candidate format — and would have to report a device
    /// that failed to answer as one that does not support input. Whether the endpoint accepts a
    /// given format under a share mode is what the configuration queries are for.
    fn supports_input(&self) -> bool {
        self.device.supports_input()
    }

    /// As [`DeviceTrait::supports_output`]. See [`supports_input`](Self::supports_input) for why
    /// the share mode does not enter into it.
    fn supports_output(&self) -> bool {
        self.device.supports_output()
    }

    /// Whether a synchronized duplex stream is possible.
    ///
    /// False for exclusive mode: it has nothing to say about a duplex stream, so
    /// [`build_duplex_stream_raw`](Self::build_duplex_stream_raw) refuses it, and answering the
    /// device's own capability here would promise a stream that cannot be built.
    fn supports_duplex(&self) -> bool {
        self.is_default() && self.device.supports_duplex()
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::supported_input_configs`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a CPAL
    /// configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        self.device.supported_input_configs_with(self.share_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a CPAL
    /// configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        self.device.supported_output_configs_with(self.share_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_input_config_with(self.share_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_output_config_with(self.share_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_input_stream_raw`], and additionally:
    ///
    /// - [`ErrorKind::ExclusiveModeDenied`] if exclusive-mode use of the endpoint is turned off.
    ///
    /// Three kinds it already reports arise in further circumstances:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if this is exclusive-mode loopback capture from an
    ///   output device: loopback taps the engine mixer, which exclusive mode bypasses.
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
    /// [`ErrorKind::ExclusiveModeDenied`]: crate::ErrorKind::ExclusiveModeDenied
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    /// [`ErrorKind::DeviceBusy`]: crate::ErrorKind::DeviceBusy
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn build_input_stream_raw<F, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: F,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        F: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.device.build_input_stream_raw_with(
            config,
            sample_format,
            self.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_output_stream_raw`], and additionally:
    ///
    /// - [`ErrorKind::ExclusiveModeDenied`] if exclusive-mode use of the endpoint is turned off.
    ///
    /// Two kinds it already reports arise in further circumstances:
    ///
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
    /// [`ErrorKind::ExclusiveModeDenied`]: crate::ErrorKind::ExclusiveModeDenied
    /// [`ErrorKind::DeviceBusy`]: crate::ErrorKind::DeviceBusy
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn build_output_stream_raw<F, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: F,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        F: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.device.build_output_stream_raw_with(
            config,
            sample_format,
            self.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_duplex_stream_raw`], and additionally
    /// [`ErrorKind::UnsupportedOperation`] in exclusive mode: WASAPI has no exclusive-mode
    /// duplex streams, and the mode has nowhere to be honoured on a builder that would otherwise
    /// open shared mode.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    fn build_duplex_stream_raw<F, E>(
        &self,
        config: DuplexStreamConfig,
        input_sample_format: SampleFormat,
        output_sample_format: SampleFormat,
        data_callback: F,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        F: FnMut(&Data, &mut Data, &DuplexCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        if !self.is_default() {
            return Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Exclusive mode does not apply to duplex streams",
            ));
        }
        self.device.build_duplex_stream_raw(
            config,
            input_sample_format,
            output_sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }
}

/// The invariant [`sealed::Sealed`] documents, in the one place it could be broken: a mode that
/// reaches a device with no WASAPI endpoint behind it is the default one, because
/// [`WasapiDeviceExt::with_options`] refused every other kind.
fn assert_options_default(share_mode: ShareMode) {
    debug_assert_eq!(
        share_mode,
        ShareMode::Shared,
        "non-default share mode reached a device with no WASAPI endpoint",
    );
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

pub(crate) mod sealed {
    use std::time::Duration;

    use super::ShareMode;
    use crate::{
        CallbackInfo, Data, Error, SampleFormat, StreamConfig, SupportedStreamConfig,
        traits::DeviceTrait,
    };

    /// The half of [`WasapiDeviceExt`](super::WasapiDeviceExt) that reaches a backend, and the
    /// seal on it.
    ///
    /// These are the operations [`WasapiConfigured`](super::WasapiConfigured) implements
    /// [`DeviceTrait`] with. They are not public: a share-mode-aware method that shadows a
    /// cross-platform one is exactly the shape this API is trying not to have, and every one of
    /// them is reachable through `DeviceTrait` on a configured device.
    ///
    /// Implementations may assume `share_mode` has already been checked against
    /// [`has_wasapi_endpoint`](Self::has_wasapi_endpoint), because
    /// [`with_options`](super::WasapiDeviceExt::with_options) is the only way to obtain the
    /// configured device that calls them.
    pub trait Sealed: DeviceTrait {
        /// Whether a WASAPI endpoint sits behind this device, and so whether a mode other than
        /// the default can be honoured at all.
        fn has_wasapi_endpoint(&self) -> bool;

        /// [`DeviceTrait::default_input_config`] under `share_mode`.
        fn default_input_config_with(
            &self,
            share_mode: ShareMode,
        ) -> Result<SupportedStreamConfig, Error>;

        /// [`DeviceTrait::default_output_config`] under `share_mode`.
        fn default_output_config_with(
            &self,
            share_mode: ShareMode,
        ) -> Result<SupportedStreamConfig, Error>;

        /// [`DeviceTrait::supported_input_configs`] under `share_mode`.
        fn supported_input_configs_with(
            &self,
            share_mode: ShareMode,
        ) -> Result<Self::SupportedInputConfigs, Error>;

        /// [`DeviceTrait::supported_output_configs`] under `share_mode`.
        fn supported_output_configs_with(
            &self,
            share_mode: ShareMode,
        ) -> Result<Self::SupportedOutputConfigs, Error>;

        /// [`DeviceTrait::build_input_stream_raw`] under `share_mode`.
        fn build_input_stream_raw_with<D, E>(
            &self,
            config: StreamConfig,
            sample_format: SampleFormat,
            share_mode: ShareMode,
            data_callback: D,
            error_callback: E,
            timeout: Option<Duration>,
        ) -> Result<Self::Stream, Error>
        where
            D: FnMut(&Data, &CallbackInfo) + Send + 'static,
            E: FnMut(Error) + Send + 'static;

        /// [`DeviceTrait::build_output_stream_raw`] under `share_mode`.
        fn build_output_stream_raw_with<D, E>(
            &self,
            config: StreamConfig,
            sample_format: SampleFormat,
            share_mode: ShareMode,
            data_callback: D,
            error_callback: E,
            timeout: Option<Duration>,
        ) -> Result<Self::Stream, Error>
        where
            D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
            E: FnMut(Error) + Send + 'static;
    }
}

impl WasapiDeviceExt for super::Device {}

impl sealed::Sealed for super::Device {
    fn has_wasapi_endpoint(&self) -> bool {
        #[cfg(windows)]
        {
            wasapi_device(self).is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn default_input_config_with(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_input_config_for(share_mode);
        }
        // Not a WASAPI endpoint, so the mode is the default one and the cross-platform method is
        // exactly what it means. See `sealed::Sealed`.
        assert_options_default(share_mode);
        DeviceTrait::default_input_config(self)
    }

    fn default_output_config_with(
        &self,
        share_mode: ShareMode,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_output_config_for(share_mode);
        }
        assert_options_default(share_mode);
        DeviceTrait::default_output_config(self)
    }

    fn supported_input_configs_with(
        &self,
        share_mode: ShareMode,
    ) -> Result<Self::SupportedInputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .supported_input_configs_for(share_mode)
                .map(super::SupportedInputConfigs::from_wasapi);
        }
        assert_options_default(share_mode);
        DeviceTrait::supported_input_configs(self)
    }

    fn supported_output_configs_with(
        &self,
        share_mode: ShareMode,
    ) -> Result<Self::SupportedOutputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device
                .supported_output_configs_for(share_mode)
                .map(super::SupportedOutputConfigs::from_wasapi);
        }
        assert_options_default(share_mode);
        DeviceTrait::supported_output_configs(self)
    }

    fn build_input_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        share_mode: ShareMode,
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
                .build_input_stream_raw_for(
                    config,
                    sample_format,
                    share_mode,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        assert_options_default(share_mode);
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
        share_mode: ShareMode,
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
                .build_output_stream_raw_for(
                    config,
                    sample_format,
                    share_mode,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        assert_options_default(share_mode);
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
