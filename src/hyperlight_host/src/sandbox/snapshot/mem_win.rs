use std::fmt::Debug;
use std::ops::Deref;
use std::os::windows::io::AsRawHandle as _;
use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingA, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS,
    PAGE_READWRITE, PAGE_WRITECOPY, UnmapViewOfFile, UnmapViewOfFileEx, VirtualAlloc2,
};
use windows::core::PCSTR;

#[derive(Debug)]
pub struct MemorySnapshot {
    handle: HANDLE,
    size: usize,
}

impl MemorySnapshot {
    pub fn from_file(file: std::fs::File) -> std::io::Result<Self> {
        let size = file.metadata()?.len() as usize;

        // we need usize to be 8 bytes on Windows so that we can split
        // the size into high and low parts
        const _: () = assert!(std::mem::size_of::<usize>() == 8);

        let size = size.next_multiple_of(page_size::get() as _);
        let size_high = (size >> 32) as u32;
        let size_low = (size & 0xFFFFFFFF) as u32;

        let handle = unsafe {
            CreateFileMappingA(
                HANDLE(file.as_raw_handle()),
                None,
                PAGE_READWRITE,
                size_high,
                size_low,
                PCSTR::null(),
            )
        }?;

        Ok(Self { handle, size })
    }

    pub fn zeroed(size: usize) -> std::io::Result<Self> {
        // we need usize to be 8 bytes on Windows so that we can split
        // the size into high and low parts
        const _: () = assert!(std::mem::size_of::<usize>() == 8);

        let size = size.next_multiple_of(page_size::get() as _);
        let size_high = (size >> 32) as u32;
        let size_low = (size & 0xFFFFFFFF) as u32;

        let handle = unsafe {
            CreateFileMappingA(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                size_high,
                size_low,
                PCSTR::null(),
            )
        }?;

        Ok(Self { handle, size })
    }

    pub fn from_slice(buf: &[u8]) -> std::io::Result<Self> {
        let mut this = Self::zeroed(buf.len())?;
        this.map_mut()?.as_mut_slice()[0..buf.len()].copy_from_slice(buf);
        Ok(this)
    }

    pub fn map_cow(&self) -> std::io::Result<CowMappedMemory> {
        CowMappedMemory::new(self)
    }

    pub fn arc_map_cow(self: &Arc<Self>) -> std::io::Result<ArcCowMappedMemory> {
        ArcCowMappedMemory::new(self.clone())
    }

    pub fn map_mut(&mut self) -> std::io::Result<MutableMappedMemory> {
        MutableMappedMemory::new(self)
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Self::from_slice(self.map_cow()?.as_slice())
    }
}

impl Drop for MemorySnapshot {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

pub type ArcCowMappedMemory = MappedMemory<{ PAGE_WRITECOPY.0 }, Arc<MemorySnapshot>>;
pub type CowMappedMemory<'a> = MappedMemory<{ PAGE_WRITECOPY.0 }, &'a MemorySnapshot>;

pub type MutableMappedMemory<'a> = MappedMemory<{ PAGE_READWRITE.0 }, &'a mut MemorySnapshot>;

#[derive(Debug)]
pub struct MappedMemory<const FLAGS: u32, S: Debug> {
    #[allow(dead_code)]
    snapshot: S,
    ptr: *mut u8,
    length: usize,
}

impl<const FLAGS: u32, S: Deref<Target = MemorySnapshot> + Debug> MappedMemory<FLAGS, S> {
    fn new(snapshot: S) -> std::io::Result<Self> {
        let length = snapshot.size;
        let placeholder = unsafe {
            VirtualAlloc2(
                None,
                None,
                length,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS.0,
                None,
            )
        };
        if placeholder.is_null() {
            return Err(std::io::Error::last_os_error())?;
        }
        let ptr = unsafe {
            MapViewOfFile3(
                snapshot.handle,
                None,
                Some(placeholder as *const _),
                0,
                length,
                MEM_REPLACE_PLACEHOLDER,
                FLAGS,
                None,
            )
        };
        if ptr.Value.is_null() {
            return Err(std::io::Error::last_os_error())?;
        }
        if ptr.Value != placeholder {
            return Err(std::io::Error::other(format!(
                "Memory mapping failed: pointer mismatch, received {:?}, expected {:?}",
                ptr.Value, placeholder
            )))?;
        }
        let ptr = ptr.Value as _;
        Ok(Self {
            snapshot,
            ptr,
            length,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.length) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.length) }
    }

    pub fn take_new_snapshot(&self) -> std::io::Result<MemorySnapshot> {
        // TODO: be clever in the case where the memory hasn't been modified
        // and just return the original snapshot.
        // This would probably require different implementations for each
        // MappedMemory alias, since the optimization doesn't work for
        // MutableMappedMemory.
        MemorySnapshot::from_slice(self.as_slice())
    }

    pub fn restore(&mut self) -> std::io::Result<()> {
        unsafe {
            UnmapViewOfFileEx(
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.ptr as _,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        }?;
        let new_ptr = unsafe {
            MapViewOfFile3(
                self.snapshot.handle,
                None,
                Some(self.ptr as *const _),
                0,
                self.snapshot.size,
                MEM_REPLACE_PLACEHOLDER,
                FLAGS,
                None,
            )
        };
        if new_ptr.Value.is_null() {
            println!("trying to map to {:?}", self.ptr);
            return Err(std::io::Error::last_os_error())?;
        }
        let new_ptr: *mut u8 = new_ptr.Value as _;
        if new_ptr != self.ptr {
            return Err(std::io::Error::other(format!(
                "Memory restore failed: pointer mismatch, received {:?}, expected {:?}",
                new_ptr, self.ptr
            )))?;
        }
        Ok(())
    }
}

impl<const FLAGS: u32, S: Clone + Debug> MappedMemory<FLAGS, S> {
    pub fn get_base_snapshot(&self) -> S {
        self.snapshot.clone()
    }
}

impl<const FLAGS: u32, S: Debug> Drop for MappedMemory<FLAGS, S> {
    fn drop(&mut self) {
        let _ = unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr as _,
            })
        };
    }
}
