use bitflags::bitflags;

#[cfg_attr(target_os = "linux", path = "mem_linux.rs")]
#[cfg_attr(target_os = "windows", path = "mem_win.rs")]
mod r#impl;

pub(crate) use r#impl::{ArcCowMappedMemory, CowMappedMemory, MemorySnapshot, MutableMappedMemory};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MemoryAccess: u8 {
        const NONE = 0x00;
        const READ = 0x01;
        const WRITE = 0x02;
        const EXEC = 0x04;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use super::MemorySnapshot;

    #[test]
    fn test_zeroed() {
        // Test that MemorySnapshot::zeroed genertes a snapshot full of zeros
        // of at least the requested size (it may be larger due to alignment)
        let snapshot = MemorySnapshot::zeroed(1).unwrap();
        let mapping = snapshot.map_cow().unwrap();
        assert!(mapping.as_slice().len() >= 1);
        assert!(mapping.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_from_slice() {
        // Test that MemorySnapshot::from_slice genertes a snapshot initialized to
        // the contents of the slice.
        // The resulting allocation may be larger than the slice due to alignment.
        let snapshot = MemorySnapshot::from_slice(b"hello slice").unwrap();
        let mapping = snapshot.map_cow().unwrap();
        assert!(mapping.as_slice().starts_with(b"hello slice"));
    }

    #[test]
    fn test_from_file() {
        // Test that MemorySnapshot::from_file creates a snapshot initialized to
        // the contents of the file.
        let d = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create_new(d.path().join("tempfile")).unwrap();
        f.write_all(b"hello file").unwrap();
        let snapshot = MemorySnapshot::from_file(f).unwrap();
        let mapping = snapshot.map_cow().unwrap();
        assert!(mapping.as_slice().starts_with(b"hello file"));
    }

    #[test]
    fn test_map_mut() {
        // Test that mutating a snapshot mapped with map_mut actually mutates the
        // original snapshot.
        let mut snapshot = MemorySnapshot::zeroed(10).unwrap();

        snapshot.map_mut().unwrap().as_mut_slice()[0..10].copy_from_slice(b"0123456789");

        let mapping = snapshot.map_cow().unwrap();
        assert!(mapping.as_slice().starts_with(b"0123456789"));
    }

    #[test]
    fn test_map_restore() {
        // Test that restoring a mapping works and that it restores the original
        // contents of the snapshot without chaging the mapping's address.
        let snapshot = MemorySnapshot::from_slice(b"0123456789").unwrap();

        let mut mapping = snapshot.map_cow().unwrap();
        mapping.as_mut_slice()[0..10].copy_from_slice(b"9876543210");
        assert!(mapping.as_slice().starts_with(b"9876543210"));

        let ptr = mapping.as_slice().as_ptr();

        mapping.restore().unwrap();

        assert!(mapping.as_slice().starts_with(b"0123456789"));

        let new_ptr = mapping.as_slice().as_ptr();

        assert_eq!(ptr, new_ptr);
    }

    #[test]
    fn test_map_cow() {
        // Test that mutating a snapshot mapped with map_cow does not mutate the
        // original snapshot.
        // CoW mappings of the same snapshot should not interfere with each other.
        let snapshot = MemorySnapshot::zeroed(10).unwrap();

        let mut map1 = snapshot.map_cow().unwrap();
        map1.as_mut_slice()[0..10].copy_from_slice(b"0123456789");

        let map2 = snapshot.map_cow().unwrap();

        assert!(map1.as_slice().starts_with(b"0123456789"));
        assert!(!map2.as_slice().starts_with(b"0000000000"));
    }

    #[test]
    fn test_mapped_arc() {
        // Test that an Arc-wrapped snapshot can be cow mapped and the lifetime
        // of the mapping is independet of the lifetime of the Arc-wrapped snapshot.
        let snapshot = MemorySnapshot::from_slice(b"hello world").unwrap();
        let snapshot = Arc::new(snapshot);

        let mapping = snapshot.arc_map_cow().unwrap();

        drop(snapshot);

        assert!(mapping.as_slice().starts_with(b"hello world"));
    }

    #[test]
    fn test_mapping_base() {
        // Test that the base of a mapping is the same as the original snapshot.
        let snapshot = MemorySnapshot::zeroed(1).unwrap();

        let mapping = snapshot.map_cow().unwrap();

        let base = mapping.get_base_snapshot();

        assert!(base as *const _ == &snapshot as *const _);
    }

    #[test]
    fn test_arc_mapping_base() {
        // Test that the base of an arc mapping is the same as the original
        // Arc-wrapped snapshot.
        let snapshot = MemorySnapshot::zeroed(1).unwrap();
        let snapshot = Arc::new(snapshot);

        let mapping = snapshot.arc_map_cow().unwrap();

        let base = mapping.get_base_snapshot();

        assert!(Arc::ptr_eq(&base, &snapshot));
    }

    #[test]
    fn test_mapped_arc_remapping_from_base() {
        // Test that remapping from the base of an Arc-wrapped snapshot works
        // and that the two mappings do not interfere with each other.
        let snapshot = MemorySnapshot::from_slice(b"hello world").unwrap();
        let snapshot = Arc::new(snapshot);

        let mut mapping1 = snapshot.arc_map_cow().unwrap();

        let base = mapping1.get_base_snapshot();

        let mapping2 = base.map_cow().unwrap();

        mapping1.as_mut_slice()[0..11].copy_from_slice(b"hello slice");

        assert!(mapping1.as_slice().starts_with(b"hello slice"));
        assert!(mapping2.as_slice().starts_with(b"hello world"));
    }

    #[test]
    fn test_try_clone_snapshot() {
        // Test that cloning a snapshot works and that mutating the original snapshot
        // does not affect the cloned snapshot.
        let mut snapshot1 = MemorySnapshot::from_slice(b"hello world").unwrap();
        let snapshot2 = snapshot1.try_clone().unwrap();

        let mut map1 = snapshot1.map_mut().unwrap();
        map1.as_mut_slice()[0..11].copy_from_slice(b"hello slice");

        let map2 = snapshot2.map_cow().unwrap();
        assert!(map2.as_slice().starts_with(b"hello world"));
    }

    #[test]
    fn test_take_snapshot() {
        // Test that taking a snapshot from a mapping works and that mutating the
        // original mapping does not affect the new snapshot.
        let mut snapshot1 = MemorySnapshot::from_slice(b"hello world").unwrap();
        let mut map1 = snapshot1.map_mut().unwrap();
        map1.as_mut_slice()[0..11].copy_from_slice(b"hello slice");

        let snapshot2 = map1.take_new_snapshot().unwrap();
        let map2 = snapshot2.map_cow().unwrap();

        assert!(map2.as_slice().starts_with(b"hello slice"));

        map1.as_mut_slice()[0..11].copy_from_slice(b"hello world");

        assert!(map2.as_slice().starts_with(b"hello slice"));
    }
}
