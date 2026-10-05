//! Length-bounded native install frames shared by host and device.

pub const MAGIC: &[u8; 8] = b"LUMUP001";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 20;
/// Current device install limit; transports can impose a smaller cap.
pub const MAX_DEVICE_PAYLOAD: usize = 16 << 20;
pub const SIGNATURE_LEN: usize = 64;
const APP_MAGIC: &[u8; 8] = b"LUMAPP01";
const APP_HEADER_LEN: usize = 48;
pub const MAX_APP_NAME_LEN: usize = 64;
const SLOT_MAGIC: &[u8; 8] = b"LUMSLT01";
const SLOT_HEADER_LEN: usize = 24;
const INVENTORY_MAGIC: &[u8; 8] = b"LUMINV01";
const INVENTORY_ENTRY_LEN: usize = 50;
pub const MAX_INVENTORY_PAYLOAD: usize = 64 << 10;

/// One authenticated stored app. `stale` means its native target no longer
/// matches the running firmware; its source hash lets the host rebuild it.
pub struct InventoryEntry<'a> {
    pub name: &'a str,
    pub generation: u64,
    pub native_fp: u64,
    pub source_hash: [u8; 32],
    pub stale: bool,
}

pub fn encode_inventory(entries: &[InventoryEntry<'_>]) -> Result<Vec<u8>, &'static str> {
    let count = u16::try_from(entries.len()).map_err(|_| "too many native apps")?;
    let mut out = Vec::new();
    out.extend_from_slice(INVENTORY_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    let mut previous = "";
    for entry in entries {
        validate_app_name(entry.name)?;
        if entry.name <= previous || entry.generation == 0 {
            return Err("native inventory is not sorted");
        }
        previous = entry.name;
        out.push(entry.name.len() as u8);
        out.push(u8::from(entry.stale));
        out.extend_from_slice(&entry.generation.to_le_bytes());
        out.extend_from_slice(&entry.native_fp.to_le_bytes());
        out.extend_from_slice(&entry.source_hash);
        out.extend_from_slice(entry.name.as_bytes());
        if out.len() > MAX_INVENTORY_PAYLOAD {
            return Err("native inventory too large");
        }
    }
    Ok(out)
}

pub fn decode_inventory(bytes: &[u8]) -> Result<Vec<InventoryEntry<'_>>, &'static str> {
    if bytes.len() < 10 || bytes.len() > MAX_INVENTORY_PAYLOAD || &bytes[..8] != INVENTORY_MAGIC {
        return Err("invalid native inventory header");
    }
    let count = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as usize;
    if count > (bytes.len() - 10) / INVENTORY_ENTRY_LEN {
        return Err("truncated native inventory");
    }
    let mut entries = Vec::with_capacity(count);
    let mut at = 10usize;
    for _ in 0..count {
        if at
            .checked_add(INVENTORY_ENTRY_LEN)
            .is_none_or(|end| end > bytes.len())
        {
            return Err("truncated native inventory");
        }
        let name_len = bytes[at] as usize;
        let stale = match bytes[at + 1] {
            0 => false,
            1 => true,
            _ => return Err("invalid native inventory flags"),
        };
        let generation = u64::from_le_bytes(bytes[at + 2..at + 10].try_into().unwrap());
        let native_fp = u64::from_le_bytes(bytes[at + 10..at + 18].try_into().unwrap());
        let source_hash = bytes[at + 18..at + 50].try_into().unwrap();
        at += INVENTORY_ENTRY_LEN;
        let end = at
            .checked_add(name_len)
            .filter(|end| *end <= bytes.len())
            .ok_or("truncated native inventory name")?;
        let name = std::str::from_utf8(&bytes[at..end]).map_err(|_| "invalid native app name")?;
        validate_app_name(name)?;
        if generation == 0
            || entries
                .last()
                .is_some_and(|last: &InventoryEntry<'_>| last.name >= name)
        {
            return Err("invalid native inventory order or generation");
        }
        entries.push(InventoryEntry {
            name,
            generation,
            native_fp,
            source_hash,
            stale,
        });
        at = end;
    }
    if at != bytes.len() {
        return Err("trailing native inventory data");
    }
    Ok(entries)
}

/// One crash-tolerant app slot. A writer updates the inactive slot; readers
/// select the highest valid generation. A torn write cannot replace the old
/// valid slot.
pub struct Slot<'a> {
    pub generation: u64,
    pub frame: &'a [u8],
}

pub fn encode_slot(
    generation: u64,
    frame: &[u8],
    max_payload: usize,
) -> Result<Vec<u8>, &'static str> {
    if generation == 0 || decode(frame, max_payload)?.kind != Kind::Install {
        return Err("invalid native app slot");
    }
    let len = u32::try_from(frame.len()).map_err(|_| "native app slot too large")?;
    let capacity = SLOT_HEADER_LEN
        .checked_add(frame.len())
        .ok_or("native app slot too large")?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(SLOT_MAGIC);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(frame);
    let crc = crc32(&out[..20], frame);
    out[20..24].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

pub fn decode_slot(bytes: &[u8], max_payload: usize) -> Result<Slot<'_>, &'static str> {
    if bytes.len() < SLOT_HEADER_LEN || &bytes[..8] != SLOT_MAGIC {
        return Err("invalid native app slot header");
    }
    let generation = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    if generation == 0 || SLOT_HEADER_LEN.checked_add(len) != Some(bytes.len()) {
        return Err("invalid native app slot length");
    }
    let frame = &bytes[SLOT_HEADER_LEN..];
    let expected = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    if crc32(&bytes[..20], frame) != expected || decode(frame, max_payload)?.kind != Kind::Install {
        return Err("invalid native app slot contents");
    }
    Ok(Slot { generation, frame })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Target = 2,
    Install = 3,
    /// Empty payload means success; nonempty UTF-8 is a rejection reason.
    Result = 4,
    InventoryQuery = 5,
    Inventory = 6,
}

pub struct Frame<'a> {
    pub kind: Kind,
    pub payload: &'a [u8],
    pub signature: Option<&'a [u8; SIGNATURE_LEN]>,
}

/// The source hash identifies an app's exact declared source set. Engine
/// compatibility is checked separately against the blob's target fingerprint.
/// The name is a single VFS component, never a path supplied by the uploader.
pub struct InstallPayload<'a> {
    pub name: &'a str,
    pub source_hash: [u8; 32],
    pub blob: &'a [u8],
}

pub fn encode_install_payload(
    name: &str,
    source_hash: [u8; 32],
    blob: &[u8],
) -> Result<Vec<u8>, &'static str> {
    validate_app_name(name)?;
    super::NativeContainer::parse(blob)?;
    let blob_len = u32::try_from(blob.len()).map_err(|_| "native app too large")?;
    let total = APP_HEADER_LEN
        .checked_add(name.len())
        .and_then(|n| n.checked_add(blob.len()))
        .ok_or("native app too large")?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(APP_MAGIC);
    out.push(name.len() as u8);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&blob_len.to_le_bytes());
    out.extend_from_slice(&source_hash);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(blob);
    Ok(out)
}

pub fn decode_install_payload(payload: &[u8]) -> Result<InstallPayload<'_>, &'static str> {
    if payload.len() < APP_HEADER_LEN || &payload[..8] != APP_MAGIC || payload[9..12] != [0; 3] {
        return Err("invalid native app header");
    }
    let name_len = payload[8] as usize;
    let blob_len = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as usize;
    let blob_start = APP_HEADER_LEN
        .checked_add(name_len)
        .ok_or("native app too large")?;
    if name_len == 0 || blob_start.checked_add(blob_len) != Some(payload.len()) {
        return Err("invalid native app length");
    }
    let name = std::str::from_utf8(&payload[APP_HEADER_LEN..blob_start])
        .map_err(|_| "invalid native app name")?;
    validate_app_name(name)?;
    let blob = &payload[blob_start..];
    super::NativeContainer::parse(blob)?;
    Ok(InstallPayload {
        name,
        source_hash: payload[16..48].try_into().unwrap(),
        blob,
    })
}

/// Validate one complete INSTALL frame for the running native target before
/// storing or mapping its blob. `verify` checks a detached signature over the
/// blob (Ed25519 against a key allow-list in release builds); an unsigned frame
/// is accepted only when `allow_unsigned` is set.
pub fn authorize_install<'a>(
    bytes: &'a [u8],
    max_payload: usize,
    target: &crate::target::TargetSpec,
    allow_unsigned: bool,
    verify: impl FnOnce(&[u8], &[u8; SIGNATURE_LEN]) -> Result<(), &'static str>,
) -> Result<InstallPayload<'a>, &'static str> {
    let app = authenticate_install(bytes, max_payload, allow_unsigned, verify)?;
    super::NativeContainer::parse(app.blob)?.mapping_len(target)?;
    Ok(app)
}

/// Authenticate a stored INSTALL frame without requiring the old target to
/// match the current firmware. Used to report apps requiring a host rebuild.
pub fn authenticate_install<'a>(
    bytes: &'a [u8],
    max_payload: usize,
    allow_unsigned: bool,
    verify: impl FnOnce(&[u8], &[u8; SIGNATURE_LEN]) -> Result<(), &'static str>,
) -> Result<InstallPayload<'a>, &'static str> {
    let frame = decode(bytes, max_payload)?;
    if frame.kind != Kind::Install {
        return Err("expected native install frame");
    }
    let app = decode_install_payload(frame.payload)?;
    match frame.signature {
        Some(signature) => verify(app.blob, signature)?,
        None if !allow_unsigned => return Err("native app signature required"),
        None => (),
    }
    Ok(app)
}

fn validate_app_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || name.len() > MAX_APP_NAME_LEN
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || name == "."
        || name == ".."
    {
        return Err("invalid native app name");
    }
    Ok(())
}

pub fn encode(
    kind: Kind,
    payload: &[u8],
    signature: Option<&[u8; SIGNATURE_LEN]>,
) -> Result<Vec<u8>, &'static str> {
    validate_shape(kind, payload, signature.is_some())?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| "install payload too large")?;
    let len = HEADER_LEN
        .checked_add(payload.len())
        .and_then(|n| {
            n.checked_add(if signature.is_some() {
                SIGNATURE_LEN
            } else {
                0
            })
        })
        .ok_or("install frame too large")?;
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(kind as u8);
    out.push(u8::from(signature.is_some()));
    out.push(0);
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    if let Some(signature) = signature {
        out.extend_from_slice(signature);
    }
    out.extend_from_slice(payload);
    let crc = crc32(&out[..16], &out[HEADER_LEN..]);
    out[16..20].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

pub fn decode(bytes: &[u8], max_payload: usize) -> Result<Frame<'_>, &'static str> {
    let expected_len = frame_len(bytes, max_payload)?;
    if bytes.len() != expected_len {
        return Err("invalid install frame length");
    }
    let kind = match bytes[9] {
        1 => Kind::Hello,
        2 => Kind::Target,
        3 => Kind::Install,
        4 => Kind::Result,
        5 => Kind::InventoryQuery,
        6 => Kind::Inventory,
        _ => unreachable!(),
    };
    let signed = bytes[10] == 1;
    let extra = if signed { SIGNATURE_LEN } else { 0 };
    let expected_crc = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if crc32(&bytes[..16], &bytes[HEADER_LEN..]) != expected_crc {
        return Err("install frame CRC mismatch");
    }
    let signature: Option<&[u8; SIGNATURE_LEN]> = signed.then(|| {
        bytes[HEADER_LEN..HEADER_LEN + SIGNATURE_LEN]
            .try_into()
            .unwrap()
    });
    let payload = &bytes[HEADER_LEN + extra..];
    validate_shape(kind, payload, signed)?;
    Ok(Frame {
        kind,
        payload,
        signature,
    })
}

/// Inspect the fixed header before allocating a receive buffer. Callers must
/// still pass the complete frame to `decode` for CRC and payload validation.
pub fn frame_len(header: &[u8], max_payload: usize) -> Result<usize, &'static str> {
    if header.len() < HEADER_LEN || &header[..8] != MAGIC || header[8] != VERSION || header[11] != 0
    {
        return Err("invalid install frame header");
    }
    match header[9] {
        1..=6 => (),
        _ => return Err("unknown install frame kind"),
    };
    let signed = match header[10] {
        0 => false,
        1 => true,
        _ => return Err("unsupported install frame flags"),
    };
    if signed && header[9] != Kind::Install as u8 {
        return Err("signature on non-install frame");
    }
    let payload_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if (header[9] == Kind::Hello as u8 && payload_len != 0)
        || (header[9] == Kind::InventoryQuery as u8 && payload_len != 0)
        || (header[9] == Kind::Target as u8
            && payload_len != crate::target::TargetSpec::ENCODED_LEN)
        || (header[9] == Kind::Install as u8 && payload_len <= APP_HEADER_LEN)
        || (header[9] == Kind::Inventory as u8
            && (payload_len < 10 || payload_len > MAX_INVENTORY_PAYLOAD))
    {
        return Err("invalid install frame payload length");
    }
    let extra = if signed { SIGNATURE_LEN } else { 0 };
    if payload_len > max_payload {
        return Err("install payload too large");
    }
    HEADER_LEN
        .checked_add(extra)
        .and_then(|n| n.checked_add(payload_len))
        .ok_or("invalid install frame length")
}

/// Incremental bounded receive buffer for a serial or USB byte stream.
/// The header is checked before reserving space for the payload. `feed`
/// consumes at most one frame; call `reset` before feeding the next one.
pub struct Receiver {
    bytes: Vec<u8>,
    wanted: usize,
    max_payload: usize,
}

impl Receiver {
    pub fn new(max_payload: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(HEADER_LEN),
            wanted: HEADER_LEN,
            max_payload,
        }
    }

    /// Return the number of input bytes consumed toward the current frame.
    pub fn feed(&mut self, input: &[u8]) -> Result<usize, &'static str> {
        let mut consumed = 0;
        while consumed < input.len() && self.bytes.len() < self.wanted {
            let n = (input.len() - consumed).min(self.wanted - self.bytes.len());
            self.bytes
                .try_reserve(n)
                .map_err(|_| "cannot allocate install frame")?;
            self.bytes.extend_from_slice(&input[consumed..consumed + n]);
            consumed += n;
            if self.bytes.len() == HEADER_LEN && self.wanted == HEADER_LEN {
                match frame_len(&self.bytes, self.max_payload) {
                    Ok(wanted) => self.wanted = wanted,
                    Err(error) => {
                        self.reset();
                        return Err(error);
                    }
                }
            }
        }
        Ok(consumed)
    }

    /// A complete frame is decoded only after its CRC and payload pass validation.
    pub fn frame(&self) -> Option<Result<Frame<'_>, &'static str>> {
        (self.bytes.len() == self.wanted).then(|| decode(&self.bytes, self.max_payload))
    }

    /// Take a complete, validated frame for processing after reception ends.
    pub fn finish(self) -> Result<Vec<u8>, &'static str> {
        decode(&self.bytes, self.max_payload)?;
        Ok(self.bytes)
    }

    pub fn reset(&mut self) {
        self.bytes.clear();
        self.wanted = HEADER_LEN;
    }
}

fn validate_shape(kind: Kind, payload: &[u8], signed: bool) -> Result<(), &'static str> {
    if signed && kind != Kind::Install {
        return Err("signature on non-install frame");
    }
    match kind {
        Kind::Hello | Kind::InventoryQuery if !payload.is_empty() => {
            Err("query frame has a payload")
        }
        Kind::Target => crate::target::TargetSpec::decode(payload).map(|_| ()),
        Kind::Install => decode_install_payload(payload).map(|_| ()),
        Kind::Inventory => decode_inventory(payload).map(|_| ()),
        Kind::Result if std::str::from_utf8(payload).is_err() => Err("invalid result text"),
        _ => Ok(()),
    }
}

fn crc32(header: &[u8], body: &[u8]) -> u32 {
    crate::crc32::crc32_from(crate::crc32::crc32_from(0, header), body)
}
