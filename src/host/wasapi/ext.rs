//! The WASAPI backend's implementation of [`WasapiDeviceExt`].
//!
//! The trait, [`ShareMode`](crate::platform::wasapi_ext::ShareMode),
//! [`WasapiStreamOptions`] and [`WasapiConfigured`](crate::platform::wasapi_ext::WasapiConfigured)
//! all live in [`crate::platform::wasapi_ext`] and are compiled on every platform; this is the
//! half that actually reaches an endpoint, and is the only half that is Windows-only.

use std::time::Duration;

use crate::{
    CallbackInfo, Data, Error, SampleFormat, StreamConfig, SupportedStreamConfig,
    platform::wasapi_ext::{WasapiDeviceExt, WasapiStreamOptions, sealed::Sealed},
};

impl WasapiDeviceExt for super::Device {}

impl Sealed for super::Device {
    fn has_wasapi_endpoint(&self) -> bool {
        true
    }

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        self.default_input_config_for(options.share_mode)
    }

    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        self.default_output_config_for(options.share_mode)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedInputConfigs, Error> {
        self.supported_input_configs_for(options.share_mode)
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedOutputConfigs, Error> {
        self.supported_output_configs_for(options.share_mode)
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
        self.build_input_stream_raw_for(
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
        D: FnMut(&mut Data, &CallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_output_stream_raw_for(
            config,
            sample_format,
            options.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }
}
