//! Minimal read-only DICOM parser, scoped to what WSI (VL Whole Slide
//! Microscopy Image) instances need.
//!
//! A DICOM file is a 128-byte preamble, the magic `DICM`, a *file meta group*
//! (group `0002`, always explicit-VR little-endian, whose length is given by
//! `(0002,0000)`), and then the dataset proper, encoded per the
//! TransferSyntaxUID from the meta group. Every WSI transfer syntax OpenSlide
//! accepts encodes its dataset as explicit-VR little-endian — only the *pixel*
//! encoding differs — so that is the only dataset encoding parsed here;
//! implicit-VR and big-endian datasets are rejected with a clear error.
//!
//! A dataset is a flat, tag-ordered sequence of elements. An element is
//! `(tag, VR, length, value)`, where a handful of VRs use a 12-byte header with
//! a 32-bit length and the rest use an 8-byte header with a 16-bit length. `SQ`
//! elements nest whole datasets as *items*, and both sequences and items may
//! have an undefined length (`0xFFFFFFFF`), in which case they run until a
//! delimiter item (`(FFFE,E0DD)` for sequences, `(FFFE,E00D)` for items).
//!
//! Pixel data (`(7FE0,0010)`) is the one element never loaded into memory: for
//! encapsulated (JPEG-ish) syntaxes it is an undefined-length element holding a
//! Basic Offset Table item followed by one item ("fragment") per compressed
//! frame, and for uncompressed syntaxes it is one defined-length blob of
//! sequential frames. Either way this parser only records *where* the bytes are
//! ([`PixelData`]), so a multi-GB slide costs a few hundred KiB of metadata and
//! frames are fetched individually at read time.
//!
//! `PerFrameFunctionalGroupsSequence` gets the same treatment for a different
//! reason: a sparsely-tiled level has one item per frame (8000+ on a routine
//! slide, tens of MiB of XML-ish nesting), and the only thing the read path
//! needs from it is each frame's position in the total pixel matrix. Those are
//! extracted into [`DicomObject::frame_positions`] and the items dropped, which
//! also keeps them out of the property dump — matching what the C driver gets
//! from libdicom's metadata subset.

use crate::error::{OpenSlideError, Result};
use crate::io::SlideReader;

/// A DICOM attribute tag, packed as `(group << 16) | element`.
pub type Tag = u32;

/// Build a [`Tag`] from its group and element numbers.
pub const fn tag(group: u16, element: u16) -> Tag {
    ((group as u32) << 16) | element as u32
}

/// Undefined length marker, used by sequences, items, and encapsulated pixel data.
const UNDEFINED_LENGTH: u32 = 0xFFFF_FFFF;

/// Item tag `(FFFE,E000)`: starts a sequence item or a pixel-data fragment.
const ITEM: Tag = tag(0xFFFE, 0xE000);
/// Item delimiter `(FFFE,E00D)`: ends an undefined-length item.
const ITEM_DELIMITER: Tag = tag(0xFFFE, 0xE00D);
/// Sequence delimiter `(FFFE,E0DD)`: ends an undefined-length sequence.
const SEQUENCE_DELIMITER: Tag = tag(0xFFFE, 0xE0DD);

/// Tags this crate reads by name.
pub mod tags {
    use super::{Tag, tag};

    // File meta group.
    pub const FILE_META_GROUP_LENGTH: Tag = tag(0x0002, 0x0000);
    pub const MEDIA_STORAGE_SOP_CLASS_UID: Tag = tag(0x0002, 0x0002);
    pub const TRANSFER_SYNTAX_UID: Tag = tag(0x0002, 0x0010);

    // Instance identity and flavor.
    pub const IMAGE_TYPE: Tag = tag(0x0008, 0x0008);
    pub const SOP_INSTANCE_UID: Tag = tag(0x0008, 0x0018);
    pub const SERIES_INSTANCE_UID: Tag = tag(0x0020, 0x000E);
    pub const CONCATENATION_UID: Tag = tag(0x0020, 0x9161);
    pub const IN_CONCATENATION_NUMBER: Tag = tag(0x0020, 0x9162);
    pub const IN_CONCATENATION_TOTAL_NUMBER: Tag = tag(0x0020, 0x9163);
    pub const DIMENSION_ORGANIZATION_TYPE: Tag = tag(0x0020, 0x9311);

    // Image format.
    pub const SAMPLES_PER_PIXEL: Tag = tag(0x0028, 0x0002);
    pub const PHOTOMETRIC_INTERPRETATION: Tag = tag(0x0028, 0x0004);
    pub const PLANAR_CONFIGURATION: Tag = tag(0x0028, 0x0006);
    pub const NUMBER_OF_FRAMES: Tag = tag(0x0028, 0x0008);
    pub const ROWS: Tag = tag(0x0028, 0x0010);
    pub const COLUMNS: Tag = tag(0x0028, 0x0011);
    pub const PIXEL_SPACING: Tag = tag(0x0028, 0x0030);
    pub const BITS_ALLOCATED: Tag = tag(0x0028, 0x0100);
    pub const BITS_STORED: Tag = tag(0x0028, 0x0101);
    pub const HIGH_BIT: Tag = tag(0x0028, 0x0102);
    pub const PIXEL_REPRESENTATION: Tag = tag(0x0028, 0x0103);
    pub const ICC_PROFILE: Tag = tag(0x0028, 0x2000);
    pub const PIXEL_MEASURES_SEQUENCE: Tag = tag(0x0028, 0x9110);

    // Whole-slide geometry.
    pub const TOTAL_PIXEL_MATRIX_COLUMNS: Tag = tag(0x0048, 0x0006);
    pub const TOTAL_PIXEL_MATRIX_ROWS: Tag = tag(0x0048, 0x0007);
    pub const OPTICAL_PATH_SEQUENCE: Tag = tag(0x0048, 0x0105);
    pub const OBJECTIVE_LENS_POWER: Tag = tag(0x0048, 0x0112);
    pub const PLANE_POSITION_SLIDE_SEQUENCE: Tag = tag(0x0048, 0x021A);
    pub const COLUMN_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX: Tag = tag(0x0048, 0x021E);
    pub const ROW_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX: Tag = tag(0x0048, 0x021F);
    pub const TOTAL_PIXEL_MATRIX_FOCAL_PLANES: Tag = tag(0x0048, 0x0303);

    pub const BARCODE_VALUE: Tag = tag(0x2200, 0x0005);

    // Functional groups and pixel data.
    pub const SHARED_FUNCTIONAL_GROUPS_SEQUENCE: Tag = tag(0x5200, 0x9229);
    pub const PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE: Tag = tag(0x5200, 0x9230);
    pub const EXTENDED_OFFSET_TABLE: Tag = tag(0x7FE0, 0x0001);
    pub const EXTENDED_OFFSET_TABLE_LENGTHS: Tag = tag(0x7FE0, 0x0002);
    pub const PIXEL_DATA: Tag = tag(0x7FE0, 0x0010);
}

/// Keyword for a tag, or `None` if it isn't in the dictionary below.
pub fn keyword(tag: Tag) -> Option<&'static str> {
    DICTIONARY
        .binary_search_by_key(&tag, |(t, _)| *t)
        .ok()
        .map(|i| DICTIONARY[i].1)
}

/// Tag → keyword dictionary.
///
/// The full DICOM dictionary is ~4000 entries; this is the subset the VL Whole
/// Slide Microscopy Image IOD actually uses (derived from the tags present in
/// the reference slides), plus the attributes this driver keys on. It exists
/// only to name properties: tags outside it are skipped in the property dump,
/// the same way the C driver skips tags libdicom can't name. Sorted by tag —
/// [`keyword`] binary-searches it.
static DICTIONARY: &[(Tag, &str)] = &[
    (0x0002_0000, "FileMetaInformationGroupLength"),
    (0x0002_0001, "FileMetaInformationVersion"),
    (0x0002_0002, "MediaStorageSOPClassUID"),
    (0x0002_0003, "MediaStorageSOPInstanceUID"),
    (0x0002_0010, "TransferSyntaxUID"),
    (0x0002_0012, "ImplementationClassUID"),
    (0x0002_0013, "ImplementationVersionName"),
    (0x0002_0016, "SourceApplicationEntityTitle"),
    (0x0008_0008, "ImageType"),
    (0x0008_0016, "SOPClassUID"),
    (0x0008_0018, "SOPInstanceUID"),
    (0x0008_0020, "StudyDate"),
    (0x0008_0023, "ContentDate"),
    (0x0008_002A, "AcquisitionDateTime"),
    (0x0008_0030, "StudyTime"),
    (0x0008_0033, "ContentTime"),
    (0x0008_0050, "AccessionNumber"),
    (0x0008_0060, "Modality"),
    (0x0008_0070, "Manufacturer"),
    (0x0008_0090, "ReferringPhysicianName"),
    (0x0008_0100, "CodeValue"),
    (0x0008_0102, "CodingSchemeDesignator"),
    (0x0008_0104, "CodeMeaning"),
    (0x0008_0119, "LongCodeValue"),
    (0x0008_1090, "ManufacturerModelName"),
    (0x0008_9206, "VolumetricProperties"),
    (0x0010_0010, "PatientName"),
    (0x0010_0020, "PatientID"),
    (0x0010_0030, "PatientBirthDate"),
    (0x0010_0040, "PatientSex"),
    (0x0018_0050, "SliceThickness"),
    (0x0018_1000, "DeviceSerialNumber"),
    (0x0018_1020, "SoftwareVersions"),
    (0x0018_1210, "ConvolutionKernel"),
    (0x0018_9073, "AcquisitionDuration"),
    (0x0018_9074, "FrameAcquisitionDateTime"),
    (0x0018_9151, "FrameReferenceDateTime"),
    (0x0018_9220, "FrameAcquisitionDuration"),
    (0x0020_000D, "StudyInstanceUID"),
    (0x0020_000E, "SeriesInstanceUID"),
    (0x0020_0010, "StudyID"),
    (0x0020_0011, "SeriesNumber"),
    (0x0020_0013, "InstanceNumber"),
    (0x0020_0052, "FrameOfReferenceUID"),
    (0x0020_1040, "PositionReferenceIndicator"),
    (0x0020_9111, "FrameContentSequence"),
    (0x0020_9157, "DimensionIndexValues"),
    (0x0020_9161, "ConcatenationUID"),
    (0x0020_9162, "InConcatenationNumber"),
    (0x0020_9163, "InConcatenationTotalNumber"),
    (0x0020_9164, "DimensionOrganizationUID"),
    (0x0020_9165, "DimensionIndexPointer"),
    (0x0020_9167, "FunctionalGroupPointer"),
    (0x0020_9221, "DimensionOrganizationSequence"),
    (0x0020_9222, "DimensionIndexSequence"),
    (0x0020_9228, "ConcatenationFrameOffsetNumber"),
    (0x0020_9311, "DimensionOrganizationType"),
    (0x0022_0016, "IlluminationTypeCodeSequence"),
    (0x0022_0019, "LensesCodeSequence"),
    (0x0028_0002, "SamplesPerPixel"),
    (0x0028_0004, "PhotometricInterpretation"),
    (0x0028_0006, "PlanarConfiguration"),
    (0x0028_0008, "NumberOfFrames"),
    (0x0028_0010, "Rows"),
    (0x0028_0011, "Columns"),
    (0x0028_0030, "PixelSpacing"),
    (0x0028_0100, "BitsAllocated"),
    (0x0028_0101, "BitsStored"),
    (0x0028_0102, "HighBit"),
    (0x0028_0103, "PixelRepresentation"),
    (0x0028_0301, "BurnedInAnnotation"),
    (0x0028_2000, "ICCProfile"),
    (0x0028_2110, "LossyImageCompression"),
    (0x0028_2112, "LossyImageCompressionRatio"),
    (0x0028_2114, "LossyImageCompressionMethod"),
    (0x0028_9110, "PixelMeasuresSequence"),
    (0x0040_0512, "ContainerIdentifier"),
    (0x0040_0513, "IssuerOfTheContainerIdentifierSequence"),
    (0x0040_0518, "ContainerTypeCodeSequence"),
    (0x0040_0551, "SpecimenIdentifier"),
    (0x0040_0554, "SpecimenUID"),
    (0x0040_0555, "AcquisitionContextSequence"),
    (0x0040_0560, "SpecimenDescriptionSequence"),
    (0x0040_0562, "IssuerOfTheSpecimenIdentifierSequence"),
    (0x0040_072A, "XOffsetInSlideCoordinateSystem"),
    (0x0040_073A, "YOffsetInSlideCoordinateSystem"),
    (0x0040_074A, "ZOffsetInSlideCoordinateSystem"),
    (0x0040_08EA, "MeasurementUnitsCodeSequence"),
    (0x0040_A040, "ValueType"),
    (0x0040_A043, "ConceptNameCodeSequence"),
    (0x0040_A160, "TextValue"),
    (0x0040_A30A, "NumericValue"),
    (0x0048_0001, "ImagedVolumeWidth"),
    (0x0048_0002, "ImagedVolumeHeight"),
    (0x0048_0003, "ImagedVolumeDepth"),
    (0x0048_0006, "TotalPixelMatrixColumns"),
    (0x0048_0007, "TotalPixelMatrixRows"),
    (0x0048_0008, "TotalPixelMatrixOriginSequence"),
    (0x0048_0010, "SpecimenLabelInImage"),
    (0x0048_0011, "FocusMethod"),
    (0x0048_0012, "ExtendedDepthOfField"),
    (0x0048_0015, "RecommendedAbsentPixelCIELabValue"),
    (0x0048_0100, "IlluminatorTypeCodeSequence"),
    (0x0048_0102, "ImageOrientationSlide"),
    (0x0048_0105, "OpticalPathSequence"),
    (0x0048_0106, "OpticalPathIdentifier"),
    (0x0048_0107, "OpticalPathDescription"),
    (0x0048_0108, "IlluminationColorCodeSequence"),
    (0x0048_0112, "ObjectiveLensPower"),
    (0x0048_0113, "ObjectiveLensNumericalAperture"),
    (0x0048_0207, "OpticalPathIdentificationSequence"),
    (0x0048_021A, "PlanePositionSlideSequence"),
    (0x0048_021E, "ColumnPositionInTotalImagePixelMatrix"),
    (0x0048_021F, "RowPositionInTotalImagePixelMatrix"),
    (0x0048_0302, "NumberOfOpticalPaths"),
    (0x0048_0303, "TotalPixelMatrixFocalPlanes"),
    (0x2200_0002, "LabelText"),
    (0x2200_0005, "BarcodeValue"),
    (0x5200_9229, "SharedFunctionalGroupsSequence"),
    (0x5200_9230, "PerFrameFunctionalGroupsSequence"),
    (0x7FE0_0001, "ExtendedOffsetTable"),
    (0x7FE0_0002, "ExtendedOffsetTableLengths"),
    (0x7FE0_0010, "PixelData"),
];

/// SOP Class UID of a VL Whole Slide Microscopy Image instance.
pub const VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE: &str = "1.2.840.10008.5.1.4.1.1.77.1.6";

/// Implicit-VR little-endian: dataset encoding this parser does not support.
const IMPLICIT_VR_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2";
/// Explicit-VR big-endian (retired): likewise unsupported.
const EXPLICIT_VR_BIG_ENDIAN: &str = "1.2.840.10008.1.2.2";

/// A value representation: the two ASCII bytes as they appear in the file.
pub type Vr = [u8; 2];

/// VRs whose header is 12 bytes (2 reserved bytes then a 32-bit length).
fn is_long_form(vr: Vr) -> bool {
    matches!(
        &vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    )
}

/// Text VRs that hold exactly one value, in which a backslash is an ordinary
/// character rather than a value separator.
fn is_single_valued_text(vr: Vr) -> bool {
    matches!(&vr, b"LT" | b"ST" | b"UT")
}

/// VRs holding text. Multi-valued text is `\`-separated in the file.
fn is_text(vr: Vr) -> bool {
    matches!(
        &vr,
        b"AE"
            | b"AS"
            | b"CS"
            | b"DA"
            | b"DS"
            | b"DT"
            | b"IS"
            | b"LO"
            | b"LT"
            | b"PN"
            | b"SH"
            | b"ST"
            | b"TM"
            | b"UC"
            | b"UI"
            | b"UR"
            | b"UT"
    )
}

/// An element's parsed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A text VR, `\`-separated when multi-valued, padding trimmed.
    Text(String),
    /// A binary integer VR (`US`/`SS`/`UL`/`SL`/`UV`/`SV`/`AT`).
    Ints(Vec<i64>),
    /// A binary float VR (`FL`/`FD`).
    Floats(Vec<f64>),
    /// An opaque byte VR (`OB`/`OW`/`UN`/…), e.g. an ICC profile.
    Bytes(Vec<u8>),
    /// A sequence's items.
    Items(Vec<DataSet>),
    /// Present in the file but deliberately not retained: bulk pixel data, and
    /// the per-frame functional groups (see the module docs).
    Elided,
}

/// One parsed element.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub tag: Tag,
    pub vr: Vr,
    pub value: Value,
}

impl Element {
    /// The `index`-th value of a text element, trimmed. `None` for non-text
    /// VRs, or past the last value.
    pub fn text_at(&self, index: usize) -> Option<&str> {
        let Value::Text(text) = &self.value else {
            return None;
        };
        if is_single_valued_text(self.vr) {
            // A backslash here is content, not a separator.
            return (index == 0).then(|| text.trim());
        }
        text.split('\\').nth(index).map(str::trim)
    }

    /// Every value of this element, rendered as strings. Empty for VRs with no
    /// meaningful string form (sequences, opaque bytes, elided bulk data).
    pub fn value_strings(&self) -> Vec<String> {
        match &self.value {
            Value::Text(text) if is_single_valued_text(self.vr) => vec![text.trim().to_string()],
            Value::Text(text) => text.split('\\').map(|s| s.trim().to_string()).collect(),
            Value::Ints(values) => values.iter().map(i64::to_string).collect(),
            Value::Floats(values) => values.iter().map(f64::to_string).collect(),
            Value::Bytes(_) | Value::Items(_) | Value::Elided => Vec::new(),
        }
    }
}

/// A DICOM dataset: elements in file (tag) order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataSet {
    elements: Vec<Element>,
}

impl DataSet {
    /// Elements in file order.
    pub fn iter(&self) -> impl Iterator<Item = &Element> {
        self.elements.iter()
    }

    /// The element with this tag, VR included.
    pub fn element(&self, tag: Tag) -> Option<&Element> {
        self.elements.iter().find(|e| e.tag == tag)
    }

    pub fn get(&self, tag: Tag) -> Option<&Value> {
        self.element(tag).map(|e| &e.value)
    }

    /// The `index`-th value of a text element (`\`-separated), trimmed.
    pub fn text_at(&self, tag: Tag, index: usize) -> Option<&str> {
        self.element(tag)?.text_at(index)
    }

    /// The first value of a text element.
    pub fn text(&self, tag: Tag) -> Option<&str> {
        self.text_at(tag, 0)
    }

    /// An integer value, from either a binary integer VR or a numeric-text VR
    /// (`IS`/`DS`), matching libdicom's integer accessor.
    pub fn int_at(&self, tag: Tag, index: usize) -> Option<i64> {
        match self.get(tag)? {
            Value::Ints(v) => v.get(index).copied(),
            Value::Floats(v) => v.get(index).map(|f| *f as i64),
            Value::Text(_) => self.text_at(tag, index)?.parse().ok(),
            _ => None,
        }
    }

    pub fn int(&self, tag: Tag) -> Option<i64> {
        self.int_at(tag, 0)
    }

    /// A floating-point value, from either a binary float VR or `DS`/`IS` text.
    pub fn float_at(&self, tag: Tag, index: usize) -> Option<f64> {
        match self.get(tag)? {
            Value::Floats(v) => v.get(index).copied(),
            Value::Ints(v) => v.get(index).map(|i| *i as f64),
            Value::Text(_) => self.text_at(tag, index)?.parse().ok(),
            _ => None,
        }
    }

    pub fn float(&self, tag: Tag) -> Option<f64> {
        self.float_at(tag, 0)
    }

    pub fn bytes(&self, tag: Tag) -> Option<&[u8]> {
        match self.get(tag)? {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn items(&self, tag: Tag) -> Option<&[DataSet]> {
        match self.get(tag)? {
            Value::Items(items) => Some(items),
            _ => None,
        }
    }

    /// The `index`-th item of a sequence.
    pub fn item(&self, tag: Tag, index: usize) -> Option<&DataSet> {
        self.items(tag)?.get(index)
    }

    fn push(&mut self, element: Element) {
        self.elements.push(element);
    }
}

impl FromIterator<Element> for DataSet {
    fn from_iter<I: IntoIterator<Item = Element>>(iter: I) -> Self {
        Self {
            elements: iter.into_iter().collect(),
        }
    }
}

/// Where a frame's encoded bytes live in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub offset: u64,
    pub length: u32,
}

/// The pixel data element, recorded by location rather than loaded.
#[derive(Debug, Clone, PartialEq)]
pub enum PixelData {
    /// Uncompressed: one defined-length blob of `frame_count` equal-size frames.
    Native { offset: u64, length: u64 },
    /// Encapsulated: one item per compressed fragment, plus the Basic Offset
    /// Table's contents (empty when the BOT item is zero-length, which is
    /// common — the offsets are then recovered by walking the fragments).
    Encapsulated {
        fragments: Vec<Frame>,
        basic_offset_table: Vec<u32>,
    },
}

/// A parsed DICOM instance.
#[derive(Debug)]
pub struct DicomObject {
    /// The file meta group (group `0002`).
    pub file_meta: DataSet,
    /// The dataset, minus the elided bulk elements (see the module docs).
    pub dataset: DataSet,
    /// TransferSyntaxUID from the file meta group.
    pub transfer_syntax: String,
    /// Location of the pixel data, if the instance has any.
    pub pixel_data: Option<PixelData>,
    /// 1-based `(column, row)` position of each frame in the total pixel matrix,
    /// from `PerFrameFunctionalGroupsSequence`. Empty when that sequence is
    /// absent, which is how a `TILED_FULL` level is laid out.
    pub frame_positions: Vec<(i64, i64)>,
}

impl DicomObject {
    /// Byte location of each of `frame_count` frames.
    ///
    /// Prefers the Extended Offset Table (`(7FE0,0001)`/`(7FE0,0002)`) when the
    /// instance carries one; otherwise uses the fragment walk, which requires
    /// one fragment per frame — the layout WSI encoders produce, and the only
    /// one resolvable without an offset table. Native (uncompressed) pixel data
    /// is sliced into `frame_count` equal parts.
    pub fn frame_locations(&self, frame_count: usize, frame_size: u64) -> Result<Vec<Frame>> {
        let pixel_data = self
            .pixel_data
            .as_ref()
            .ok_or_else(|| OpenSlideError::Decode("DICOM instance has no pixel data".into()))?;

        match pixel_data {
            PixelData::Native { offset, length } => {
                let needed = frame_size
                    .checked_mul(frame_count as u64)
                    .ok_or_else(|| OpenSlideError::Decode("DICOM frame size overflow".into()))?;
                if *length < needed {
                    return Err(OpenSlideError::Decode(format!(
                        "DICOM pixel data is {length} bytes, need {needed} for {frame_count} frames"
                    )));
                }
                Ok((0..frame_count as u64)
                    .map(|i| Frame {
                        offset: offset + i * frame_size,
                        length: frame_size as u32,
                    })
                    .collect())
            }
            PixelData::Encapsulated { fragments, .. } => {
                if let Some(frames) = self.extended_offset_table(frame_count)? {
                    return Ok(frames);
                }
                if fragments.len() != frame_count {
                    return Err(OpenSlideError::Decode(format!(
                        "DICOM pixel data has {} fragments for {frame_count} frames and no \
                         offset table; multi-fragment frames are unsupported",
                        fragments.len()
                    )));
                }
                Ok(fragments.clone())
            }
        }
    }

    /// Frame locations from the Extended Offset Table, if present and usable.
    ///
    /// Offsets in the table are relative to the first fragment's item tag, the
    /// same origin the Basic Offset Table uses.
    fn extended_offset_table(&self, frame_count: usize) -> Result<Option<Vec<Frame>>> {
        let (Some(offsets), Some(lengths)) = (
            self.dataset.bytes(tags::EXTENDED_OFFSET_TABLE),
            self.dataset.bytes(tags::EXTENDED_OFFSET_TABLE_LENGTHS),
        ) else {
            return Ok(None);
        };
        let Some(PixelData::Encapsulated { fragments, .. }) = self.pixel_data.as_ref() else {
            return Ok(None);
        };
        let Some(first) = fragments.first() else {
            return Ok(None);
        };
        // Fragment offsets point at the item tag, 8 bytes before the data.
        let origin = first.offset - 8;

        if offsets.len() != lengths.len() || offsets.len() != frame_count * 8 {
            return Err(OpenSlideError::Decode(format!(
                "DICOM extended offset table has {} offset / {} length bytes for {frame_count} frames",
                offsets.len(),
                lengths.len()
            )));
        }
        let read_u64 = |b: &[u8], i: usize| {
            u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().expect("8-byte slice"))
        };
        Ok(Some(
            (0..frame_count)
                .map(|i| Frame {
                    offset: origin + read_u64(offsets, i) + 8,
                    length: read_u64(lengths, i) as u32,
                })
                .collect(),
        ))
    }
}

/// Read and validate the preamble and the file meta group.
///
/// Returns the meta group's dataset and the offset at which the main dataset
/// starts. This is the cheap path used for format detection: it touches only
/// the first few hundred bytes.
pub fn parse_file_meta(reader: &dyn SlideReader) -> Result<(DataSet, u64)> {
    let mut cur = Cursor::new(reader)?;
    if cur.size < 132 {
        return Err(OpenSlideError::Decode("file too short to be DICOM".into()));
    }
    cur.seek(128);
    if cur.take(4)? != b"DICM" {
        return Err(OpenSlideError::Decode("missing DICM magic".into()));
    }

    // The meta group must open with its own length, so we know where it ends.
    let header = read_element_header(&mut cur)?;
    if header.tag != tags::FILE_META_GROUP_LENGTH {
        return Err(OpenSlideError::Decode(
            "DICOM file meta group does not start with its group length".into(),
        ));
    }
    let group_length = match value_from_bytes(header.vr, cur.take(header.length as usize)?) {
        Value::Ints(v) if !v.is_empty() => v[0] as u64,
        _ => {
            return Err(OpenSlideError::Decode(
                "DICOM file meta group length is not an integer".into(),
            ));
        }
    };

    let meta_start = cur.pos();
    let meta_end = meta_start
        .checked_add(group_length)
        .filter(|end| *end <= cur.size)
        .ok_or_else(|| OpenSlideError::Decode("DICOM file meta group runs past EOF".into()))?;

    let mut meta = DataSet::default();
    let mut ctx = ParseContext::nested();
    parse_dataset(&mut cur, Some(meta_end), &mut meta, &mut ctx)?;
    Ok((meta, meta_end))
}

/// Parse a whole DICOM instance: meta group, dataset, and pixel data location.
pub fn parse(reader: &dyn SlideReader) -> Result<DicomObject> {
    let (file_meta, dataset_start) = parse_file_meta(reader)?;

    let transfer_syntax = file_meta
        .text(tags::TRANSFER_SYNTAX_UID)
        .ok_or_else(|| OpenSlideError::Decode("DICOM has no TransferSyntaxUID".into()))?
        .to_string();
    if transfer_syntax == IMPLICIT_VR_LITTLE_ENDIAN || transfer_syntax == EXPLICIT_VR_BIG_ENDIAN {
        return Err(OpenSlideError::Decode(format!(
            "unsupported DICOM dataset encoding {transfer_syntax} (only explicit-VR \
             little-endian datasets are supported)"
        )));
    }

    let mut cur = Cursor::new(reader)?;
    cur.seek(dataset_start);
    let mut dataset = DataSet::default();
    let mut ctx = ParseContext::top_level();
    let end = cur.size;
    parse_dataset(&mut cur, Some(end), &mut dataset, &mut ctx)?;

    Ok(DicomObject {
        file_meta,
        dataset,
        transfer_syntax,
        pixel_data: ctx.pixel_data,
        frame_positions: ctx.frame_positions,
    })
}

// ─────────────────────────────── parsing ───────────────────────────────────

/// Side channel for the two elements handled specially at the top level.
struct ParseContext {
    top_level: bool,
    pixel_data: Option<PixelData>,
    frame_positions: Vec<(i64, i64)>,
}

impl ParseContext {
    fn top_level() -> Self {
        Self {
            top_level: true,
            pixel_data: None,
            frame_positions: Vec::new(),
        }
    }

    fn nested() -> Self {
        Self {
            top_level: false,
            pixel_data: None,
            frame_positions: Vec::new(),
        }
    }
}

struct ElementHeader {
    tag: Tag,
    vr: Vr,
    length: u32,
}

/// Read one element header. Items and delimiters (group `FFFE`) carry no VR.
fn read_element_header(cur: &mut Cursor) -> Result<ElementHeader> {
    let group = cur.u16()?;
    let element = cur.u16()?;
    let tag = tag(group, element);
    if group == 0xFFFE {
        return Ok(ElementHeader {
            tag,
            vr: *b"  ",
            length: cur.u32()?,
        });
    }
    let vr = cur.take(2)?;
    let vr: Vr = [vr[0], vr[1]];
    let length = if is_long_form(vr) {
        cur.u16()?; // reserved
        cur.u32()?
    } else {
        cur.u16()? as u32
    };
    Ok(ElementHeader { tag, vr, length })
}

/// Parse elements into `out`.
///
/// `end` bounds a defined-length range; `None` means "until an item delimiter",
/// which is how an undefined-length sequence item ends.
fn parse_dataset(
    cur: &mut Cursor,
    end: Option<u64>,
    out: &mut DataSet,
    ctx: &mut ParseContext,
) -> Result<()> {
    loop {
        match end {
            Some(end) if cur.pos() >= end => return Ok(()),
            // A truncated trailing element is more likely than a file that ends
            // exactly on a boundary; either way there is nothing more to read.
            None if cur.remaining() < 8 => return Ok(()),
            _ => {}
        }
        if end.is_some() && cur.remaining() < 8 {
            return Ok(());
        }

        let header = read_element_header(cur)?;
        match header.tag {
            ITEM_DELIMITER | SEQUENCE_DELIMITER => return Ok(()),
            _ => {}
        }

        // Pixel data is checked before the sequence branch: encapsulated pixel
        // data is an `OB` element with an undefined length, which otherwise
        // looks exactly like an item stream.
        if ctx.top_level && header.tag == tags::PIXEL_DATA {
            ctx.pixel_data = Some(parse_pixel_data(cur, header.length)?);
            out.push(Element {
                tag: header.tag,
                vr: header.vr,
                value: Value::Elided,
            });
            continue;
        }

        if header.vr == *b"SQ" || (header.length == UNDEFINED_LENGTH && header.tag != ITEM) {
            // Sequences, and any other element that uses undefined length (a
            // `UN`-typed sequence from an implicit-VR conversion), are item
            // streams.
            if ctx.top_level && header.tag == tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE {
                collect_frame_positions(cur, header.length, ctx)?;
                out.push(Element {
                    tag: header.tag,
                    vr: header.vr,
                    value: Value::Elided,
                });
            } else {
                let items = parse_items(cur, header.length)?;
                out.push(Element {
                    tag: header.tag,
                    vr: header.vr,
                    value: Value::Items(items),
                });
            }
            continue;
        }

        let bytes = cur.take(header.length as usize)?;
        out.push(Element {
            tag: header.tag,
            vr: header.vr,
            value: value_from_bytes(header.vr, bytes),
        });
    }
}

/// Parse a sequence's items. `length` may be [`UNDEFINED_LENGTH`].
fn parse_items(cur: &mut Cursor, length: u32) -> Result<Vec<DataSet>> {
    let end = if length == UNDEFINED_LENGTH {
        None
    } else {
        Some(cur.pos() + length as u64)
    };
    let mut items = Vec::new();
    let mut ctx = ParseContext::nested();
    loop {
        match end {
            Some(end) if cur.pos() >= end => return Ok(items),
            None if cur.remaining() < 8 => return Ok(items),
            _ => {}
        }
        let header = read_element_header(cur)?;
        match header.tag {
            SEQUENCE_DELIMITER => return Ok(items),
            ITEM => {}
            other => {
                return Err(OpenSlideError::Decode(format!(
                    "expected DICOM sequence item, found tag ({:04X},{:04X})",
                    other >> 16,
                    other & 0xFFFF
                )));
            }
        }
        let mut item = DataSet::default();
        let item_end = if header.length == UNDEFINED_LENGTH {
            None
        } else {
            Some(cur.pos() + header.length as u64)
        };
        parse_dataset(cur, item_end, &mut item, &mut ctx)?;
        if let Some(item_end) = item_end {
            cur.seek(item_end);
        }
        items.push(item);
    }
}

/// Walk `PerFrameFunctionalGroupsSequence`, keeping only each frame's position.
///
/// The items themselves are parsed and dropped: on a sparse level there is one
/// per frame and they dominate the file's metadata (tens of MiB), while the read
/// path only needs the position that maps a frame to a tile.
fn collect_frame_positions(cur: &mut Cursor, length: u32, ctx: &mut ParseContext) -> Result<()> {
    let end = if length == UNDEFINED_LENGTH {
        None
    } else {
        Some(cur.pos() + length as u64)
    };
    let mut nested = ParseContext::nested();
    loop {
        match end {
            Some(end) if cur.pos() >= end => return Ok(()),
            None if cur.remaining() < 8 => return Ok(()),
            _ => {}
        }
        let header = read_element_header(cur)?;
        match header.tag {
            SEQUENCE_DELIMITER => return Ok(()),
            ITEM => {}
            other => {
                return Err(OpenSlideError::Decode(format!(
                    "expected DICOM per-frame item, found tag ({:04X},{:04X})",
                    other >> 16,
                    other & 0xFFFF
                )));
            }
        }
        let item_end = if header.length == UNDEFINED_LENGTH {
            None
        } else {
            Some(cur.pos() + header.length as u64)
        };
        let mut item = DataSet::default();
        parse_dataset(cur, item_end, &mut item, &mut nested)?;
        if let Some(item_end) = item_end {
            cur.seek(item_end);
        }

        let position = item
            .item(tags::PLANE_POSITION_SLIDE_SEQUENCE, 0)
            .and_then(|p| {
                Some((
                    p.int(tags::COLUMN_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX)?,
                    p.int(tags::ROW_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX)?,
                ))
            })
            .ok_or_else(|| {
                OpenSlideError::Decode(
                    "DICOM per-frame functional group has no plane position".into(),
                )
            })?;
        ctx.frame_positions.push(position);
    }
}

/// Record where the pixel data is without reading any of it.
fn parse_pixel_data(cur: &mut Cursor, length: u32) -> Result<PixelData> {
    if length != UNDEFINED_LENGTH {
        let offset = cur.pos();
        cur.skip(length as u64);
        return Ok(PixelData::Native {
            offset,
            length: length as u64,
        });
    }

    // Encapsulated: a Basic Offset Table item, then one item per fragment.
    let header = read_element_header(cur)?;
    if header.tag != ITEM {
        return Err(OpenSlideError::Decode(
            "encapsulated DICOM pixel data does not start with the basic offset table".into(),
        ));
    }
    let bot_bytes = cur.take(header.length as usize)?;
    let basic_offset_table = bot_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().expect("4-byte chunk")))
        .collect();

    let mut fragments = Vec::new();
    while cur.remaining() >= 8 {
        let header = read_element_header(cur)?;
        match header.tag {
            SEQUENCE_DELIMITER => break,
            ITEM => {}
            other => {
                return Err(OpenSlideError::Decode(format!(
                    "expected DICOM pixel data fragment, found tag ({:04X},{:04X})",
                    other >> 16,
                    other & 0xFFFF
                )));
            }
        }
        fragments.push(Frame {
            offset: cur.pos(),
            length: header.length,
        });
        cur.skip(header.length as u64);
    }

    Ok(PixelData::Encapsulated {
        fragments,
        basic_offset_table,
    })
}

/// Decode an element's raw bytes according to its VR.
fn value_from_bytes(vr: Vr, bytes: Vec<u8>) -> Value {
    if is_text(vr) {
        // Text is padded to an even length with a space, or NUL for UI.
        let text = String::from_utf8_lossy(&bytes);
        return Value::Text(text.trim_end_matches(['\0', ' ']).to_string());
    }
    match &vr {
        b"US" => Value::Ints(ints(&bytes, 2, |c| u16::from_le_bytes([c[0], c[1]]) as i64)),
        b"SS" => Value::Ints(ints(&bytes, 2, |c| i16::from_le_bytes([c[0], c[1]]) as i64)),
        b"UL" => Value::Ints(ints(&bytes, 4, |c| {
            u32::from_le_bytes(c.try_into().expect("4 bytes")) as i64
        })),
        b"SL" => Value::Ints(ints(&bytes, 4, |c| {
            i32::from_le_bytes(c.try_into().expect("4 bytes")) as i64
        })),
        b"UV" | b"SV" => Value::Ints(ints(&bytes, 8, |c| {
            i64::from_le_bytes(c.try_into().expect("8 bytes"))
        })),
        // AT is a tag value: two u16s, group then element.
        b"AT" => Value::Ints(ints(&bytes, 4, |c| {
            (tag(
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
            )) as i64
        })),
        b"FL" => Value::Floats(floats(&bytes, 4, |c| {
            f32::from_le_bytes(c.try_into().expect("4 bytes")) as f64
        })),
        b"FD" => Value::Floats(floats(&bytes, 8, |c| {
            f64::from_le_bytes(c.try_into().expect("8 bytes"))
        })),
        _ => Value::Bytes(bytes),
    }
}

fn ints(bytes: &[u8], width: usize, f: impl Fn(&[u8]) -> i64) -> Vec<i64> {
    bytes.chunks_exact(width).map(f).collect()
}

fn floats(bytes: &[u8], width: usize, f: impl Fn(&[u8]) -> f64) -> Vec<f64> {
    bytes.chunks_exact(width).map(f).collect()
}

// ─────────────────────────────── cursor ────────────────────────────────────

/// Buffered forward reader over a [`SlideReader`].
///
/// DICOM parsing is a long run of 2–8 byte reads, so a raw positioned read per
/// field would be one syscall per field. Small reads come out of a 64 KiB
/// window; values larger than the window bypass it and are read directly.
struct Cursor<'r> {
    reader: &'r dyn SlideReader,
    size: u64,
    pos: u64,
    buf: Vec<u8>,
    /// Absolute file offset of `buf[0]`.
    buf_start: u64,
}

const WINDOW: usize = 64 * 1024;

impl<'r> Cursor<'r> {
    fn new(reader: &'r dyn SlideReader) -> Result<Self> {
        Ok(Self {
            size: reader.size()?,
            reader,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
        })
    }

    fn pos(&self) -> u64 {
        self.pos
    }

    fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    fn skip(&mut self, n: u64) {
        self.pos = self.pos.saturating_add(n);
    }

    fn remaining(&self) -> u64 {
        self.size.saturating_sub(self.pos)
    }

    fn eof() -> OpenSlideError {
        OpenSlideError::Decode("unexpected end of DICOM file".into())
    }

    /// Make at least `n` bytes at `pos` available in the window.
    fn fill(&mut self, n: usize) -> Result<()> {
        if let Some(off) = self.pos.checked_sub(self.buf_start) {
            if off as usize + n <= self.buf.len() {
                return Ok(());
            }
        }
        if self.remaining() < n as u64 {
            return Err(Self::eof());
        }
        let want = n.max(WINDOW).min(self.remaining() as usize);
        self.buf.resize(want, 0);
        self.reader.read_exact_at(self.pos, &mut self.buf)?;
        self.buf_start = self.pos;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<Vec<u8>> {
        if n > WINDOW {
            if self.remaining() < n as u64 {
                return Err(Self::eof());
            }
            let mut out = vec![0u8; n];
            self.reader.read_exact_at(self.pos, &mut out)?;
            self.pos += n as u64;
            return Ok(out);
        }
        self.fill(n)?;
        let off = (self.pos - self.buf_start) as usize;
        let out = self.buf[off..off + n].to_vec();
        self.pos += n as u64;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16> {
        self.fill(2)?;
        let off = (self.pos - self.buf_start) as usize;
        let v = u16::from_le_bytes([self.buf[off], self.buf[off + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32> {
        self.fill(4)?;
        let off = (self.pos - self.buf_start) as usize;
        let v = u32::from_le_bytes(self.buf[off..off + 4].try_into().expect("4 bytes"));
        self.pos += 4;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemReader(Vec<u8>);

    impl SlideReader for MemReader {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }

        fn size(&self) -> Result<u64> {
            Ok(self.0.len() as u64)
        }
    }

    /// Builder for the byte layout the parser expects, so tests can state a
    /// file's structure instead of hand-rolling headers.
    #[derive(Default)]
    struct Builder(Vec<u8>);

    impl Builder {
        fn element(&mut self, tag: Tag, vr: &[u8; 2], value: &[u8]) -> &mut Self {
            self.0
                .extend_from_slice(&((tag >> 16) as u16).to_le_bytes());
            self.0
                .extend_from_slice(&((tag & 0xFFFF) as u16).to_le_bytes());
            self.0.extend_from_slice(vr);
            if is_long_form(*vr) {
                self.0.extend_from_slice(&0u16.to_le_bytes());
                self.0
                    .extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                self.0
                    .extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            self.0.extend_from_slice(value);
            self
        }

        /// An element whose length is undefined (a sequence or encapsulated
        /// pixel data); `value` must already contain the items and delimiter.
        fn undefined(&mut self, tag: Tag, vr: &[u8; 2], value: &[u8]) -> &mut Self {
            self.0
                .extend_from_slice(&((tag >> 16) as u16).to_le_bytes());
            self.0
                .extend_from_slice(&((tag & 0xFFFF) as u16).to_le_bytes());
            self.0.extend_from_slice(vr);
            self.0.extend_from_slice(&0u16.to_le_bytes());
            self.0.extend_from_slice(&UNDEFINED_LENGTH.to_le_bytes());
            self.0.extend_from_slice(value);
            self
        }

        fn take(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.0)
        }
    }

    /// `(FFFE,E000)` item header with a defined length.
    fn item(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE000u16.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn sequence_delimiter() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE0DDu16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    /// Wrap a dataset in a preamble, magic, and a file meta group declaring
    /// `transfer_syntax`.
    fn file(transfer_syntax: &str, dataset: &[u8]) -> MemReader {
        let mut meta = Builder::default();
        meta.element(
            tags::MEDIA_STORAGE_SOP_CLASS_UID,
            b"UI",
            pad(VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE).as_bytes(),
        );
        meta.element(
            tags::TRANSFER_SYNTAX_UID,
            b"UI",
            pad(transfer_syntax).as_bytes(),
        );
        let meta = meta.take();

        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        let mut group_length = Builder::default();
        group_length.element(
            tags::FILE_META_GROUP_LENGTH,
            b"UL",
            &(meta.len() as u32).to_le_bytes(),
        );
        out.extend_from_slice(&group_length.take());
        out.extend_from_slice(&meta);
        out.extend_from_slice(dataset);
        MemReader(out)
    }

    /// DICOM values are even-length; UIDs pad with NUL.
    fn pad(s: &str) -> String {
        if s.len() % 2 == 0 {
            s.to_string()
        } else {
            format!("{s}\0")
        }
    }

    #[test]
    fn parses_file_meta_and_scalar_values() {
        let mut ds = Builder::default();
        ds.element(
            tags::IMAGE_TYPE,
            b"CS",
            b"DERIVED\\PRIMARY\\VOLUME\\RESAMPLED",
        )
        .element(tags::COLUMNS, b"US", &256u16.to_le_bytes())
        .element(tags::ROWS, b"US", &256u16.to_le_bytes())
        .element(
            tags::TOTAL_PIXEL_MATRIX_COLUMNS,
            b"UL",
            &1000u32.to_le_bytes(),
        )
        .element(tags::NUMBER_OF_FRAMES, b"IS", b"12")
        .element(tags::OBJECTIVE_LENS_POWER, b"DS", b"40");
        let reader = file("1.2.840.10008.1.2.4.50", &ds.take());

        let obj = parse(&reader).expect("parse");
        assert_eq!(obj.transfer_syntax, "1.2.840.10008.1.2.4.50");
        assert_eq!(
            obj.file_meta.text(tags::MEDIA_STORAGE_SOP_CLASS_UID),
            Some(VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE)
        );
        // Image flavor is ImageType's third value.
        assert_eq!(obj.dataset.text_at(tags::IMAGE_TYPE, 2), Some("VOLUME"));
        assert_eq!(obj.dataset.int(tags::COLUMNS), Some(256));
        assert_eq!(
            obj.dataset.int(tags::TOTAL_PIXEL_MATRIX_COLUMNS),
            Some(1000)
        );
        // IS/DS text VRs are readable as numbers.
        assert_eq!(obj.dataset.int(tags::NUMBER_OF_FRAMES), Some(12));
        assert_eq!(obj.dataset.float(tags::OBJECTIVE_LENS_POWER), Some(40.0));
        assert!(obj.pixel_data.is_none());
    }

    #[test]
    fn rejects_non_dicom_and_unsupported_encodings() {
        let junk = MemReader(vec![0u8; 4096]);
        assert!(parse(&junk).is_err(), "no DICM magic");

        let short = MemReader(vec![0u8; 8]);
        assert!(parse(&short).is_err(), "too short");

        // Implicit VR little-endian: the dataset carries no VRs to parse.
        let reader = file(IMPLICIT_VR_LITTLE_ENDIAN, &[]);
        let err = parse(&reader).expect_err("implicit VR must be rejected");
        assert!(
            err.to_string().contains("explicit-VR"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_nested_sequences() {
        // SharedFunctionalGroups → PixelMeasures → PixelSpacing, both a
        // defined-length sequence and an undefined-length one.
        let mut measures = Builder::default();
        measures.element(tags::PIXEL_SPACING, b"DS", b"0.001\\0.002");
        let measures_item = item(&measures.take());

        let mut inner = Builder::default();
        inner.element(tags::PIXEL_MEASURES_SEQUENCE, b"SQ", &measures_item);
        let inner_item = item(&inner.take());

        let mut ds = Builder::default();
        ds.element(tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE, b"SQ", &inner_item);
        let mut undefined_payload = inner_item.clone();
        undefined_payload.extend_from_slice(&sequence_delimiter());
        ds.undefined(tags::OPTICAL_PATH_SEQUENCE, b"SQ", &undefined_payload);
        let reader = file("1.2.840.10008.1.2.1", &ds.take());

        let obj = parse(&reader).expect("parse");
        for seq in [
            tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
            tags::OPTICAL_PATH_SEQUENCE,
        ] {
            let spacing = obj
                .dataset
                .item(seq, 0)
                .and_then(|g| g.item(tags::PIXEL_MEASURES_SEQUENCE, 0))
                .expect("pixel measures");
            assert_eq!(spacing.float_at(tags::PIXEL_SPACING, 0), Some(0.001));
            assert_eq!(spacing.float_at(tags::PIXEL_SPACING, 1), Some(0.002));
        }
    }

    /// One per-frame functional group item placing a frame at `(col, row)`.
    fn per_frame_item(col: i32, row: i32) -> Vec<u8> {
        let mut position = Builder::default();
        position
            .element(
                tags::COLUMN_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX,
                b"SL",
                &col.to_le_bytes(),
            )
            .element(
                tags::ROW_POSITION_IN_TOTAL_IMAGE_PIXEL_MATRIX,
                b"SL",
                &row.to_le_bytes(),
            );
        let mut group = Builder::default();
        group.element(
            tags::PLANE_POSITION_SLIDE_SEQUENCE,
            b"SQ",
            &item(&position.take()),
        );
        item(&group.take())
    }

    #[test]
    fn collects_frame_positions_without_retaining_the_items() {
        let mut per_frame = per_frame_item(1, 1);
        per_frame.extend_from_slice(&per_frame_item(257, 1));
        let mut ds = Builder::default();
        ds.element(
            tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
            b"SQ",
            &per_frame,
        );
        let reader = file("1.2.840.10008.1.2.4.50", &ds.take());

        let obj = parse(&reader).expect("parse");
        assert_eq!(obj.frame_positions, vec![(1, 1), (257, 1)]);
        // Elided, so the property dump never sees 8000 frames' worth of items.
        assert_eq!(
            obj.dataset.get(tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE),
            Some(&Value::Elided)
        );
    }

    #[test]
    fn locates_encapsulated_frames_by_fragment_walk() {
        // Empty basic offset table, two JPEG-ish fragments: the common layout.
        let mut payload = item(&[]);
        payload.extend_from_slice(&item(&[0xFF, 0xD8, 1, 2]));
        payload.extend_from_slice(&item(&[0xFF, 0xD8, 3, 4, 5, 6]));
        payload.extend_from_slice(&sequence_delimiter());

        let mut ds = Builder::default();
        ds.element(tags::NUMBER_OF_FRAMES, b"IS", b"2").undefined(
            tags::PIXEL_DATA,
            b"OB",
            &payload,
        );
        let reader = file("1.2.840.10008.1.2.4.50", &ds.take());

        let obj = parse(&reader).expect("parse");
        let frames = obj.frame_locations(2, 0).expect("frame locations");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].length, 4);
        assert_eq!(frames[1].length, 6);
        // The recorded offsets point at the fragment payload, past the item tag.
        let bytes = reader.0;
        assert_eq!(
            &bytes[frames[1].offset as usize..frames[1].offset as usize + 6],
            &[0xFF, 0xD8, 3, 4, 5, 6]
        );

        // A frame count that doesn't match the fragment count is an error, not
        // a silently wrong tile mapping.
        assert!(obj.frame_locations(3, 0).is_err());
    }

    #[test]
    fn slices_native_pixel_data_into_equal_frames() {
        let mut ds = Builder::default();
        ds.element(tags::ROWS, b"US", &2u16.to_le_bytes())
            .element(tags::COLUMNS, b"US", &2u16.to_le_bytes())
            .element(tags::PIXEL_DATA, b"OW", &[7u8; 24]);
        let reader = file("1.2.840.10008.1.2.1", &ds.take());

        let obj = parse(&reader).expect("parse");
        let PixelData::Native { length, .. } = obj.pixel_data.as_ref().expect("pixel data") else {
            panic!("expected native pixel data");
        };
        assert_eq!(*length, 24);

        // Two 2x2 RGB frames of 12 bytes each.
        let frames = obj.frame_locations(2, 12).expect("frame locations");
        assert_eq!(frames[1].offset, frames[0].offset + 12);
        assert_eq!(frames[0].length, 12);
        // Asking for more than fits is an error.
        assert!(obj.frame_locations(3, 12).is_err());
    }

    #[test]
    fn prefers_the_extended_offset_table() {
        let mut payload = item(&[]);
        payload.extend_from_slice(&item(&[1, 2, 3, 4]));
        payload.extend_from_slice(&item(&[5, 6, 7, 8]));
        payload.extend_from_slice(&sequence_delimiter());

        // Offsets are measured from the first fragment's item tag; declare the
        // frames in the opposite order from the fragment walk so we can tell
        // which source was used.
        let mut offsets = Vec::new();
        offsets.extend_from_slice(&12u64.to_le_bytes()); // second fragment
        offsets.extend_from_slice(&0u64.to_le_bytes()); // first fragment
        let mut lengths = Vec::new();
        lengths.extend_from_slice(&4u64.to_le_bytes());
        lengths.extend_from_slice(&4u64.to_le_bytes());

        let mut ds = Builder::default();
        ds.element(tags::EXTENDED_OFFSET_TABLE, b"OV", &offsets)
            .element(tags::EXTENDED_OFFSET_TABLE_LENGTHS, b"OV", &lengths)
            .undefined(tags::PIXEL_DATA, b"OB", &payload);
        let reader = file("1.2.840.10008.1.2.4.50", &ds.take());

        let obj = parse(&reader).expect("parse");
        let frames = obj.frame_locations(2, 0).expect("frame locations");
        let bytes = &reader.0;
        assert_eq!(
            &bytes[frames[0].offset as usize..frames[0].offset as usize + 4],
            &[5, 6, 7, 8],
            "frame 0 should come from the extended offset table"
        );
        assert_eq!(
            &bytes[frames[1].offset as usize..frames[1].offset as usize + 4],
            &[1, 2, 3, 4]
        );
    }
}
