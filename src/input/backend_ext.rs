use ::input as libinput;
use smithay::backend::input;
use smithay::output::Output;
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use smithay::wayland::virtual_pointer::VirtualPointerDevice;

use crate::niri::State;

pub trait NiriInputBackend: input::InputBackend<Device = Self::NiriDevice> {
    type NiriDevice: NiriInputDevice;
}
impl<T: input::InputBackend> NiriInputBackend for T
where
    Self::Device: NiriInputDevice,
{
    type NiriDevice = Self::Device;
}

pub trait NiriInputDevice: input::Device {
    // FIXME: this should maybe be per-event, not per-device,
    // but it's not clear that this matters in practice?
    // it might be more obvious once we implement it for libinput
    fn output(&self, state: &State) -> Option<Output>;
}

impl NiriInputDevice for libinput::Device {
    fn output(&self, _state: &State) -> Option<Output> {
        // FIXME: Allow specifying the output per-device?
        None
    }
}

impl NiriInputDevice for VirtualPointerDevice {
    fn output(&self, _: &State) -> Option<Output> {
        self.output()
    }
}

impl NiriInputDevice for VirtualKeyboardDevice {
    fn output(&self, _: &State) -> Option<Output> {
        None
    }
}
