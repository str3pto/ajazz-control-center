//! Direct native driver for AJAZZ AKP153 Stream Dock family using hidapi.
//!
//! Speaks the exact hardware wire protocol calibrated from pyajazz:
//! - 512-byte CRT command frames with unpadded streaming chunks
//! - 15 LCD keys mapped from UI coordinates [1..15] to hardware slots
//! - 3-zone vertical side LCD strip [TOP=16, MID=17, BOT=18]
//! - 85x85 JPEG encoding with upright orientation (Rot270 counter-clockwise)
//! - Bi-directional input reporting with slot-to-UI translation

use std::sync::{Arc, Mutex};
use hidapi::{HidApi, HidDevice};
use image::{imageops::FilterType, DynamicImage, ColorType, codecs::jpeg::JpegEncoder};

/// Hardware slot mapping for the 15-key grid.
/// UI index 0..14 (Row 1: 1..5, Row 2: 6..10, Row 3: 11..15)
/// Maps to hardware BAT slots [13, 10, 7, 4, 1, 14, 11, 8, 5, 2, 15, 12, 9, 6, 3]
const UI_INDEX_TO_SLOT: [u8; 15] = [
    13, 10, 7, 4, 1,
    14, 11, 8, 5, 2,
    15, 12, 9, 6, 3,
];

pub struct Akp153Device {
    device: Arc<Mutex<HidDevice>>,
    pub serial: String,
    pub vid: u16,
    pub pid: u16,
}

impl Akp153Device {
    /// Attempts to find and open an AKP153 device.
    pub fn open(vid: u16, pid: u16) -> Option<Self> {
        let api = match HidApi::new() {
            Ok(a) => a,
            Err(_) => return None,
        };

        // Locate device with usage_page 0xFFA0 or interface 0
        for dev_info in api.device_list() {
            if dev_info.vendor_id() == vid && dev_info.product_id() == pid {
                // Prefer usage_page 0xFFA0 (65440) or interface 0
                if dev_info.usage_page() == 0xFFA0 || dev_info.interface_number() == 0 {
                    if let Ok(dev) = dev_info.open_device(&api) {
                        let serial = dev_info
                            .serial_number()
                            .unwrap_or("4250D2785144")
                            .to_string();
                        let akp = Self {
                            device: Arc::new(Mutex::new(dev)),
                            serial,
                            vid,
                            pid,
                        };
                        akp.init();
                        return Some(akp);
                    }
                }
            }
        }
        None
    }

    /// Sends the standard init sequence: DIS, CLE, STP, LIG 80%
    pub fn init(&self) {
        let _ = self.write_cmd(b"CRT\x00\x00DIS");
        let _ = self.write_cmd(b"CRT\x00\x00CLE\x00\x00\x00\xff");
        let _ = self.write_cmd(b"CRT\x00\x00STP");
        let _ = self.set_brightness(80);
    }

    fn write_cmd(&self, data: &[u8]) -> Result<(), String> {
        let mut buf = Vec::with_capacity(1 + data.len());
        buf.push(0x00); // Report ID 0
        buf.extend_from_slice(data);
        let dev = self.device.lock().map_err(|e| format!("Mutex lock error: {e}"))?;
        dev.write(&buf)
            .map_err(|e| format!("HID write error: {e}"))?;
        Ok(())
    }

    pub fn set_brightness(&self, percent: u8) -> Result<(), String> {
        let p = percent.clamp(0, 100);
        let mut cmd = b"CRT\x00\x00LIG\x00\x00\x00".to_vec();
        cmd[7] = p;
        self.write_cmd(&cmd)
    }

    pub fn keep_alive(&self) -> Result<(), String> {
        self.write_cmd(b"CRT\x00\x00CONNECT")
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let _ = self.write_cmd(b"CRT\x00\x00CLE\x00\x00\x44\x43");
    }

    /// Renders an image to a key or touchzone.
    /// `key`: 0..14 or 1..15 for grid keys; 0..2 for touchzone (TOP, MID, BOT).
    pub fn set_image(&self, key: u8, touchzone: bool, img: DynamicImage) -> Result<(), String> {
        let slot = if touchzone {
            match key {
                0 => 16, // Top
                1 => 17, // Mid
                2 => 18, // Bot
                other => 16 + other.min(2),
            }
        } else {
            let idx = (key as usize).min(14);
            UI_INDEX_TO_SLOT[idx]
        };

        // Resize exact to 85x85
        let resized = img.resize_exact(85, 85, FilterType::Nearest);
        // Upright rotation: Rot270 (matches PIL rotate(90))
        let rotated = resized.rotate270();
        let rgb_data = rotated.into_rgb8().into_raw();

        let mut jpeg = Vec::new();
        {
            let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 85);
            encoder
                .encode(&rgb_data, 85, 85, ColorType::Rgb8.into())
                .map_err(|e| format!("JPEG encode error: {e}"))?;
        }

        // Lock device for the full atomic BAT + chunks + STP write sequence
        let dev = self.device.lock().map_err(|e| format!("Mutex lock error: {e}"))?;

        // 1. Send BAT announce frame
        let len = jpeg.len();
        let mut bat = b"CRT\x00\x00BAT\x00\x00\x00\x00\x00".to_vec();
        bat[10] = (len >> 8) as u8;
        bat[11] = (len & 0xFF) as u8;
        bat[12] = slot;
        let mut bat_buf = Vec::with_capacity(1 + bat.len());
        bat_buf.push(0x00);
        bat_buf.extend_from_slice(&bat);
        dev.write(&bat_buf).map_err(|e| format!("BAT write error: {e}"))?;

        // 2. Stream unpadded 1024-byte chunks
        for chunk in jpeg.chunks(1024) {
            let mut chunk_buf = Vec::with_capacity(1 + chunk.len());
            chunk_buf.push(0x00);
            chunk_buf.extend_from_slice(chunk);
            dev.write(&chunk_buf)
                .map_err(|e| format!("Chunk write error: {e}"))?;
        }

        // 3. Flush commit
        let mut stp_buf = Vec::with_capacity(1 + 8);
        stp_buf.push(0x00);
        stp_buf.extend_from_slice(b"CRT\x00\x00STP");
        dev.write(&stp_buf).map_err(|e| format!("STP write error: {e}"))?;

        Ok(())
    }

    /// Read raw input frame with timeout
    pub fn read_input(&self, timeout_ms: i32) -> Result<Option<(u8, u8)>, String> {
        let mut buf = [0u8; 512];
        let n = {
            let dev = self.device.lock().map_err(|e| format!("Mutex lock error: {e}"))?;
            dev.read_timeout(&mut buf, timeout_ms)
                .map_err(|e| format!("HID read error: {e}"))?
        };

        if n >= 11 {
            // Check prefix ACK\0\0OK\0\0
            if buf[0] == 0x41 && buf[1] == 0x43 && buf[2] == 0x4B {
                let slot = buf[9];
                let state = buf[10];
                // Translate slot back to 1-based UI key
                if let Some(ui_key) = slot_to_ui_key(slot) {
                    return Ok(Some((ui_key, state)));
                }
            }
        }
        Ok(None)
    }

    #[allow(dead_code)]
    pub fn device_handle(&self) -> Arc<Mutex<HidDevice>> {
        self.device.clone()
    }
}

/// Translates hardware BAT slot (1..15) back to 1-based UI key (1..15).
fn slot_to_ui_key(slot: u8) -> Option<u8> {
    for (ui_idx, &s) in UI_INDEX_TO_SLOT.iter().enumerate() {
        if s == slot {
            return Some((ui_idx + 1) as u8);
        }
    }
    None
}
