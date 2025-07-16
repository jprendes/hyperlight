use std::fmt::Debug;
use std::ops::{Bound, Deref, RangeBounds};
use std::os::fd::{AsRawFd as _, FromRawFd};
use std::ptr::null_mut;
use std::sync::Arc;

use libc::{
    MAP_FAILED, MAP_FIXED, MAP_NORESERVE, MAP_PRIVATE, MAP_SHARED, PROT_EXEC, PROT_NONE, PROT_READ,
    PROT_WRITE,
};

use super::MemoryAccess;

#[derive(Debug)]
pub struct MemorySnapshot {
    file: std::fs::File,
}

impl MemorySnapshot {
    pub fn from_file(file: std::fs::File) -> std::io::Result<Self> {
        Ok(Self { file })
    }

    pub fn zeroed(size: usize) -> std::io::Result<Self> {
        let size = size.next_multiple_of(page_size::get());
        let fd = unsafe { libc::memfd_create(b"hyperlight_snapshot\0".as_ptr() as _, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.set_len(size as u64)?;
        Self::from_file(file)
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

pub type ArcCowMappedMemory = MappedMemory<MAP_PRIVATE, Arc<MemorySnapshot>>;
pub type CowMappedMemory<'a> = MappedMemory<MAP_PRIVATE, &'a MemorySnapshot>;

pub type MutableMappedMemory<'a> = MappedMemory<MAP_SHARED, &'a mut MemorySnapshot>;

#[derive(Debug)]
pub struct MappedMemory<const FLAGS: libc::c_int, S: Debug> {
    #[allow(dead_code)]
    snapshot: S,
    ptr: *mut u8,
    length: usize,
}

impl<const FLAGS: libc::c_int, S: Deref<Target = MemorySnapshot> + Debug> MappedMemory<FLAGS, S> {
    fn new(snapshot: S) -> std::io::Result<Self> {
        let length = snapshot.file.metadata()?.len() as usize;
        let length = length.next_multiple_of(page_size::get());

        let ptr = unsafe {
            libc::mmap(
                null_mut(),
                length,
                PROT_READ | PROT_WRITE,
                FLAGS | MAP_NORESERVE,
                snapshot.file.as_raw_fd(),
                0,
            )
        };
        if ptr == MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        let ptr = ptr as *mut u8;

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
        let new_ptr = unsafe {
            libc::mmap(
                self.ptr as _,
                self.length,
                PROT_READ | PROT_WRITE,
                FLAGS | MAP_NORESERVE | MAP_FIXED,
                self.snapshot.file.as_raw_fd(),
                0,
            )
        };
        if new_ptr == MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn protect(
        &mut self,
        offset: impl RangeBounds<usize>,
        access: MemoryAccess,
    ) -> std::io::Result<()> {
        let start = match offset.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s + 1,
            Bound::Unbounded => 0,
        };
        let end = match offset.end_bound() {
            Bound::Included(&s) => s + 1,
            Bound::Excluded(&s) => s,
            Bound::Unbounded => self.length,
        };

        if end <= start || end > self.length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid range for memory protection",
            ));
        }

        if start != start.next_multiple_of(page_size::get())
            || end != end.next_multiple_of(page_size::get())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Memory protection range must be page-aligned",
            ));
        }

        let res =
            unsafe { libc::mprotect(self.ptr.add(start) as _, end - start, access.to_posix()) };
        if res < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl<const FLAGS: libc::c_int, S: Clone + Debug> MappedMemory<FLAGS, S> {
    pub fn get_base_snapshot(&self) -> S {
        self.snapshot.clone()
    }
}

impl<const FLAGS: libc::c_int, S: Debug> Drop for MappedMemory<FLAGS, S> {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as _, self.length);
        }
    }
}

impl MemoryAccess {
    fn to_posix(&self) -> libc::c_int {
        let mut access = 0;
        if *self == MemoryAccess::NONE {
            access = PROT_NONE;
        } else {
            if self.contains(MemoryAccess::READ) {
                access |= PROT_READ;
            }
            if self.contains(MemoryAccess::WRITE) {
                access |= PROT_WRITE;
            }
            if self.contains(MemoryAccess::EXEC) {
                access |= PROT_EXEC;
            }
        }
        access
    }
}
