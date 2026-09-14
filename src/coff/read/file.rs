//! Dispatch over the binary PE/COFF input kinds.

use super::header::{is_bigobj, is_import_object};
use super::import::ShortImport;
use super::object::CoffObject;
use super::pe::PeImage;
use super::source::Source;
use crate::error::Result;

/// Any binary PE/COFF input: an object, a short import object or an image.
#[derive(Clone, Copy, Debug)]
pub enum CoffFile<'a> {
    /// A relocatable object (regular or `/bigobj`).
    Object(CoffObject<'a>),
    /// A short import object from an import library.
    ShortImport(ShortImport<'a>),
    /// A PE image (DLL or executable).
    Image(PeImage<'a>),
}

impl<'a> CoffFile<'a> {
    /// Parses `data` as whichever kind its leading bytes announce:
    /// `ANON_OBJECT_HEADER_BIGOBJ` or a regular header for objects, an
    /// `IMPORT_OBJECT_HEADER` for short imports, `MZ` for images.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the selected parser fails.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        if is_import_object(data) && !is_bigobj(data) {
            return ShortImport::parse(data, source).map(Self::ShortImport);
        }
        if data.starts_with(b"MZ") {
            return PeImage::parse(data, source).map(Self::Image);
        }
        CoffObject::parse(data, source).map(Self::Object)
    }

    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    #[must_use]
    pub fn machine(&self) -> u16 {
        match self {
            Self::Object(object) => object.machine(),
            Self::ShortImport(import) => import.machine,
            Self::Image(image) => image.machine(),
        }
    }
}
