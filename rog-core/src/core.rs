// Return show-stopping errors, otherwise map error to a log level

use crate::{config::Config, virt_device::VirtKeys};
use log::{debug, error, info, warn};
use rog_aura::{aura_brightness_bytes, error::AuraError, BuiltInModeByte};
use rusb::DeviceHandle;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::marker::{PhantomData, PhantomPinned};
use std::path::Path;
use std::process::Command;
use std::ptr::NonNull;
use std::time::Duration;

static LED_INIT1: [u8; 2] = [0x5d, 0xb9];
static LED_INIT2: &str = "]ASUS Tech.Inc."; // ] == 0x5d
static LED_INIT3: [u8; 6] = [0x5d, 0x05, 0x20, 0x31, 0, 0x08];
static LED_INIT4: &str = "^ASUS Tech.Inc."; // ^ == 0x5e
static LED_INIT5: [u8; 6] = [0x5e, 0x05, 0x20, 0x31, 0, 0x08];

// Only these two packets must be 17 bytes
static LED_APPLY: [u8; 17] = [0x5d, 0xb4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
static LED_SET: [u8; 17] = [0x5d, 0xb5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

static FAN_TYPE_1_PATH: &str = "/sys/devices/platform/asus-nb-wmi/throttle_thermal_policy";
static FAN_TYPE_2_PATH: &str = "/sys/devices/platform/asus-nb-wmi/fan_boost_mode";

/// ROG device controller
///
/// For the GX502GW the LED setup sequence looks like:
///
/// -` LED_INIT1`
/// - `LED_INIT3`
/// - `LED_INIT4`
/// - `LED_INIT2`
/// - `LED_INIT4`
pub struct RogCore {
    handle: DeviceHandle<rusb::GlobalContext>,
    virt_keys: VirtKeys,
    _pin: PhantomPinned,
}

impl RogCore {
    pub fn new(vendor: u16, product: u16, led_endpoint: u8) -> Result<RogCore, Box<dyn Error>> {
        let mut dev_handle = RogCore::get_device(vendor, product)?;
        dev_handle.set_active_configuration(0).unwrap_or(());

        let dev_config = dev_handle.device().config_descriptor(0).unwrap();
        // Interface with outputs
        let mut interface = 0;
        for iface in dev_config.interfaces() {
            for desc in iface.descriptors() {
                for endpoint in desc.endpoint_descriptors() {
                    if endpoint.address() == led_endpoint {
                        info!("INTERVAL: {:?}", endpoint.interval());
                        info!("MAX_PKT_SIZE: {:?}", endpoint.max_packet_size());
                        info!("SYNC: {:?}", endpoint.sync_type());
                        info!("TRANSFER_TYPE: {:?}", endpoint.transfer_type());
                        info!("ENDPOINT: {:X?}", endpoint.address());
                        interface = desc.interface_number();
                        break;
                    }
                }
            }
        }

        dev_handle.set_auto_detach_kernel_driver(true).unwrap();
        dev_handle.claim_interface(interface)?;

        Ok(RogCore {
            handle: dev_handle,
            virt_keys: VirtKeys::new(),
            _pin: PhantomPinned,
        })
    }

    pub async fn reload(&mut self, config: &mut Config) -> Result<(), Box<dyn Error>> {
        let path = if Path::new(FAN_TYPE_1_PATH).exists() {
            FAN_TYPE_1_PATH
        } else if Path::new(FAN_TYPE_2_PATH).exists() {
            FAN_TYPE_2_PATH
        } else {
            return Ok(());
        };

        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(format!("{:?}\n", config.fan_mode).as_bytes())?;
        self.set_pstate_for_fan_mode(FanLevel::from(config.fan_mode), config)?;
        info!("Reloaded last saved settings");
        Ok(())
    }

    pub fn virt_keys(&mut self) -> &mut VirtKeys {
        &mut self.virt_keys
    }

    fn get_device(
        vendor: u16,
        product: u16,
    ) -> Result<DeviceHandle<rusb::GlobalContext>, rusb::Error> {
        for device in rusb::devices().unwrap().iter() {
            let device_desc = device.device_descriptor().unwrap();
            if device_desc.vendor_id() == vendor && device_desc.product_id() == product {
                return device.open();
            }
        }
        Err(rusb::Error::NoDevice)
    }

    pub fn fan_mode_step(&mut self, config: &mut Config) -> Result<(), Box<dyn Error>> {
        let path = if Path::new(FAN_TYPE_1_PATH).exists() {
            FAN_TYPE_1_PATH
        } else if Path::new(FAN_TYPE_2_PATH).exists() {
            FAN_TYPE_2_PATH
        } else {
            return Ok(());
        };

        let mut fan_ctrl = OpenOptions::new().read(true).write(true).open(path)?;

        let mut buf = String::new();
        if let Ok(_) = fan_ctrl.read_to_string(&mut buf) {
            let mut n = u8::from_str_radix(&buf.trim_end(), 10)?;
            info!("Current fan mode: {:#?}", FanLevel::from(n));
            // wrap around the step number
            if n < 2 {
                n += 1;
            } else {
                n = 0;
            }
            info!("Fan mode stepped to: {:#?}", FanLevel::from(n));
            fan_ctrl.write_all(format!("{:?}\n", n).as_bytes())?;
            self.set_pstate_for_fan_mode(FanLevel::from(n), config)?;
            config.fan_mode = n;
            config.write();
        }
        Ok(())
    }

    fn set_pstate_for_fan_mode(
        &self,
        mode: FanLevel,
        config: &mut Config,
    ) -> Result<(), Box<dyn Error>> {
        // Set CPU pstate
        if let Ok(pstate) = intel_pstate::PState::new() {
            // re-read the config here in case a user changed the pstate settings
            config.read();
            match mode {
                FanLevel::Normal => {
                    pstate.set_min_perf_pct(config.mode_performance.normal.min_percentage)?;
                    pstate.set_max_perf_pct(config.mode_performance.normal.max_percentage)?;
                    pstate.set_no_turbo(config.mode_performance.normal.no_turbo)?;
                    info!(
                        "CPU Power: min-freq: {:?}, max-freq: {:?}, turbo: {:?}",
                        config.mode_performance.normal.min_percentage,
                        config.mode_performance.normal.max_percentage,
                        !config.mode_performance.normal.no_turbo
                    );
                }
                FanLevel::Boost => {
                    pstate.set_min_perf_pct(config.mode_performance.boost.min_percentage)?;
                    pstate.set_max_perf_pct(config.mode_performance.boost.max_percentage)?;
                    pstate.set_no_turbo(config.mode_performance.boost.no_turbo)?;
                    info!(
                        "CPU Power: min-freq: {:?}, max-freq: {:?}, turbo: {:?}",
                        config.mode_performance.boost.min_percentage,
                        config.mode_performance.boost.max_percentage,
                        !config.mode_performance.boost.no_turbo
                    );
                }
                FanLevel::Silent => {
                    pstate.set_min_perf_pct(config.mode_performance.silent.min_percentage)?;
                    pstate.set_max_perf_pct(config.mode_performance.silent.max_percentage)?;
                    pstate.set_no_turbo(config.mode_performance.silent.no_turbo)?;
                    info!(
                        "CPU Power: min-freq: {:?}, max-freq: {:?}, turbo: {:?}",
                        config.mode_performance.silent.min_percentage,
                        config.mode_performance.silent.max_percentage,
                        !config.mode_performance.silent.no_turbo
                    );
                }
            }
        }
        Ok(())
    }

    /// A direct call to systemd to suspend the PC.
    ///
    /// This avoids desktop environments being required to handle it
    /// (which means it works while in a TTY also)
    pub fn suspend_with_systemd(&self) {
        std::process::Command::new("systemctl")
            .arg("suspend")
            .spawn()
            .map_or_else(|err| warn!("Failed to suspend: {}", err), |_| {});
    }

    /// A direct call to rfkill to suspend wireless devices.
    ///
    /// This avoids desktop environments being required to handle it (which
    /// means it works while in a TTY also)
    pub fn toggle_airplane_mode(&self) {
        match Command::new("rfkill").arg("list").output() {
            Ok(output) => {
                if output.status.success() {
                    if let Ok(out) = String::from_utf8(output.stdout) {
                        if out.contains(": yes") {
                            Command::new("rfkill")
                                .arg("unblock")
                                .arg("all")
                                .spawn()
                                .map_or_else(
                                    |err| warn!("Could not unblock rf devices: {}", err),
                                    |_| {},
                                );
                        } else {
                            Command::new("rfkill")
                                .arg("block")
                                .arg("all")
                                .spawn()
                                .map_or_else(
                                    |err| warn!("Could not block rf devices: {}", err),
                                    |_| {},
                                );
                        }
                    }
                } else {
                    warn!("Could not list rf devices");
                }
            }
            Err(err) => {
                warn!("Could not list rf devices: {}", err);
            }
        }
    }

    pub fn get_raw_device_handle(&mut self) -> NonNull<DeviceHandle<rusb::GlobalContext>> {
        // Breaking every damn lifetime guarantee rust gives us
        unsafe {
            NonNull::new_unchecked(&mut self.handle as *mut DeviceHandle<rusb::GlobalContext>)
        }
    }
}

/// Lifetime is tied to `DeviceHandle` from `RogCore`
pub struct KeyboardReader<'d, C: 'd>
where
    C: rusb::UsbContext,
{
    handle: NonNull<DeviceHandle<C>>,
    endpoint: u8,
    filter: Vec<u8>,
    _phantom: PhantomData<&'d DeviceHandle<C>>,
}

/// UNSAFE
unsafe impl<'d, C> Send for KeyboardReader<'d, C> where C: rusb::UsbContext {}
unsafe impl<'d, C> Sync for KeyboardReader<'d, C> where C: rusb::UsbContext {}

impl<'d, C> KeyboardReader<'d, C>
where
    C: rusb::UsbContext,
{
    pub fn new(device_handle: NonNull<DeviceHandle<C>>, key_endpoint: u8, filter: Vec<u8>) -> Self {
        KeyboardReader {
            handle: device_handle,
            endpoint: key_endpoint,
            filter,
            _phantom: PhantomData,
        }
    }

    /// Write the bytes read from the device interrupt to the buffer arg, and returns the
    /// count of bytes written
    ///
    /// `report_filter_bytes` is used to filter the data read from the interupt so
    /// only the relevant byte array is returned.
    pub async fn poll_keyboard(&self) -> Option<[u8; 32]> {
        let mut buf = [0u8; 32];
        match unsafe { self.handle.as_ref() }.read_interrupt(
            self.endpoint,
            &mut buf,
            Duration::from_millis(200),
        ) {
            Ok(_) => {
                if self.filter.contains(&buf[0])
                    && (buf[1] != 0 || buf[2] != 0 || buf[3] != 0 || buf[4] != 0)
                {
                    return Some(buf);
                }
            }
            Err(err) => match err {
                rusb::Error::Timeout => {}
                _ => error!("Failed to read keyboard interrupt: {:?}", err),
            },
        }
        None
    }
}

pub enum AuraCommand {
    BrightInc,
    BrightDec,
    BuiltinNext,
    BuiltinPrev,
    WriteBytes(Vec<u8>),
    WriteEffect(Vec<Vec<u8>>),
    ReloadLast,
}

/// UNSAFE: Must live as long as RogCore
///
/// Because we're holding a pointer to something that *may* go out of scope while the
/// pointer is held. We're relying on access to struct to be behind a Mutex, and for behaviour
/// that may cause invalididated pointer to cause the program to panic rather than continue.
pub struct LedWriter<'d, C: 'd>
where
    C: rusb::UsbContext,
{
    handle: NonNull<DeviceHandle<C>>,
    bright_min_max: (u8, u8),
    supported_modes: Vec<BuiltInModeByte>,
    led_endpoint: u8,
    initialised: bool,
    _phantom: PhantomData<&'d DeviceHandle<C>>,
}

/// UNSAFE
unsafe impl<'d, C> Send for LedWriter<'d, C> where C: rusb::UsbContext {}
unsafe impl<'d, C> Sync for LedWriter<'d, C> where C: rusb::UsbContext {}

impl<'d, C> LedWriter<'d, C>
where
    C: rusb::UsbContext,
{
    pub fn new(
        device_handle: NonNull<DeviceHandle<C>>,
        led_endpoint: u8,
        bright_min_max: (u8, u8),
        supported_modes: Vec<BuiltInModeByte>,
    ) -> Self {
        LedWriter {
            handle: device_handle,
            led_endpoint,
            bright_min_max,
            supported_modes,
            initialised: false,
            _phantom: PhantomData,
        }
    }

    pub async fn do_command(
        &mut self,
        command: AuraCommand,
        config: &mut Config,
    ) -> Result<(), AuraError> {
        if !self.initialised {
            self.write_bytes(&LED_INIT1).await?;
            self.write_bytes(LED_INIT2.as_bytes()).await?;
            self.write_bytes(&LED_INIT3).await?;
            self.write_bytes(LED_INIT4.as_bytes()).await?;
            self.write_bytes(&LED_INIT5).await?;
            self.initialised = true;
        }

        match command {
            AuraCommand::BrightInc => {
                let mut bright = config.brightness;
                if bright < self.bright_min_max.1 {
                    bright += 1;
                    config.brightness = bright;
                    let bytes = aura_brightness_bytes(bright);
                    self.set_and_save(&bytes, config).await?;
                    info!("Increased LED brightness to {:#?}", bright);
                }
            }
            AuraCommand::BrightDec => {
                let mut bright = config.brightness;
                if bright > self.bright_min_max.0 {
                    bright -= 1;
                    config.brightness = bright;
                    let bytes = aura_brightness_bytes(bright);
                    self.set_and_save(&bytes, config).await?;
                    info!("Decreased LED brightness to {:#?}", bright);
                }
            }
            AuraCommand::BuiltinNext => {
                // TODO: different path for multi-zone (byte 2 controlled, non-zero)
                let mode_curr = config.current_mode[3];
                let idx = self
                    .supported_modes
                    .binary_search(&mode_curr.into())
                    .unwrap();
                let idx_next = if idx < self.supported_modes.len() - 1 {
                    idx + 1
                } else {
                    0
                };
                self.set_builtin(config, idx_next).await?;
            }
            AuraCommand::BuiltinPrev => {
                // TODO: different path for multi-zone (byte 2 controlled, non-zero)
                let mode_curr = config.current_mode[3];
                let idx = self
                    .supported_modes
                    .binary_search(&mode_curr.into())
                    .unwrap();
                let idx_next = if idx > 0 {
                    idx - 1
                } else {
                    self.supported_modes.len() - 1
                };
                self.set_builtin(config, idx_next).await?;
            }
            AuraCommand::WriteBytes(bytes) => self.set_and_save(&bytes, config).await?,
            AuraCommand::WriteEffect(effect) => self.write_effect(effect).await?,
            AuraCommand::ReloadLast => self.reload_last_builtin(&config).await?,
        }
        Ok(())
    }

    /// Should only be used if the bytes you are writing are verified correct
    async fn write_bytes(&mut self, message: &[u8]) -> Result<(), AuraError> {
        match unsafe { self.handle.as_ref() }.write_interrupt(
            self.led_endpoint,
            message,
            Duration::from_millis(5),
        ) {
            Ok(_) => {}
            Err(err) => match err {
                rusb::Error::Timeout => {}
                _ => error!("Failed to write to led interrupt: {:?}", err),
            },
        }
        Ok(())
    }

    async fn write_array_of_bytes(&mut self, messages: &[&[u8]]) -> Result<(), AuraError> {
        for message in messages {
            self.write_bytes(*message).await?;
            self.write_bytes(&LED_SET).await?;
        }
        // Changes won't persist unless apply is set
        self.write_bytes(&LED_APPLY).await?;
        Ok(())
    }

    /// Write an effect block
    ///
    /// `aura_effect_init` must be called any effect routine, and called only once.
    async fn write_effect(&self, effect: Vec<Vec<u8>>) -> Result<(), AuraError> {
        for row in effect.iter() {
            match unsafe { self.handle.as_ref() }.write_interrupt(
                self.led_endpoint,
                row,
                Duration::from_millis(1),
            ) {
                Ok(_) => {}
                Err(err) => match err {
                    rusb::Error::Timeout => {}
                    _ => error!("Failed to write LED interrupt: {:?}", err),
                },
            }
        }
        Ok(())
    }

    /// Used to set a builtin mode and save the settings for it
    async fn set_and_save(&mut self, bytes: &[u8], config: &mut Config) -> Result<(), AuraError> {
        let mode = BuiltInModeByte::from(bytes[3]);
        // safety pass-through of possible effect write
        if bytes[1] == 0xbc {
            self.write_bytes(bytes).await?;
            return Ok(());
        } else if self.supported_modes.contains(&mode) || bytes[1] == 0xba {
            let messages = [bytes];
            self.write_array_of_bytes(&messages).await?;
            config.set_field_from(bytes);
            config.write();
            return Ok(());
        }
        warn!("{:?} not supported", mode);
        Err(AuraError::NotSupported)
    }

    /// Used to set a builtin mode and save the settings for it
    async fn reload_last_builtin(&mut self, config: &Config) -> Result<(), AuraError> {
        let mode_curr = config.current_mode[3];
        let mode = config
            .builtin_modes
            .get_field_from(mode_curr)
            .unwrap()
            .to_owned();
        self.write_bytes(&mode).await?;
        info!("Reloaded last built-in mode");
        Ok(())
    }

    /// Select next Aura effect
    ///
    /// If the current effect is the last one then the effect selected wraps around to the first.
    async fn set_builtin(&mut self, config: &mut Config, index: usize) -> Result<(), AuraError> {
        let mode_next = config
            .builtin_modes
            .get_field_from(self.supported_modes[index].into())
            .unwrap()
            .to_owned();
        println!("{:X?}", &mode_next);
        self.set_and_save(&mode_next, config).await?;
        info!("Switched LED mode to {:#?}", self.supported_modes[index]);
        Ok(())
    }
}

#[derive(Debug)]
enum FanLevel {
    Normal,
    Boost,
    Silent,
}

impl From<u8> for FanLevel {
    fn from(n: u8) -> Self {
        match n {
            0 => FanLevel::Normal,
            1 => FanLevel::Boost,
            2 => FanLevel::Silent,
            _ => FanLevel::Normal,
        }
    }
}

impl From<FanLevel> for u8 {
    fn from(n: FanLevel) -> Self {
        match n {
            FanLevel::Normal => 0,
            FanLevel::Boost => 1,
            FanLevel::Silent => 2,
        }
    }
}
