use async_trait::async_trait;
use n3ds_controller_common::InputMessage;

#[cfg(target_os = "linux")]
mod linux;

/// Creates a new virtual device using the platform-specific APIs.
pub async fn new() -> anyhow::Result<impl VirtualDevice> {
    #[cfg(target_os = "linux")]
    linux::UInputDevice::new().await
}

/// A virtual device acts like a real gamepad device, but is controlled by software.
#[async_trait]
pub trait VirtualDevice: Sized {
    async fn new() -> anyhow::Result<Self>;

    fn emit_input(&self, message: InputMessage) -> anyhow::Result<()>;
}
