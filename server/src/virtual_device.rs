use async_trait::async_trait;
use n3ds_remote_play_common::InputState;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
mod windows;

/// Creates a new virtual device factory using the platform-specific APIs.
pub fn new_device_factory() -> anyhow::Result<impl VirtualDeviceFactory> {
    #[cfg(target_os = "linux")]
    return Ok(linux::UInputDeviceFactory);
    #[cfg(target_os = "windows")]
    return windows::ViGEmDeviceFactory::new();
}

/// A virtual device factory creates virtual devices.
///
/// Some virtual device APIs require setup work before they can create a device,
/// hence this trait.
#[async_trait]
pub trait VirtualDeviceFactory: Clone {
    type Device: VirtualDevice;

    async fn new_device(&self) -> anyhow::Result<Self::Device>;
}

/// A virtual device acts like a real gamepad device, but is controlled by software.
pub trait VirtualDevice: Sized + Send {
    fn emit_input(&mut self, input_state: InputState) -> anyhow::Result<()>;
}
