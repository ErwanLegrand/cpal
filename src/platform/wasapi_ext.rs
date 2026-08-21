//! WASAPI-specific extensions to the cross-platform API.
//!
//! WASAPI can open an endpoint in one of two share modes. Everything CPAL does by default uses
//! *shared* mode, where the Windows audio engine mixes the stream with every other application's
//! and may resample or convert it on the way. *Exclusive* mode hands the endpoint to a single
//! client: no mixing, no format conversion, and typically a much smaller device period, at the
//! cost of the device becoming unavailable to everything else while the stream lives.
//!
//! Share mode is not part of [`StreamConfig`], because it is meaningless on every other backend.
//! It is requested through [`WasapiStreamOptions`], which [`WasapiDeviceExt::with_options`] binds
//! to a device once. What comes back is a [`WasapiConfigured`], which implements [`DeviceTrait`]
//! itself: the ordinary queries and builders on it answer for the options it carries, so the mode
//! is chosen in one place rather than repeated at every call.
//!
//! ```no_run
//! use cpal::platform::wasapi_ext::{WasapiDeviceExt, WasapiStreamOptions};
//! use cpal::traits::{DeviceTrait, HostTrait};
//!
//! let device = cpal::default_host().default_output_device().unwrap();
//! let exclusive = device.with_options(WasapiStreamOptions::exclusive())?;
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

use std::{fmt, hash, time::Duration};

use crate::{
    CallbackInfo, Data, DeviceDescription, DeviceId, DuplexCallbackInfo, DuplexStreamConfig, Error,
    ErrorKind, SampleFormat, StreamConfig, SupportedStreamConfig, traits::DeviceTrait,
};

/// How a WASAPI stream shares its endpoint with the rest of the system.
///
/// See the [module documentation](self) for what the two modes mean in practice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// Options selecting shared mode — the default, spelled out.
    ///
    /// A device configured with these behaves exactly as the device itself, on every platform,
    /// which is what makes it the option to pass when the mode is chosen at runtime.
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

/// Binds [`WasapiStreamOptions`] to a device, for the WASAPI features the cross-platform API has
/// no vocabulary for.
///
/// This trait is sealed: it is implemented for [`crate::Device`] on every platform and for the
/// WASAPI backend's own device on Windows, and cannot be implemented outside this crate.
pub trait WasapiDeviceExt: sealed::Sealed + Sized {
    /// Binds `options` to this device, returning a [`WasapiConfigured`] whose [`DeviceTrait`]
    /// methods answer for them.
    ///
    /// The supported-format set, the default format and the buffer size a device reports all
    /// differ between the two share modes, so this is deliberately the only way to reach the
    /// exclusive-mode ones: negotiating and building happen on one value, carrying one mode.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if `options` are anything but the default and this
    ///   device has no WASAPI endpoint behind it. Default options are accepted by every device on
    ///   every platform.
    ///
    /// This is the only place that refusal can happen; a `WasapiConfigured` that exists can be
    /// asked for the mode it carries.
    ///
    /// [`ErrorKind::UnsupportedOperation`]: crate::ErrorKind::UnsupportedOperation
    fn with_options(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<WasapiConfigured<'_, Self>, Error> {
        // Compared against the default as a whole rather than by field, so an option added to
        // `WasapiStreamOptions` later is refused here too instead of being silently dropped.
        if options != WasapiStreamOptions::default() && !self.has_wasapi_endpoint() {
            return Err(unsupported_options(options));
        }
        Ok(WasapiConfigured {
            device: self,
            options,
        })
    }
}

/// A device with [`WasapiStreamOptions`] bound to it, from
/// [`with_options`](WasapiDeviceExt::with_options).
///
/// It implements [`DeviceTrait`], so the configuration queries and stream builders are the
/// cross-platform ones, answering for the options it carries. With the default options it behaves
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
pub struct WasapiConfigured<'a, D> {
    device: &'a D,
    options: WasapiStreamOptions,
}

impl<D> WasapiConfigured<'_, D> {
    /// The options this device is configured with.
    pub fn options(&self) -> WasapiStreamOptions {
        self.options
    }

    /// Whether these options are the ones every device honours, in which case every method
    /// forwards to the bare device unchanged.
    fn is_default(&self) -> bool {
        self.options == WasapiStreamOptions::default()
    }
}

impl<D> Clone for WasapiConfigured<'_, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for WasapiConfigured<'_, D> {}

impl<D: fmt::Debug> fmt::Debug for WasapiConfigured<'_, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasapiConfigured")
            .field("device", self.device)
            .field("options", &self.options)
            .finish()
    }
}

/// The device's own name, unchanged: this is the same endpoint, and [`DeviceTrait`] documents
/// `to_string()` as the way to get a device's name.
impl<D: fmt::Display> fmt::Display for WasapiConfigured<'_, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.device, f)
    }
}

impl<D: PartialEq> PartialEq for WasapiConfigured<'_, D> {
    fn eq(&self, other: &Self) -> bool {
        self.options == other.options && self.device == other.device
    }
}

impl<D: Eq> Eq for WasapiConfigured<'_, D> {}

impl<D: hash::Hash> hash::Hash for WasapiConfigured<'_, D> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.device.hash(state);
        self.options.hash(state);
    }
}

// `Send` and `Sync` are the automatic ones: the only fields are a `&D`, which is both as long as
// `D: Sync`, and plain data. Every `D` here is a `DeviceTrait`, which requires `Send + Sync`.

impl<D: WasapiDeviceExt> DeviceTrait for WasapiConfigured<'_, D> {
    type SupportedInputConfigs = D::SupportedInputConfigs;
    type SupportedOutputConfigs = D::SupportedOutputConfigs;
    type Stream = D::Stream;

    /// As [`DeviceTrait::description`]: the endpoint is the same one whatever the options.
    fn description(&self) -> Result<DeviceDescription, Error> {
        self.device.description()
    }

    /// As [`DeviceTrait::id`]: the endpoint is the same one whatever the options.
    fn id(&self) -> Result<DeviceId, Error> {
        self.device.id()
    }

    /// Whether the device supports input *under these options*.
    ///
    /// With the default options this is the device's own answer. Otherwise it is decided by
    /// [`supported_input_configs`](Self::supported_input_configs), which in exclusive mode means
    /// probing the endpoint rather than reading a flag.
    fn supports_input(&self) -> bool {
        if self.is_default() {
            return self.device.supports_input();
        }
        self.supported_input_configs()
            .is_ok_and(|mut configs| configs.next().is_some())
    }

    /// Whether the device supports output *under these options*.
    ///
    /// With the default options this is the device's own answer. Otherwise it is decided by
    /// [`supported_output_configs`](Self::supported_output_configs), which in exclusive mode means
    /// probing the endpoint rather than reading a flag.
    fn supports_output(&self) -> bool {
        if self.is_default() {
            return self.device.supports_output();
        }
        self.supported_output_configs()
            .is_ok_and(|mut configs| configs.next().is_some())
    }

    /// Whether a synchronized duplex stream is possible.
    ///
    /// False for any non-default options: those options have nothing to say about a duplex
    /// stream, so [`build_duplex_stream_raw`](Self::build_duplex_stream_raw) refuses them, and
    /// answering the device's own capability here would promise a stream that cannot be built.
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
        self.device.supported_input_configs_with(self.options)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a CPAL
    /// configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        self.device.supported_output_configs_with(self.options)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_input_config_with(self.options)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_output_config_with(self.options)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_input_stream_raw`]. Exclusive mode adds no error kind to that set;
    /// three of them just arise in further circumstances:
    ///
    /// - [`ErrorKind::UnsupportedOperation`] if this is exclusive-mode loopback capture from an
    ///   output device: loopback taps the engine mixer, which exclusive mode bypasses.
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
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
            self.options,
            data_callback,
            error_callback,
            timeout,
        )
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_output_stream_raw`]. Exclusive mode adds no error kind to that set;
    /// two of them just arise in further circumstances:
    ///
    /// - [`ErrorKind::DeviceBusy`] if another application already holds the endpoint.
    /// - [`ErrorKind::UnsupportedConfig`] if the device does not accept `config`/`sample_format`
    ///   natively — there is no engine to convert for it.
    ///
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
            self.options,
            data_callback,
            error_callback,
            timeout,
        )
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::build_duplex_stream_raw`], and additionally
    /// [`ErrorKind::UnsupportedOperation`] for any non-default options: no WASAPI share mode
    /// offers duplex streams, and these options have nowhere to be honoured on a builder that
    /// would otherwise open shared mode.
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
                "WASAPI stream options do not apply to duplex streams",
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

/// The invariant [`sealed::Sealed`] documents, in the one place it could be broken: options that
/// reach a device with no WASAPI endpoint behind it are the default ones, because
/// [`WasapiDeviceExt::with_options`] refused every other kind.
fn assert_options_default(options: WasapiStreamOptions) {
    debug_assert_eq!(
        options,
        WasapiStreamOptions::default(),
        "non-default WASAPI options reached a device with no WASAPI endpoint",
    );
}

/// The error reported when WASAPI-specific options are asked of a device with no WASAPI
/// endpoint. Exclusive mode is named separately, being the option a caller is most likely to
/// have asked for deliberately.
fn unsupported_options(options: WasapiStreamOptions) -> Error {
    if options.share_mode != ShareMode::Shared {
        return Error::with_message(
            ErrorKind::UnsupportedOperation,
            "Exclusive mode requires a WASAPI device",
        );
    }
    Error::with_message(
        ErrorKind::UnsupportedOperation,
        "These WASAPI stream options require a WASAPI device",
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

pub(crate) mod sealed {
    use std::time::Duration;

    use super::WasapiStreamOptions;
    use crate::{
        CallbackInfo, Data, Error, SampleFormat, StreamConfig, SupportedStreamConfig,
        traits::DeviceTrait,
    };

    /// The half of [`WasapiDeviceExt`](super::WasapiDeviceExt) that reaches a backend, and the
    /// seal on it.
    ///
    /// These are the operations [`WasapiConfigured`](super::WasapiConfigured) implements
    /// [`DeviceTrait`] with. They are not public: an options-aware method that shadows a
    /// cross-platform one is exactly the shape this API is trying not to have, and every one of
    /// them is reachable through `DeviceTrait` on a configured device.
    ///
    /// Implementations may assume `options` have already been checked against
    /// [`has_wasapi_endpoint`](Self::has_wasapi_endpoint), because
    /// [`with_options`](super::WasapiDeviceExt::with_options) is the only way to obtain the
    /// configured device that calls them.
    pub trait Sealed: DeviceTrait {
        /// Whether a WASAPI endpoint sits behind this device, and so whether options other than
        /// the default can be honoured at all.
        fn has_wasapi_endpoint(&self) -> bool;

        /// [`DeviceTrait::default_input_config`] under `options`.
        fn default_input_config_with(
            &self,
            options: WasapiStreamOptions,
        ) -> Result<SupportedStreamConfig, Error>;

        /// [`DeviceTrait::default_output_config`] under `options`.
        fn default_output_config_with(
            &self,
            options: WasapiStreamOptions,
        ) -> Result<SupportedStreamConfig, Error>;

        /// [`DeviceTrait::supported_input_configs`] under `options`.
        fn supported_input_configs_with(
            &self,
            options: WasapiStreamOptions,
        ) -> Result<Self::SupportedInputConfigs, Error>;

        /// [`DeviceTrait::supported_output_configs`] under `options`.
        fn supported_output_configs_with(
            &self,
            options: WasapiStreamOptions,
        ) -> Result<Self::SupportedOutputConfigs, Error>;

        /// [`DeviceTrait::build_input_stream_raw`] under `options`.
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

        /// [`DeviceTrait::build_output_stream_raw`] under `options`.
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
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = wasapi_device(self) {
            return device.default_input_config_with(options);
        }
        // Not a WASAPI endpoint, so the options are the default ones and the cross-platform
        // method is exactly what they mean. See `sealed::Sealed`.
        assert_options_default(options);
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
        assert_options_default(options);
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
        assert_options_default(options);
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
        assert_options_default(options);
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
        assert_options_default(options);
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
        assert_options_default(options);
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
