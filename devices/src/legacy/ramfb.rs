// Copyright (c) 2022 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use super::fwcfg::{FwCfgOps, FwCfgWriteCallback};
use crate::legacy::Result;
use acpi::AmlBuilder;
use address_space::{AddressSpace, GuestAddress};
use anyhow::Context;
use drm_fourcc::DrmFourcc;
use log::error;
use std::mem::size_of;
use std::sync::{Arc, Mutex};
use sysbus::{Result as SysBusResult, SysBus, SysBusDevOps, SysBusDevType};
use util::pixman::{pixman_format_bpp, pixman_format_code_t, pixman_image_create_bits};
use vnc::console::{
    console_init, display_graphic_update, display_replace_surface, DisplayConsole, DisplaySurface,
    HardWareOperations,
};

const BYTES_PER_PIXELS: u32 = 8;
const WIDTH_MAX: u32 = 16_000;
const HEIGHT_MAX: u32 = 12_000;

#[repr(packed)]
struct RamfbCfg {
    _addr: u64,
    _fourcc: u32,
    _flags: u32,
    _width: u32,
    _height: u32,
    _stride: u32,
}

#[derive(Clone)]
pub struct RamfbState {
    pub surface: Option<DisplaySurface>,
    sys_mem: Arc<AddressSpace>,
}

// SAFETY: The type of image, the field of the struct DisplaySurface
// is the raw pointer. create_display_surface() method will create
// image object. The memory that the image pointer refers to is
// modified by guest OS and accessed by vnc. So implement Sync and
// Send is safe.
unsafe impl Sync for RamfbState {}
unsafe impl Send for RamfbState {}

impl RamfbState {
    pub fn new(sys_mem: Arc<AddressSpace>) -> Self {
        Self {
            surface: None,
            sys_mem,
        }
    }

    pub fn setup(&mut self, fw_cfg: &Arc<Mutex<dyn FwCfgOps>>) -> Result<()> {
        let mut locked_fw_cfg = fw_cfg.lock().unwrap();
        let ramfb_state_cb = self.clone();
        let cfg: Vec<u8> = [0; size_of::<RamfbCfg>()].to_vec();
        locked_fw_cfg
            .add_file_callback_entry(
                "etc/ramfb",
                cfg,
                None,
                Some(Arc::new(Mutex::new(ramfb_state_cb))),
                true,
            )
            .with_context(|| "Failed to set fwcfg")?;
        Ok(())
    }

    fn create_display_surface(
        &mut self,
        width: u32,
        height: u32,
        format: pixman_format_code_t,
        mut stride: u32,
        addr: u64,
    ) {
        if width < 16 || height < 16 || width > WIDTH_MAX || height > HEIGHT_MAX {
            error!("The resolution: {}x{} is unsupported.", width, height);
        }

        if stride == 0 {
            let linesize = width * pixman_format_bpp(format as u32) as u32 / BYTES_PER_PIXELS;
            stride = linesize;
        }

        let fb_addr = match self.sys_mem.get_host_address(GuestAddress(addr)) {
            Some(addr) => addr,
            None => {
                error!("Failed to get the host address of the framebuffer");
                return;
            }
        };

        let mut ds = DisplaySurface {
            format,
            ..Default::default()
        };
        // SAFETY: pixman_image_create_bits() is C function. All
        // parameters passed of the function have been checked.
        // It returns a raw pointer.
        unsafe {
            ds.image = pixman_image_create_bits(
                format,
                width as i32,
                height as i32,
                fb_addr as *mut u32,
                stride as i32,
            );
        }

        if ds.image.is_null() {
            error!("Failed to create the surface of Ramfb!");
            return;
        }

        self.surface = Some(ds);
    }

    fn reset_ramfb_state(&mut self) {
        self.surface = None;
    }
}

impl FwCfgWriteCallback for RamfbState {
    fn write_callback(&mut self, data: Vec<u8>, _start: u64, _len: usize) {
        if data.len() < 28 {
            error!("RamfbCfg data format is incorrect");
            return;
        }
        let addr = u64::from_be_bytes(
            data.as_slice()
                .split_at(size_of::<u64>())
                .0
                .try_into()
                .unwrap(),
        );
        let fourcc = u32::from_be_bytes(
            data.as_slice()[8..]
                .split_at(size_of::<u32>())
                .0
                .try_into()
                .unwrap(),
        );
        let width = u32::from_be_bytes(
            data.as_slice()[16..]
                .split_at(size_of::<u32>())
                .0
                .try_into()
                .unwrap(),
        );
        let height = u32::from_be_bytes(
            data.as_slice()[20..]
                .split_at(size_of::<u32>())
                .0
                .try_into()
                .unwrap(),
        );
        let stride = u32::from_be_bytes(
            data.as_slice()[24..]
                .split_at(size_of::<u32>())
                .0
                .try_into()
                .unwrap(),
        );

        let format: pixman_format_code_t = if fourcc == DrmFourcc::Xrgb8888 as u32 {
            pixman_format_code_t::PIXMAN_x8r8g8b8
        } else {
            error!("Unsupported drm format: {}", fourcc);
            return;
        };

        self.create_display_surface(width, height, format, stride, addr);

        let ramfb_opts = Arc::new(RamfbInterface {
            width: width as i32,
            height: height as i32,
        });
        let con = console_init(ramfb_opts);
        display_replace_surface(&con, self.surface)
            .unwrap_or_else(|e| error!("Error occurs during surface switching: {:?}", e));
    }
}

pub struct RamfbInterface {
    width: i32,
    height: i32,
}
impl HardWareOperations for RamfbInterface {
    fn hw_update(&self, con: Arc<Mutex<DisplayConsole>>) {
        display_graphic_update(&Some(Arc::downgrade(&con)), 0, 0, self.width, self.height)
            .unwrap_or_else(|e| error!("Error occurs during graphic updating: {:?}", e));
    }
}

pub struct Ramfb {
    pub ramfb_state: RamfbState,
}

impl Ramfb {
    pub fn new(sys_mem: Arc<AddressSpace>) -> Self {
        Ramfb {
            ramfb_state: RamfbState::new(sys_mem),
        }
    }

    pub fn realize(self, sysbus: &mut SysBus) -> Result<()> {
        let dev = Arc::new(Mutex::new(self));
        sysbus.attach_dynamic_device(&dev)?;
        Ok(())
    }
}

impl SysBusDevOps for Ramfb {
    fn read(&mut self, _data: &mut [u8], _base: GuestAddress, _offset: u64) -> bool {
        error!("Ramfb can not be read!");
        false
    }

    fn write(&mut self, _data: &[u8], _base: GuestAddress, _offset: u64) -> bool {
        error!("Ramfb can not be writed!");
        false
    }

    fn get_type(&self) -> SysBusDevType {
        SysBusDevType::Ramfb
    }

    fn reset(&mut self) -> SysBusResult<()> {
        self.ramfb_state.reset_ramfb_state();
        Ok(())
    }
}

impl AmlBuilder for Ramfb {
    fn aml_bytes(&self) -> Vec<u8> {
        Vec::new()
    }
}
