//! Program headers.

use super::format::ElfFormat;

/// A decoded program header, with address-sized fields widened to 64 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgramHeader {
    /// Segment type (`PT_*`).
    pub p_type: u32,
    /// Flags (`PF_*`).
    pub p_flags: u32,
    /// File offset of the contents.
    pub p_offset: u64,
    /// Virtual address.
    pub p_vaddr: u64,
    /// Physical address.
    pub p_paddr: u64,
    /// Size in the file.
    pub p_filesz: u64,
    /// Size in memory.
    pub p_memsz: u64,
    /// Alignment.
    pub p_align: u64,
}

impl ProgramHeader {
    /// Returns the file offset corresponding to virtual address `vaddr`, if
    /// the address lies within the file-backed part of this segment.
    #[must_use]
    pub fn file_offset_of(&self, vaddr: u64) -> Option<u64> {
        let delta = vaddr.checked_sub(self.p_vaddr)?;
        if delta >= self.p_filesz {
            return None;
        }
        self.p_offset.checked_add(delta)
    }
}

/// The program header table: a lazily decoded slice of program headers.
#[derive(Debug)]
pub struct ProgramHeaderTable<'a, F: ElfFormat> {
    raw: &'a [F::Phdr],
}

impl<F: ElfFormat> Clone for ProgramHeaderTable<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ElfFormat> Copy for ProgramHeaderTable<'_, F> {}

impl<F: ElfFormat> Default for ProgramHeaderTable<'_, F> {
    fn default() -> Self {
        Self { raw: &[] }
    }
}

impl<'a, F: ElfFormat> ProgramHeaderTable<'a, F> {
    /// Wraps raw program headers.
    #[must_use]
    pub fn new(raw: &'a [F::Phdr]) -> Self {
        Self { raw }
    }

    /// Number of program headers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Whether there are no program headers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Decodes program header `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<ProgramHeader> {
        self.raw.get(index).map(F::decode_phdr)
    }

    /// Iterates over the decoded program headers.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = ProgramHeader> + use<'a, F> {
        self.raw.iter().map(F::decode_phdr)
    }

    /// Translates a virtual address to a file offset through the `PT_LOAD`
    /// segments.
    #[must_use]
    pub fn vaddr_to_offset(&self, vaddr: u64) -> Option<u64> {
        self.iter()
            .filter(|p| p.p_type == super::consts::PT_LOAD)
            .find_map(|p| p.file_offset_of(vaddr))
    }
}
