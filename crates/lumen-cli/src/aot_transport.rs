//! Serial transport for the shared native install frames.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lumen_common::aot::install::{self, Kind, Receiver};
use lumen_common::aot::NativeContainer;
use lumen_common::target::TargetSpec;

const MAX_PAYLOAD: usize = install::MAX_DEVICE_PAYLOAD;
const MAX_REPLY: usize = 1024;

pub fn target_from_reference(reference: &str) -> Result<TargetSpec, String> {
    let port_name = if reference == "device" {
        std::env::var("LUMEN_AOT_DEVICE").map_err(|_| "@device requires LUMEN_AOT_DEVICE")?
    } else if reference.is_empty() {
        return Err("device target requires a serial port after @".into());
    } else {
        reference.to_owned()
    };
    let mut port = open(&port_name, 115_200)?;
    query_target(port.as_mut()).map(|(target, _)| target)
}

pub fn target_device(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: lumen-cli target-device PORT TARGET_FILE [--baud RATE]");
        return Ok(());
    }
    let (port_name, output, baud) = parse_target_args(args, "target-device")?;
    let mut port = open(port_name, baud)?;
    let (target, bytes) = query_target(port.as_mut())?;
    std::fs::write(output, bytes).map_err(|error| format!("{output}: {error}"))?;
    println!(
        "target: {:?} {:?}, profile {:?}, native fingerprint {:016x}",
        target.arch, target.abi, target.profile, target.native_fp
    );
    Ok(())
}

pub fn inventory(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: lumen-cli inventory PORT INVENTORY_FILE [--baud RATE]");
        return Ok(());
    }
    let (port_name, output, baud) = parse_target_args(args, "inventory")?;
    let mut port = open(port_name, baud)?;
    let query = install::encode(Kind::InventoryQuery, &[], None)?;
    write_frame(port.as_mut(), &query)?;
    let bytes = read_frame(
        port.as_mut(),
        Duration::from_secs(10),
        install::MAX_INVENTORY_PAYLOAD,
    )?;
    let frame = install::decode(&bytes, install::MAX_INVENTORY_PAYLOAD)?;
    if frame.kind == Kind::Result {
        return Err(format!(
            "device rejected inventory query: {}",
            std::str::from_utf8(frame.payload).map_err(|_| "invalid device result")?
        ));
    }
    if frame.kind != Kind::Inventory {
        return Err("device did not send native app inventory".into());
    }
    let entries = install::decode_inventory(frame.payload)?;
    std::fs::write(output, frame.payload).map_err(|error| format!("{output}: {error}"))?;
    for entry in entries {
        println!(
            "{}\t{}\t{:016x}\t{}",
            entry.name,
            if entry.stale { "stale" } else { "current" },
            entry.native_fp,
            lumen_common::codec::hex_encode(&entry.source_hash),
        );
    }
    Ok(())
}

pub fn upload(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: lumen-cli upload PORT APP.lumup [--baud RATE] [--target-out TARGET_FILE]");
        return Ok(());
    }
    let (port_name, frame_path, baud, target_out) = parse_upload_args(args)?;
    if target_out == Some(frame_path)
        || target_out.is_some_and(|path| {
            std::fs::canonicalize(path).ok() == std::fs::canonicalize(frame_path).ok()
        })
    {
        return Err("target output must differ from the install frame".into());
    }
    let limit = MAX_PAYLOAD + install::HEADER_LEN + 64;
    let file = std::fs::File::open(frame_path).map_err(|error| format!("{frame_path}: {error}"))?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{frame_path}: {error}"))?;
    if bytes.len() > limit {
        return Err("install frame exceeds the device limit".into());
    }
    let frame = install::decode(&bytes, MAX_PAYLOAD)?;
    if frame.kind != Kind::Install {
        return Err("upload requires an INSTALL frame".into());
    }
    let app = install::decode_install_payload(frame.payload)?;

    let mut port = open(port_name, baud)?;
    let (target, target_bytes) = query_target(port.as_mut())?;
    if let Some(path) = target_out {
        std::fs::write(path, target_bytes).map_err(|error| format!("{path}: {error}"))?;
    }
    NativeContainer::parse(app.blob)?.mapping_len(&target)?;
    write_frame(port.as_mut(), &bytes)?;
    let reply = read_frame(port.as_mut(), Duration::from_secs(60), MAX_REPLY)?;
    let reply = install::decode(&reply, MAX_REPLY)?;
    if reply.kind != Kind::Result {
        return Err("device did not send an install result".into());
    }
    if !reply.payload.is_empty() {
        return Err(format!(
            "device rejected {}: {}",
            app.name,
            std::str::from_utf8(reply.payload).map_err(|_| "invalid device result")?
        ));
    }
    println!("stored {}", app.name);
    Ok(())
}

fn parse_target_args<'a>(
    args: &'a [String],
    command: &str,
) -> Result<(&'a str, &'a str, u32), String> {
    let mut positional = Vec::new();
    let mut baud = 115_200;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baud" => {
                baud = iter
                    .next()
                    .ok_or("--baud requires a rate")?
                    .parse()
                    .map_err(|_| "invalid baud rate")?;
                if baud == 0 {
                    return Err("baud rate must be nonzero".into());
                }
            }
            _ if arg.starts_with('-') => return Err(format!("unknown device option: {arg}")),
            _ => positional.push(arg.as_str()),
        }
    }
    match positional.as_slice() {
        [port, output] => Ok((port, output, baud)),
        _ => Err(format!(
            "usage: lumen-cli {command} PORT OUTPUT_FILE [--baud RATE]"
        )),
    }
}

fn parse_upload_args(args: &[String]) -> Result<(&str, &str, u32, Option<&str>), String> {
    let mut positional = Vec::new();
    let mut baud = 115_200;
    let mut target_out = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baud" => {
                baud = iter
                    .next()
                    .ok_or("--baud requires a rate")?
                    .parse()
                    .map_err(|_| "invalid baud rate")?;
                if baud == 0 {
                    return Err("baud rate must be nonzero".into());
                }
            }
            "--target-out" => {
                target_out = Some(iter.next().ok_or("--target-out requires a file")?.as_str())
            }
            _ if arg.starts_with('-') => return Err(format!("unknown upload option: {arg}")),
            _ => positional.push(arg.as_str()),
        }
    }
    match positional.as_slice() {
        [port, frame] => Ok((port, frame, baud, target_out)),
        _ => Err(
            "usage: lumen-cli upload PORT APP.lumup [--baud RATE] [--target-out TARGET_FILE]"
                .into(),
        ),
    }
}

fn open(path: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>, String> {
    serialport::new(path, baud)
        .timeout(Duration::from_millis(250))
        .open()
        .map_err(|error| format!("{path}: {error}"))
}

fn query_target(
    port: &mut dyn serialport::SerialPort,
) -> Result<(TargetSpec, [u8; TargetSpec::ENCODED_LEN]), String> {
    let hello = install::encode(Kind::Hello, &[], None)?;
    write_frame(port, &hello)?;
    let bytes = read_frame(port, Duration::from_secs(10), MAX_REPLY)?;
    let frame = install::decode(&bytes, MAX_REPLY)?;
    if frame.kind == Kind::Result {
        return Err(format!(
            "device rejected target query: {}",
            std::str::from_utf8(frame.payload).map_err(|_| "invalid device result")?
        ));
    }
    if frame.kind != Kind::Target {
        return Err("device did not send a target description".into());
    }
    let encoded: [u8; TargetSpec::ENCODED_LEN] = frame
        .payload
        .try_into()
        .map_err(|_| "invalid device target length")?;
    let target = TargetSpec::decode(&encoded)?;
    Ok((target, encoded))
}

fn write_frame(port: &mut dyn serialport::SerialPort, bytes: &[u8]) -> Result<(), String> {
    for chunk in bytes.chunks(512) {
        port.write_all(chunk)
            .map_err(|error| format!("serial write: {error}"))?;
    }
    port.flush()
        .map_err(|error| format!("serial flush: {error}"))
}

fn read_frame(
    port: &mut dyn serialport::SerialPort,
    timeout: Duration,
    max_payload: usize,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut matched = 0;
    let mut receiver = Receiver::new(max_payload);
    loop {
        let byte = read_byte(port, deadline)?;
        if matched < install::MAGIC.len() {
            matched = if byte == install::MAGIC[matched] {
                matched + 1
            } else if byte == install::MAGIC[0] {
                1
            } else {
                0
            };
            if matched == install::MAGIC.len() {
                receiver.feed(install::MAGIC)?;
            }
            continue;
        }
        receiver.feed(&[byte])?;
        if receiver.frame().is_some() {
            return receiver.finish().map_err(str::to_owned);
        }
    }
}

fn read_byte(port: &mut dyn serialport::SerialPort, deadline: Instant) -> Result<u8, String> {
    let mut byte = [0];
    loop {
        if Instant::now() >= deadline {
            return Err("device response timed out".into());
        }
        match port.read(&mut byte) {
            Ok(1) => return Ok(byte[0]),
            Ok(_) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                ()
            }
            Err(error) => return Err(format!("serial read: {error}")),
        }
    }
}
