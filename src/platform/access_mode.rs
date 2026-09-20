//! The access-mode seam: how the platform [`Device`](crate::Device) dispatch answers the
//! mode-aware operations.
//!
//! The public surface — [`AccessMode`](crate::AccessMode),
//! [`Device::with_access_mode`](crate::Device::with_access_mode) and the
//! [`ConfiguredDevice`](crate::ConfiguredDevice) they produce — lives at the crate root, because
//! exclusive access is a guarantee more than one backend will eventually offer. What is left
//! here is the dispatch: which backend, if any, can honour the bound mode.

use std::time::Duration;

use crate::{
    AccessMode, CallbackInfo, ConfiguredDevice, Data, DeviceDescription, DeviceId,
    DuplexCallbackInfo, DuplexStreamConfig, Error, ErrorKind, SampleFormat, StreamConfig,
    SupportedStreamConfig, traits::DeviceTrait,
};

/// Refuses a mode that no device behind this dispatch can honour.
///
/// The single place the refusal is worded and classified: a second backend with exclusive
/// support adds its own case to the dispatch below rather than a seventh copy of this error.
fn require_shared(access_mode: AccessMode) -> Result<(), Error> {
    if access_mode == AccessMode::Shared {
        return Ok(());
    }
    Err(Error::with_message(
        ErrorKind::UnsupportedOperation,
        "Exclusive mode requires a WASAPI device",
    ))
}

impl super::Device {
    /// Binds `access_mode` to this device, so the returned [`ConfiguredDevice`] answers the
    /// ordinary configuration queries and stream builders for that mode.
    ///
    /// Infallible: whether the mode can be honoured surfaces in the configured device's
    /// operations, not here, because availability is knowable only then — another process may
    /// hold the endpoint.
    pub fn with_access_mode(&self, access_mode: AccessMode) -> ConfiguredDevice<'_> {
        ConfiguredDevice::new(self, access_mode)
    }

    /// The WASAPI backend's own device behind this dispatch device, if that is what it is.
    ///
    /// The one place a backend is reached by type rather than through [`DeviceTrait`]. Replacing
    /// this with a trait-level hook — mode-aware methods each backend may override — is part of
    /// the upstream redesign ([RustAudio/cpal#1334](https://github.com/RustAudio/cpal/issues/1334));
    /// until then a second backend with exclusive support adds its own arm to the dispatch below.
    ///
    /// Crate-internal until `host::wasapi::Device` itself is public; the shape is the settled
    /// `as_wasapi()` mechanism, so exposing it later is a visibility change only.
    #[cfg(windows)]
    pub(crate) fn as_wasapi(&self) -> Option<&crate::host::wasapi::Device> {
        #[allow(unreachable_patterns)]
        match self.as_inner() {
            super::DeviceInner::Wasapi(device) => Some(device),
            _ => None,
        }
    }

    /// [`DeviceTrait::default_input_config`](crate::traits::DeviceTrait::default_input_config)
    /// under `access_mode`.
    pub(crate) fn default_input_config_with(
        &self,
        access_mode: AccessMode,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device.default_input_config_for(access_mode);
        }
        require_shared(access_mode)?;
        DeviceTrait::default_input_config(self)
    }

    /// [`DeviceTrait::default_output_config`](crate::traits::DeviceTrait::default_output_config)
    /// under `access_mode`. See [`default_input_config_with`](Self::default_input_config_with).
    pub(crate) fn default_output_config_with(
        &self,
        access_mode: AccessMode,
    ) -> Result<SupportedStreamConfig, Error> {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device.default_output_config_for(access_mode);
        }
        require_shared(access_mode)?;
        DeviceTrait::default_output_config(self)
    }

    /// [`DeviceTrait::supported_input_configs`](crate::traits::DeviceTrait::supported_input_configs)
    /// under `access_mode`. See [`default_input_config_with`](Self::default_input_config_with).
    pub(crate) fn supported_input_configs_with(
        &self,
        access_mode: AccessMode,
    ) -> Result<<super::Device as DeviceTrait>::SupportedInputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device
                .supported_input_configs_for(access_mode)
                .map(super::SupportedInputConfigs::from_wasapi);
        }
        require_shared(access_mode)?;
        DeviceTrait::supported_input_configs(self)
    }

    /// [`DeviceTrait::supported_output_configs`](crate::traits::DeviceTrait::supported_output_configs)
    /// under `access_mode`. See [`default_input_config_with`](Self::default_input_config_with).
    pub(crate) fn supported_output_configs_with(
        &self,
        access_mode: AccessMode,
    ) -> Result<<super::Device as DeviceTrait>::SupportedOutputConfigs, Error> {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device
                .supported_output_configs_for(access_mode)
                .map(super::SupportedOutputConfigs::from_wasapi);
        }
        require_shared(access_mode)?;
        DeviceTrait::supported_output_configs(self)
    }

    /// [`DeviceTrait::build_input_stream_raw`](crate::traits::DeviceTrait::build_input_stream_raw)
    /// under `access_mode`. See [`default_input_config_with`](Self::default_input_config_with).
    pub(crate) fn build_input_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        access_mode: AccessMode,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<<super::Device as DeviceTrait>::Stream, Error>
    where
        D: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device
                .build_input_stream_raw_for(
                    config,
                    sample_format,
                    access_mode,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        require_shared(access_mode)?;
        DeviceTrait::build_input_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }

    /// [`DeviceTrait::build_output_stream_raw`](crate::traits::DeviceTrait::build_output_stream_raw)
    /// under `access_mode`. See [`default_input_config_with`](Self::default_input_config_with).
    pub(crate) fn build_output_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        access_mode: AccessMode,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<<super::Device as DeviceTrait>::Stream, Error>
    where
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        #[cfg(windows)]
        if let Some(device) = self.as_wasapi() {
            return device
                .build_output_stream_raw_for(
                    config,
                    sample_format,
                    access_mode,
                    data_callback,
                    error_callback,
                    timeout,
                )
                .map(Into::into);
        }
        require_shared(access_mode)?;
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

impl<'a> DeviceTrait for ConfiguredDevice<'a> {
    type SupportedInputConfigs = <super::Device as DeviceTrait>::SupportedInputConfigs;
    type SupportedOutputConfigs = <super::Device as DeviceTrait>::SupportedOutputConfigs;
    type Stream = <super::Device as DeviceTrait>::Stream;

    /// As [`DeviceTrait::description`]: the endpoint is the same one whatever the access mode.
    fn description(&self) -> Result<DeviceDescription, Error> {
        self.device.description()
    }

    /// As [`DeviceTrait::id`]: the endpoint is the same one whatever the access mode.
    fn id(&self) -> Result<DeviceId, Error> {
        self.device.id()
    }

    /// As [`DeviceTrait::supports_input`]: the direction an endpoint carries audio in is a
    /// property of the endpoint, which no mode here changes.
    ///
    /// Not answered by whether [`supported_input_configs`](Self::supported_input_configs) comes
    /// back non-empty: that would be a different, more expensive question — in exclusive mode,
    /// one blocking `IsFormatSupported` per candidate format — and would have to report a device
    /// that failed to answer as one that does not support input. Whether the endpoint accepts a
    /// given format under an access mode is what the configuration queries are for.
    fn supports_input(&self) -> bool {
        self.device.supports_input()
    }

    /// As [`DeviceTrait::supports_output`]. See [`supports_input`](Self::supports_input) for why
    /// the access mode does not enter into it.
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
    fn supported_input_configs(
        &self,
    ) -> Result<<super::Device as DeviceTrait>::SupportedInputConfigs, Error> {
        self.device.supported_input_configs_with(self.access_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::supported_output_configs`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if the device's default format cannot be mapped to a CPAL
    /// configuration, or, in shared mode, is not supported by the audio engine.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn supported_output_configs(
        &self,
    ) -> Result<<super::Device as DeviceTrait>::SupportedOutputConfigs, Error> {
        self.device.supported_output_configs_with(self.access_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_input_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_input_config_with(self.access_mode)
    }

    /// # Errors
    ///
    /// As [`DeviceTrait::default_output_config`], and additionally
    /// [`ErrorKind::UnsupportedConfig`] if, in exclusive mode, the device accepts none of the
    /// sample formats CPAL probes for.
    ///
    /// [`ErrorKind::UnsupportedConfig`]: crate::ErrorKind::UnsupportedConfig
    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.device.default_output_config_with(self.access_mode)
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
    ) -> Result<<super::Device as DeviceTrait>::Stream, Error>
    where
        F: FnMut(&Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.device.build_input_stream_raw_with(
            config,
            sample_format,
            self.access_mode,
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
    ) -> Result<<super::Device as DeviceTrait>::Stream, Error>
    where
        F: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.device.build_output_stream_raw_with(
            config,
            sample_format,
            self.access_mode,
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
    ) -> Result<<super::Device as DeviceTrait>::Stream, Error>
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
