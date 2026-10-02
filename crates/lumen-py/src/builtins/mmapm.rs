//! `mmap` on `lumen_os::mmap`: memory-mapped files and anonymous memory. The mapped region is
//! exposed to the buffer protocol as an external `lumen_common::buffer::ByteStore` that shares
//! the `Mapping` with the object, so `memoryview`, `bytes.join` and friends see the live bytes;
//! a store's pins are the exports `close()` and `resize()` refuse to cross.

/// Windows: mmap(fileno, length[, tagname[, access[, offset]]])
///
/// Maps length bytes from the file specified by the file handle fileno,
/// and returns a mmap object.  If length is larger than the current size
/// of the file, the file is extended to contain length bytes.  If length
/// is 0, the maximum length of the map is the current size of the file,
/// except that if the file is empty Windows raises an exception (you cannot
/// create an empty mapping on Windows).
///
/// Unix: mmap(fileno, length[, flags[, prot[, access[, offset]]]])
///
/// Maps length bytes from the file specified by the file descriptor fileno,
/// and returns a mmap object.  If length is 0, the maximum length of the map
/// will be the current size of the file when mmap is called.
/// flags specifies the nature of the mapping. MAP_PRIVATE creates a
/// private copy-on-write mapping, so changes to the contents of the mmap
/// object will be private to this process, and MAP_SHARED creates a mapping
/// that's shared with all other processes mapping the same areas of the file.
/// The default value is MAP_SHARED.
///
/// To map anonymous memory, pass -1 as the fileno (both versions).
#[lumen_bind::module(name = "mmap")]
pub mod mmap {
    use crate::bind::{Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::buffer::ByteStore;
    use lumen_common::search;
    use lumen_os::mmap::Mapping;
    use std::rc::Rc;

    const ACCESS_DEFAULT: i32 = 0;
    const ACCESS_READ: i32 = 1;
    const ACCESS_WRITE: i32 = 2;
    const ACCESS_COPY: i32 = 3;

    const MAP_SHARED: i32 = 1;
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;

    #[class(name = "mmap", module = "mmap")]
    pub struct Mmap {
        map: Option<Rc<Mapping>>,
        store: Option<Rc<ByteStore>>,
        pos: usize,
        offset: i64,
        access: i32,
        fd: i32,
    }

    impl Drop for Mmap {
        fn drop(&mut self) {
            if self.fd >= 0 {
                let _ = lumen_os::fs::close(self.fd);
            }
        }
    }

    fn os_err(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    fn closed(it: &mut Interp) -> Obj {
        it.value_error("mmap closed or invalid")
    }

    fn store_for(map: &Rc<Mapping>, readonly: bool) -> Rc<ByteStore> {
        let owner: Rc<dyn std::any::Any> = map.clone();
        // SAFETY: the mapping owns `ptr..ptr + len` and lives as long as the store, which holds
        // it; `resize` replaces the store whenever the mapping moves.
        let store = unsafe { ByteStore::external(map.ptr(), map.len(), owner) };
        if readonly {
            store.set_readonly();
        }
        Rc::new(store)
    }

    /// The store of mmap `v` and whether it is read-only; `None` when `v` is not an mmap, and
    /// `ValueError` when it is closed.
    pub fn buffer_of(it: &mut Interp, v: &Value) -> R<Option<(Rc<ByteStore>, bool)>> {
        let Some(p) = Py::<Mmap>::from_value(it, v) else { return Ok(None) };
        let m = p.borrow(it)?;
        match &m.store {
            Some(s) => Ok(Some((s.clone(), m.access == ACCESS_READ))),
            None => Err(closed(it)),
        }
    }

    impl Mmap {
        fn store(&self, it: &mut Interp) -> R<Rc<ByteStore>> {
            self.store.clone().ok_or_else(|| closed(it))
        }

        fn writable(&self, it: &mut Interp) -> R<()> {
            if self.access == ACCESS_READ {
                return Err(it.type_error("mmap can't modify a readonly memory map."));
            }
            Ok(())
        }

        fn remaining(&self, size: usize) -> usize {
            size.saturating_sub(self.pos)
        }
    }

    fn clamp_index(i: isize, size: usize) -> usize {
        let i = if i < 0 { i + size as isize } else { i };
        i.clamp(0, size as isize) as usize
    }

    fn index_in(it: &mut Interp, key: &Value, size: usize) -> R<usize> {
        let i = it.seq_index(key)?;
        let j = if i < 0 { i + size as i64 } else { i };
        if j < 0 || j >= size as i64 {
            return Err(it.new_exc_str("IndexError", "mmap index out of range"));
        }
        Ok(j as usize)
    }

    #[methods]
    impl Mmap {
        #[constructor]
        fn new(
            it: &mut Interp,
            #[kw] fileno: i32,
            #[kw] length: isize,
            #[kw]
            #[default(1)]
            flags: i32,
            #[kw]
            #[default(3)]
            prot: i32,
            #[kw]
            #[default(0)]
            access: i32,
            #[kw]
            #[default(0)]
            offset: i64,
        ) -> R<Mmap> {
            let (mut flags, mut prot, mut access) = (flags, prot, access);
            if length < 0 {
                return Err(it.overflow_err("memory mapped length must be positive"));
            }
            if offset < 0 {
                return Err(it.overflow_err("memory mapped offset must be positive"));
            }
            if access != ACCESS_DEFAULT && (flags != MAP_SHARED || prot != (PROT_WRITE | PROT_READ)) {
                return Err(it.value_error("mmap can't specify both access and flags, prot."));
            }
            let (private, rw) = (lumen_os::mmap::MAP_PRIVATE, PROT_READ | PROT_WRITE);
            match access {
                ACCESS_READ => (flags, prot) = (MAP_SHARED, PROT_READ),
                ACCESS_WRITE => (flags, prot) = (MAP_SHARED, rw),
                ACCESS_COPY => (flags, prot) = (private, rw),
                ACCESS_DEFAULT => {
                    if prot & PROT_READ != 0 && prot & PROT_WRITE != 0 {
                    } else if prot & PROT_WRITE != 0 {
                        access = ACCESS_WRITE;
                    } else {
                        access = ACCESS_READ;
                    }
                }
                _ => return Err(it.value_error("mmap invalid access parameter.")),
            }
            let mut length = length as u64;
            if fileno != -1 {
                if let Ok(st) = lumen_os::fs::fstat(fileno) {
                    if st.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFREG {
                        let size = st.size;
                        if length == 0 {
                            if size == 0 {
                                return Err(it.value_error("cannot mmap an empty file"));
                            }
                            if offset as u64 >= size {
                                return Err(it.value_error("mmap offset is greater than file size"));
                            }
                            if size - offset as u64 > isize::MAX as u64 {
                                return Err(it.value_error("mmap length is too large"));
                            }
                            length = size - offset as u64;
                        } else if offset as u64 > size || size - (offset as u64) < length {
                            return Err(it.value_error("mmap length is greater than file size"));
                        }
                    }
                }
            }
            let fd = if fileno == -1 {
                -1
            } else {
                lumen_os::fs::dup(fileno).map_err(|e| os_err(it, e))?
            };
            let map = match Mapping::new(length as usize, prot, flags, fd, offset) {
                Ok(m) => Rc::new(m),
                Err(e) => {
                    if fd >= 0 {
                        let _ = lumen_os::fs::close(fd);
                    }
                    return Err(os_err(it, e));
                }
            };
            let store = store_for(&map, access == ACCESS_READ);
            Ok(Mmap { map: Some(map), store: Some(store), pos: 0, offset, access, fd })
        }

        fn close(&mut self, it: &mut Interp) -> R<()> {
            if let Some(s) = &self.store {
                if s.is_pinned() {
                    return Err(it.new_exc_str("BufferError", "cannot close exported pointers exist"));
                }
            }
            self.store = None;
            self.map = None;
            if self.fd >= 0 {
                let _ = lumen_os::fs::close(self.fd);
                self.fd = -1;
            }
            Ok(())
        }

        /// Return the lowest index in the mmap where the subsequence sub is found.
        fn find(&mut self, it: &mut Interp, sub: &[u8], start: Option<isize>, end: Option<isize>) -> R<i64> {
            self.find_impl(it, sub, start, end, false)
        }

        fn rfind(&mut self, it: &mut Interp, sub: &[u8], start: Option<isize>, end: Option<isize>) -> R<i64> {
            self.find_impl(it, sub, start, end, true)
        }

        fn flush(&mut self, it: &mut Interp, offset: Option<isize>, size: Option<isize>) -> R<()> {
            let store = self.store(it)?;
            let total = store.len() as isize;
            let (offset, size) = (offset.unwrap_or(0), size.unwrap_or(total));
            if size < 0 || offset < 0 || total - offset < size {
                return Err(it.value_error("flush values out of range"));
            }
            if self.access == ACCESS_READ || self.access == ACCESS_COPY {
                return Ok(());
            }
            let map = self.map.as_ref().expect("open mmap has a mapping");
            map.sync(offset as usize, size as usize).map_err(|e| os_err(it, e))
        }

        fn madvise(&mut self, it: &mut Interp, option: i32, start: Option<isize>, length: Option<isize>) -> R<()> {
            let store = self.store(it)?;
            let size = store.len() as isize;
            let (start, length) = (start.unwrap_or(0), length.unwrap_or(size));
            if start < 0 || start >= size {
                return Err(it.value_error("madvise start out of bounds"));
            }
            if length < 0 {
                return Err(it.value_error("madvise length invalid"));
            }
            if isize::MAX - start < length {
                return Err(it.overflow_err("madvise length too large"));
            }
            let length = length.min(size - start);
            let map = self.map.as_ref().expect("open mmap has a mapping");
            map.advise(start as usize, length as usize, option).map_err(|e| os_err(it, e))
        }

        #[method(name = "move")]
        fn move_(&mut self, it: &mut Interp, dest: isize, src: isize, cnt: isize) -> R<()> {
            let store = self.store(it)?;
            self.writable(it)?;
            let size = store.len() as isize;
            if dest < 0 || src < 0 || cnt < 0 || size - dest < cnt || size - src < cnt {
                return Err(it.value_error("source, destination, or count out of range"));
            }
            store.bytes_mut().copy_within(src as usize..(src + cnt) as usize, dest as usize);
            Ok(())
        }

        /// Return a bytes object of at most n bytes read from the current position.
        fn read(&mut self, it: &mut Interp, n: Option<&Value>) -> R<Value> {
            let store = self.store(it)?;
            let want = match n {
                None | Some(Value::None) => isize::MAX,
                Some(v) => it.index_of(v)? as isize,
            };
            let remaining = self.remaining(store.len());
            let count = if want < 0 || want as usize > remaining { remaining } else { want as usize };
            let out = store.bytes()[self.pos.min(store.len())..][..count].to_vec();
            self.pos += count;
            Ok(Value::bytes(out))
        }

        fn read_byte(&mut self, it: &mut Interp) -> R<i64> {
            let store = self.store(it)?;
            if self.pos >= store.len() {
                return Err(it.value_error("read byte out of range"));
            }
            let b = store.bytes()[self.pos];
            self.pos += 1;
            Ok(b as i64)
        }

        fn readline(&mut self, it: &mut Interp) -> R<Value> {
            let store = self.store(it)?;
            let remaining = self.remaining(store.len());
            if remaining == 0 {
                return Ok(Value::bytes(Vec::new()));
            }
            let bytes = store.bytes();
            let rest = &bytes[self.pos..];
            let n = search::memchr(b'\n', rest).map_or(rest.len(), |i| i + 1);
            let out = rest[..n].to_vec();
            drop(bytes);
            self.pos += n;
            Ok(Value::bytes(out))
        }

        fn resize(&mut self, it: &mut Interp, new_size: isize) -> R<()> {
            let store = self.store(it)?;
            if store.is_pinned() {
                return Err(it.new_exc_str("BufferError", "mmap can't resize with extant buffers exported."));
            }
            if self.access != ACCESS_WRITE && self.access != ACCESS_DEFAULT {
                return Err(it.type_error("mmap can't resize a readonly or copy-on-write memory map."));
            }
            if new_size < 0 || isize::MAX as i64 - (new_size as i64) < self.offset {
                return Err(it.value_error("new size out of range"));
            }
            if !lumen_os::mmap::HAVE_MREMAP {
                return Err(it.new_exc_str("SystemError", "mmap: resizing not available--no mremap()"));
            }
            if self.fd != -1 {
                lumen_os::fs::ftruncate(self.fd, (self.offset + new_size as i64) as u64).map_err(|e| os_err(it, e))?;
            }
            let map = self.map.clone().expect("open mmap has a mapping");
            map.remap(new_size as usize).map_err(|e| os_err(it, e))?;
            self.store = Some(store_for(&map, false));
            Ok(())
        }

        fn seek(&mut self, it: &mut Interp, dist: isize, #[default(0)] how: i32) -> R<()> {
            let store = self.store(it)?;
            let size = store.len() as isize;
            let out_of_range = |it: &mut Interp| it.value_error("seek out of range");
            let where_ = match how {
                0 => dist,
                1 => {
                    if isize::MAX - (self.pos as isize) < dist {
                        return Err(out_of_range(it));
                    }
                    self.pos as isize + dist
                }
                2 => {
                    if isize::MAX - size < dist {
                        return Err(out_of_range(it));
                    }
                    size + dist
                }
                _ => return Err(it.value_error("unknown seek type")),
            };
            if where_ > size || where_ < 0 {
                return Err(out_of_range(it));
            }
            self.pos = where_ as usize;
            Ok(())
        }

        fn size(&mut self, it: &mut Interp) -> R<i64> {
            let store = self.store(it)?;
            if self.fd != -1 {
                let st = lumen_os::fs::fstat(self.fd).map_err(|e| os_err(it, e))?;
                if st.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFREG {
                    return Ok(st.size as i64);
                }
            }
            Ok(store.len() as i64)
        }

        fn tell(&mut self, it: &mut Interp) -> R<usize> {
            self.store(it)?;
            Ok(self.pos)
        }

        /// Write the bytes in data into memory at the current position of the file pointer.
        fn write(&mut self, it: &mut Interp, data: &[u8]) -> R<usize> {
            let store = self.store(it)?;
            self.writable(it)?;
            if self.pos > store.len() || store.len() - self.pos < data.len() {
                return Err(it.value_error("data out of range"));
            }
            store.bytes_mut()[self.pos..self.pos + data.len()].copy_from_slice(data);
            self.pos += data.len();
            Ok(data.len())
        }

        fn write_byte(&mut self, it: &mut Interp, byte: i64) -> R<()> {
            let store = self.store(it)?;
            if byte < i8::MIN as i64 {
                return Err(it.overflow_err("signed char is less than minimum"));
            }
            if byte > i8::MAX as i64 {
                return Err(it.overflow_err("signed char is greater than maximum"));
            }
            self.writable(it)?;
            if self.pos >= store.len() {
                return Err(it.value_error("write byte out of range"));
            }
            store.bytes_mut()[self.pos] = byte as u8;
            self.pos += 1;
            Ok(())
        }

        #[getter]
        fn closed(&self) -> bool {
            self.store.is_none()
        }

        #[proto(enter)]
        fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            slf.0.borrow(it)?.store(it)?;
            Ok(slf.0.value().clone())
        }

        #[proto(exit)]
        fn exit(&mut self, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            self.close(it)
        }

        #[proto(len)]
        fn len(&self, it: &mut Interp) -> R<usize> {
            Ok(self.store(it)?.len())
        }

        #[proto(getitem)]
        fn getitem(&self, it: &mut Interp, key: &Value) -> R<Value> {
            let store = self.store(it)?;
            let size = store.len();
            if it.has_index(key) {
                let i = index_in(it, key, size)?;
                return Ok(Value::Int(store.bytes()[i] as i64));
            }
            if !it.is_slice(key) {
                return Err(it.type_error("mmap indices must be integers"));
            }
            let (start, stop, step) = it.slice_bounds(key, size)?;
            let count = crate::ops::slice_len(start, stop, step);
            let bytes = store.bytes();
            let out = if count == 0 {
                Vec::new()
            } else if step == 1 {
                bytes[start as usize..start as usize + count].to_vec()
            } else {
                (0..count as i64).map(|k| bytes[(start + k * step) as usize]).collect()
            };
            Ok(Value::bytes(out))
        }

        #[proto(setitem)]
        fn setitem(&mut self, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
            let store = self.store(it)?;
            self.writable(it)?;
            let size = store.len();
            if it.has_index(key) {
                let i = index_in(it, key, size)?;
                if !it.has_index(value) {
                    return Err(it.type_error("mmap item value must be an int"));
                }
                let v = it.index_of(value)?;
                if !(0..=255).contains(&v) {
                    return Err(it.value_error("mmap item value must be in range(0, 256)"));
                }
                store.bytes_mut()[i] = v as u8;
                return Ok(());
            }
            if !it.is_slice(key) {
                return Err(it.type_error("mmap indices must be integer"));
            }
            let (start, stop, step) = it.slice_bounds(key, size)?;
            let count = crate::ops::slice_len(start, stop, step);
            if !it.is_buffer(value) {
                let t = it.type_name_of(value);
                return Err(it.type_error(&format!("a bytes-like object is required, not '{t}'")));
            }
            let data = it.bytes_of(value)?;
            if data.len() != count {
                return Err(it.new_exc_str("IndexError", "mmap slice assignment is wrong size"));
            }
            let mut bytes = store.bytes_mut();
            if step == 1 {
                bytes[start as usize..start as usize + count].copy_from_slice(&data);
            } else {
                for (k, b) in data.iter().enumerate() {
                    bytes[(start + k as i64 * step) as usize] = *b;
                }
            }
            Ok(())
        }

        #[proto(delitem)]
        fn delitem(&mut self, it: &mut Interp, key: &Value) -> R<()> {
            let store = self.store(it)?;
            self.writable(it)?;
            if it.has_index(key) {
                index_in(it, key, store.len())?;
                return Err(it.type_error("mmap doesn't support item deletion"));
            }
            if !it.is_slice(key) {
                return Err(it.type_error("mmap indices must be integer"));
            }
            Err(it.type_error("mmap object doesn't support slice deletion"))
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let name = it.tp_name_of(slf.0.value());
            let m = slf.0.borrow(it)?;
            let Some(store) = &m.store else { return Ok(format!("<{name} closed=True>")) };
            let access = match m.access {
                ACCESS_READ => "ACCESS_READ",
                ACCESS_WRITE => "ACCESS_WRITE",
                ACCESS_COPY => "ACCESS_COPY",
                _ => "ACCESS_DEFAULT",
            };
            Ok(format!("<{name} closed=False, access={access}, length={}, pos={}, offset={}>", store.len(), m.pos, m.offset))
        }
    }

    impl Mmap {
        fn find_impl(&mut self, it: &mut Interp, sub: &[u8], start: Option<isize>, end: Option<isize>, reverse: bool) -> R<i64> {
            let store = self.store(it)?;
            let size = store.len();
            let start = clamp_index(start.unwrap_or(self.pos as isize), size);
            let end = clamp_index(end.unwrap_or(size as isize), size);
            if end < start {
                return Ok(-1);
            }
            let bytes = store.bytes();
            let hay = &bytes[start..end];
            let found = if reverse { search::rfind(hay, sub) } else { search::find(hay, sub) };
            Ok(found.map_or(-1, |i| (i + start) as i64))
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "error", Value::Obj(it.exc_type("OSError")));
        for (name, v) in [("ACCESS_DEFAULT", ACCESS_DEFAULT), ("ACCESS_READ", ACCESS_READ), ("ACCESS_WRITE", ACCESS_WRITE), ("ACCESS_COPY", ACCESS_COPY)] {
            dict_set_str(&d, name, Value::Int(v as i64));
        }
        for (name, v) in lumen_os::mmap::constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
