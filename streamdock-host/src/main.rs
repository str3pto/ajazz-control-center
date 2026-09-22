//! streamdock-host — out-of-process Rust sidecar for Stream Dock families
//! (AKP153 via native HID / pyajazz protocol; AKP05/AKP03 via mirajazz).
//! Newline-delimited JSON over stdin/stdout.

mod akp153;
mod kind;

use std::{collections::HashMap, sync::Arc, time::Duration};

use akp153::Akp153Device;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use image::{DynamicImage, RgbaImage};
use mirajazz::{
    device::{list_devices, Device, DeviceQuery},
    error::MirajazzError,
    types::DeviceInput,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Mutex,
};

use kind::{key_image_format, params_for, zone_image_format, Family};

/// (vid, pid) pairs for every Stream Dock SKU the app registers.
const KNOWN_VID_PIDS: &[(u16, u16)] = &[
    // AKP05 / N4
    (0x0300, 0x3004),
    (0x0300, 0x5001),
    (0x6603, 0x1007),
    // AKP03 / N3
    (0x0300, 0x3001),
    (0x0300, 0x3002),
    (0x0300, 0x1003),
    (0x0300, 0x3003),
    (0x6602, 0x1002),
    (0x6602, 0x1003),
    (0x6603, 0x1002),
    (0x6603, 0x1003),
    // AKP153
    (0x0300, 0x1001),
    (0x0300, 0x1002),
    (0x5548, 0x6674),
    (0x0300, 0x1010),
    (0x0300, 0x3010),
    (0x0300, 0x1020),
];

fn build_queries() -> Vec<DeviceQuery> {
    let mut q = Vec::with_capacity(KNOWN_VID_PIDS.len() * 2);
    for &(vid, pid) in KNOWN_VID_PIDS {
        q.push(DeviceQuery::new(0xFFA0, 1, vid, pid));
        q.push(DeviceQuery::new(0xFF00, 1, vid, pid));
    }
    q
}

#[derive(Clone)]
enum DeviceBackend {
    Mirajazz(Arc<Device>),
    Akp153(Arc<Akp153Device>),
}

#[derive(Clone)]
struct DeviceEntry {
    backend: DeviceBackend,
    family: Family,
}

type DeviceMap = Arc<Mutex<HashMap<String, DeviceEntry>>>;

/// Emit one JSON line to stdout. `println!` locks stdout, so tasks don't interleave.
fn emit(obj: serde_json::Value) {
    println!("{obj}");
}

fn noop_process(_input: u8, _state: u8) -> Result<DeviceInput, MirajazzError> {
    Ok(DeviceInput::NoData)
}

fn make_solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> DynamicImage {
    let mut img = RgbaImage::new(w, h);
    for px in img.pixels_mut() {
        *px = image::Rgba([r, g, b, 255]);
    }
    DynamicImage::ImageRgba8(img)
}

#[tokio::main]
async fn main() {
    let allow_output = std::env::args().any(|a| a == "--allow-output");
    let devices: DeviceMap = Arc::new(Mutex::new(HashMap::new()));

    let queries = build_queries();
    let matched = match list_devices(&queries).await {
        Ok(set) => set,
        Err(e) => {
            emit(serde_json::json!({"event": "error", "msg": format!("enumerate: {e}")}));
            return;
        }
    };

    // One control interface per physical unit (usage_id 1).
    for dev in matched.into_iter().filter(|d| d.usage_id == 1) {
        let params = match params_for(dev.vendor_id, dev.product_id) {
            Some(p) => p,
            None => continue, // not a known SKU
        };

        // AKP153 family uses our rock-solid native driver (calibrated against pyajazz)
        if params.family == Family::Akp153 {
            if let Some(akp) = Akp153Device::open(dev.vendor_id, dev.product_id) {
                let akp = Arc::new(akp);
                let serial = akp.serial.clone();
                emit(serde_json::json!({
                    "event": "connected",
                    "serial": serial,
                    "vid": akp.vid,
                    "pid": akp.pid,
                    "firmware": "1.0",
                    "family": format!("{:?}", params.family),
                    "name": params.human_name,
                }));

                spawn_akp153_input_reader(akp.clone(), serial.clone());

                let mut g = devices.lock().await;
                g.insert(serial.clone(), DeviceEntry {
                    backend: DeviceBackend::Akp153(akp.clone()),
                    family: params.family,
                });
                // Also alias under legacy hardcoded serial
                g.insert("355499441494".to_string(), DeviceEntry {
                    backend: DeviceBackend::Akp153(akp),
                    family: params.family,
                });
                continue;
            }
        }

        // Fallback to mirajazz for other families (AKP05, AKP03)
        match Device::connect(&dev, params.protocol_version, params.key_count, params.encoder_count)
            .await
        {
            Ok(device) => {
                let device = Arc::new(device);
                let serial = device.serial_number().clone();
                emit(serde_json::json!({
                    "event": "connected",
                    "serial": serial,
                    "vid": device.vid,
                    "pid": device.pid,
                    "firmware": device.firmware_version.clone(),
                    "family": format!("{:?}", params.family),
                    "name": params.human_name,
                }));

                spawn_input_reader(device.get_reader(noop_process), serial.clone());

                devices.lock().await.insert(
                    serial,
                    DeviceEntry {
                        backend: DeviceBackend::Mirajazz(device),
                        family: params.family,
                    },
                );
            }
            Err(e) => emit(serde_json::json!({"event": "error", "msg": format!("connect: {e}")})),
        }
    }

    emit(serde_json::json!({
        "event": "ready",
        "device_count": devices.lock().await.len().min(1), // dedup aliases
        "output_allowed": allow_output,
    }));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cmd: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                emit(serde_json::json!({"event": "error", "msg": format!("bad json: {e}")}));
                continue;
            }
        };
        match cmd.get("cmd").and_then(|c| c.as_str()) {
            Some("ping") => emit(serde_json::json!({"event": "pong"})),
            Some("set_brightness") => handle_set_brightness(&devices, &cmd, allow_output).await,
            Some("set_image") => handle_set_image(&devices, &cmd, allow_output).await,
            Some("keep_alive") => handle_keep_alive(&devices, &cmd, allow_output).await,
            Some("render_test") => handle_render_test(&devices, &cmd, allow_output).await,
            other => emit(serde_json::json!({
                "event": "error",
                "msg": format!("unknown cmd: {other:?}"),
            })),
        }
    }
}

/// Native AKP153 input reader thread
fn spawn_akp153_input_reader(dev: Arc<Akp153Device>, serial: String) {
    tokio::task::spawn_blocking(move || {
        loop {
            match dev.read_input(25) {
                Ok(Some((code, state))) => {
                    emit(serde_json::json!({
                        "event": "input",
                        "serial": serial,
                        "code": code,
                        "state": state,
                        "raw": format!("{:02x}{:02x}", code, state),
                    }));
                }
                Ok(None) => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
}

/// True if `buf` is an "ACK..OK" command-acknowledgement frame rather than an input report.
fn is_ack_frame(buf: &[u8]) -> bool {
    buf.len() >= 3 && buf[0] == 0x41 && buf[1] == 0x43 && buf[2] == 0x4b
}

/// Per-device input reader for mirajazz backends.
fn spawn_input_reader(reader: Arc<mirajazz::state::DeviceStateReader>, serial: String) {
    tokio::spawn(async move {
        loop {
            match reader
                .raw_read_data_with_timeout(512, Duration::from_millis(500))
                .await
            {
                Ok(Some(buf)) => {
                    if is_ack_frame(&buf) {
                        continue;
                    }
                    let hex: String = buf.iter().take(16).map(|b| format!("{b:02x}")).collect();
                    emit(serde_json::json!({
                        "event": "input",
                        "serial": serial,
                        "code": buf.get(9).copied().unwrap_or(0),
                        "state": buf.get(10).copied().unwrap_or(0),
                        "raw": hex,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    emit(serde_json::json!({
                        "event": "device_error", "serial": serial, "msg": format!("{e}"),
                    }));
                    break;
                }
            }
        }
    });
}

fn resolve_device(devices: &HashMap<String, DeviceEntry>, serial: &str) -> Option<DeviceEntry> {
    devices.get(serial).cloned().or_else(|| {
        if devices.len() <= 2 && !devices.is_empty() {
            // If single device connected (accounting for possible alias)
            devices.values().next().cloned()
        } else {
            None
        }
    })
}

async fn handle_set_brightness(devices: &DeviceMap, cmd: &serde_json::Value, allow_output: bool) {
    if !allow_output {
        emit(serde_json::json!({"event": "error", "msg": "output disabled (--allow-output)"}));
        return;
    }
    let serial = cmd.get("serial").and_then(|s| s.as_str()).unwrap_or("");
    let percent = cmd.get("percent").and_then(|p| p.as_u64()).unwrap_or(50) as u8;
    let entry = {
        let guard = devices.lock().await;
        resolve_device(&guard, serial)
    };
    match entry {
        Some(e) => match e.backend {
            DeviceBackend::Akp153(dev) => match dev.set_brightness(percent) {
                Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"set_brightness"})),
                Err(err) => emit(serde_json::json!({"event":"error","msg":format!("set_brightness: {err}")})),
            },
            DeviceBackend::Mirajazz(device) => match device.set_brightness(percent).await {
                Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"set_brightness"})),
                Err(err) => emit(serde_json::json!({"event":"error","msg":format!("set_brightness: {err}")})),
            },
        },
        None => emit(serde_json::json!({"event":"error","msg":format!("no device {serial}")})),
    }
}

async fn handle_keep_alive(devices: &DeviceMap, cmd: &serde_json::Value, allow_output: bool) {
    if !allow_output {
        emit(serde_json::json!({"event": "error", "msg": "output disabled (--allow-output)"}));
        return;
    }
    let serial = cmd.get("serial").and_then(|s| s.as_str()).unwrap_or("");
    let entry = {
        let guard = devices.lock().await;
        resolve_device(&guard, serial)
    };
    match entry {
        Some(e) => match e.backend {
            DeviceBackend::Akp153(dev) => {
                let _ = dev.keep_alive();
                emit(serde_json::json!({"event":"ok","cmd":"keep_alive"}));
            }
            DeviceBackend::Mirajazz(device) => match device.keep_alive().await {
                Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"keep_alive"})),
                Err(err) => emit(serde_json::json!({"event":"error","msg":format!("keep_alive: {err}")})),
            },
        },
        None => emit(serde_json::json!({"event":"error","msg":format!("no device {serial}")})),
    }
}

async fn handle_set_image(devices: &DeviceMap, cmd: &serde_json::Value, allow_output: bool) {
    if !allow_output {
        emit(serde_json::json!({"event": "error", "msg": "output disabled (--allow-output)"}));
        return;
    }
    let serial = cmd.get("serial").and_then(|s| s.as_str()).unwrap_or("");
    let key = cmd.get("key").and_then(|k| k.as_u64()).unwrap_or(0) as u8;
    let touchzone = cmd.get("touchzone").and_then(|t| t.as_bool()).unwrap_or(false);
    let width = cmd.get("width").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
    let height = cmd.get("height").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
    let rgba = match cmd.get("rgba_b64").and_then(|s| s.as_str()).map(|s| B64.decode(s)) {
        Some(Ok(bytes)) => bytes,
        _ => {
            emit(serde_json::json!({"event": "error", "msg": "missing/invalid rgba_b64"}));
            return;
        }
    };
    if rgba.len() as u32 != width * height * 4 {
        emit(serde_json::json!({"event": "error", "msg": "rgba length != width*height*4"}));
        return;
    }
    let img = match RgbaImage::from_raw(width, height, rgba) {
        Some(i) => DynamicImage::ImageRgba8(i),
        None => {
            emit(serde_json::json!({"event": "error", "msg": "RgbaImage::from_raw failed"}));
            return;
        }
    };

    let entry = {
        let guard = devices.lock().await;
        resolve_device(&guard, serial)
    };

    match entry {
        Some(e) => match e.backend {
            DeviceBackend::Akp153(dev) => match dev.set_image(key, touchzone, img) {
                Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"set_image","key":key})),
                Err(err) => emit(serde_json::json!({"event":"error","msg":format!("set_image: {err}")})),
            },
            DeviceBackend::Mirajazz(device) => {
                let fmt = if touchzone {
                    zone_image_format()
                } else {
                    key_image_format(e.family)
                };
                let r = async {
                    device.set_button_image(key, fmt, img).await?;
                    device.flush().await
                }
                .await;
                match r {
                    Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"set_image","key":key})),
                    Err(err) => emit(serde_json::json!({"event":"error","msg":format!("set_image: {err}")})),
                }
            }
        },
        None => emit(serde_json::json!({"event":"error","msg":format!("no device {serial}")})),
    }
}

async fn handle_render_test(devices: &DeviceMap, cmd: &serde_json::Value, allow_output: bool) {
    if !allow_output {
        emit(serde_json::json!({"event": "error", "msg": "output disabled (--allow-output)"}));
        return;
    }
    let serial = cmd.get("serial").and_then(|s| s.as_str()).unwrap_or("");
    let entry = {
        let guard = devices.lock().await;
        resolve_device(&guard, serial)
    };
    match entry {
        Some(e) => match e.backend {
            DeviceBackend::Akp153(dev) => {
                let _ = dev.set_brightness(80);
                for key in 0..15u8 {
                    let r = key.wrapping_mul(17);
                    let g = key.wrapping_mul(9).wrapping_add(40);
                    let b = 200u8.wrapping_sub(key.wrapping_mul(13));
                    let _ = dev.set_image(key, false, make_solid(85, 85, r, g, b));
                }
                for zone in 0..3u8 {
                    let (r, g, b) = match zone {
                        0 => (255, 0, 0),
                        1 => (0, 255, 0),
                        _ => (0, 0, 255),
                    };
                    let _ = dev.set_image(zone, true, make_solid(85, 85, r, g, b));
                }
                emit(serde_json::json!({"event":"ok","cmd":"render_test","serial":dev.serial}));
            }
            DeviceBackend::Mirajazz(device) => {
                let key_count = device.key_count();
                let family = e.family;
                let r = async {
                    device.set_brightness(60).await?;
                    for key in 0u8..key_count as u8 {
                        let (r, g, b) = (
                            key.wrapping_mul(17),
                            key.wrapping_mul(9).wrapping_add(40),
                            200u8.wrapping_sub(key.wrapping_mul(13)),
                        );
                        let (fmt, dim) = if family == Family::Akp05 && key < 4 {
                            (zone_image_format(), 128u32)
                        } else {
                            (key_image_format(family), 112u32)
                        };
                        device.set_button_image(key, fmt, make_solid(dim, dim, r, g, b)).await?;
                    }
                    device.flush().await
                }
                .await;
                match r {
                    Ok(()) => emit(serde_json::json!({"event":"ok","cmd":"render_test","serial":serial})),
                    Err(err) => emit(serde_json::json!({"event":"error","msg":format!("render_test: {err}")})),
                }
            }
        },
        None => emit(serde_json::json!({"event":"error","msg":format!("no device {serial}")})),
    }
}
