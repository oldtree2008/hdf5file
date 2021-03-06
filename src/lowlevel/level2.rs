use crate::io::{ReadExt as _, SeekExt as _};
use crate::{Error, ErrorKind, Result};
use ndarray;
use ndarray::ArrayD;
use std;
use std::convert::TryFrom;
use std::io::{Read, Seek};

/// Data object.
#[derive(Debug)]
pub enum DataObject {
    /// Floating-point numbers.
    Float(ArrayD<f64>),
}

// TODO: move level2a
/// https://support.hdfgroup.org/HDF5/doc/H5.format.html#ObjectHeader
#[derive(Debug, Clone)]
pub struct ObjectHeader {
    prefix: ObjectHeaderPrefix,
}
impl ObjectHeader {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let prefix = track!(ObjectHeaderPrefix::from_reader(&mut reader))?;
        Ok(Self { prefix })
    }

    pub fn get_data_object<R: Read + Seek>(&self, mut reader: R) -> Result<DataObject> {
        let bytes = track!(self.get_data_bytes(&mut reader))?;
        let dimensions = track!(self.dimensions())?
            .iter()
            .map(|&d| d as usize)
            .collect::<Vec<_>>();
        let datatype = track!(self.datatype())?;

        let count = dimensions.iter().cloned().product::<usize>();
        let mut reader = &bytes[..];
        match datatype {
            DatatypeMessage::FloatingPoint(t) => {
                let items = (0..count)
                    .map(|i| track!(t.decode(&mut reader); i))
                    .collect::<Result<Vec<_>>>()?;
                track_assert_eq!(reader, b"", ErrorKind::InvalidFile);

                let items = track!(ndarray::aview1(&items)
                    .into_shape(dimensions)
                    .map_err(Error::from))?;
                Ok(DataObject::Float(items.to_owned()))
            }
            _ => track_panic!(ErrorKind::Unsupported),
        }
    }

    fn dimensions(&self) -> Result<&[u64]> {
        for m in &self.prefix.messages {
            if let Message::Dataspace(m) = &m.message {
                return Ok(&m.dimension_sizes);
            }
        }
        track_panic!(ErrorKind::Other);
    }

    fn datatype(&self) -> Result<DatatypeMessage> {
        for m in &self.prefix.messages {
            if let Message::Datatype(m) = &m.message {
                return Ok(m.clone());
            }
        }
        track_panic!(ErrorKind::Other);
    }

    pub fn get_data_bytes<R: Read + Seek>(&self, mut reader: R) -> Result<Vec<u8>> {
        for m in &self.prefix.messages {
            if let Message::DataLayout(m) = &m.message {
                let Layout::Contiguous { address, size } = m.layout;
                track!(reader.seek_to(address))?;
                return track!(reader.read_vec(size as usize));
            }
        }
        track_panic!(ErrorKind::Other, "Not a data object");
    }
}

#[derive(Debug, Clone)]
pub struct ObjectHeaderPrefix {
    messages: Vec<HeaderMessage>,
    object_reference_count: u32,
    object_header_size: u32,
}
impl ObjectHeaderPrefix {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let version = track!(reader.read_u8())?;
        track_assert_eq!(version, 1, ErrorKind::InvalidFile);

        let _reserved = track!(reader.read_u8())?;
        track_assert_eq!(_reserved, 0, ErrorKind::InvalidFile);

        let header_message_count = track!(reader.read_u16())?;
        let object_reference_count = track!(reader.read_u32())?;
        let object_header_size = track!(reader.read_u32())?;

        // Header messages are aligned on 8-byte boundaries for version 1 object headers.
        track!(reader.skip(4))?;

        let mut reader = reader.take(u64::from(object_header_size));
        let messages = (0..header_message_count)
            .map(|_| track!(HeaderMessage::from_reader(&mut reader)))
            .collect::<Result<_>>()?;
        track_assert_eq!(reader.limit(), 0, ErrorKind::Other; object_header_size, messages);

        Ok(Self {
            messages,
            object_reference_count,
            object_header_size,
        })
    }
}

bitflags! {
    struct HeaderMessageFlags: u8 {
        const CONSTANT = 0b0000_0001;
        const SHARED = 0b0000_0010;
        const UNSHARABLE = 0b0000_0100;
        const CANNOT_WRITE_IF_UNKNOWN = 0b0000_1000;
        const SET_5_BIT_IF_UNKNOWN = 0b0001_0000;
        const UNKNOWN_BUT_MODIFIED = 0b0010_0000;
        const SHARABLE = 0b0100_0000;
        const FAIL_IF_UNKNOWN = 0b0100_0000;
    }
}

#[derive(Debug, Clone)]
pub struct HeaderMessage {
    flags: HeaderMessageFlags,
    message: Message,
}
impl HeaderMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let kind = track!(reader.read_u16())?;
        let data_len = track!(reader.read_u16())?;
        let flags = HeaderMessageFlags::from_bits_truncate(track!(reader.read_u8())?);
        track!(reader.skip(3))?;
        let mut reader = reader.take(u64::from(data_len));
        let message = match kind {
            0x00 => track!(NilMessage::from_reader(&mut reader)).map(Message::Nil)?,
            0x01 => track!(DataspaceMessage::from_reader(&mut reader)).map(Message::Dataspace)?,
            0x03 => track!(DatatypeMessage::from_reader(&mut reader)).map(Message::Datatype)?,
            0x05 => track!(FillValueMessage::from_reader(&mut reader)).map(Message::FillValue)?,
            0x08 => track!(DataLayoutMessage::from_reader(&mut reader)).map(Message::DataLayout)?,
            0x11 => {
                track!(SymbolTableMessage::from_reader(&mut reader)).map(Message::SymbolTable)?
            }
            0x12 => track!(ObjectModificationTimeMessage::from_reader(&mut reader))
                .map(Message::ObjectModificationTime)?,
            _ => track_panic!(ErrorKind::Unsupported, "Message type: {}", kind),
        };
        track_assert_eq!(reader.limit(), 0, ErrorKind::Other);

        Ok(Self { flags, message })
    }
}

/// type=0x00
#[derive(Debug, Clone)]
pub struct NilMessage {}
impl NilMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let _ = track!(reader.read_all())?;
        Ok(Self {})
    }
}

/// type=0x01
#[derive(Debug, Clone)]
pub struct DataspaceMessage {
    dimension_sizes: Vec<u64>,
    dimension_max_sizes: Option<Vec<u64>>,
}
impl DataspaceMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let version = track!(reader.read_u8())?;
        track_assert_eq!(version, 1, ErrorKind::Unsupported);

        let dimensionality = track!(reader.read_u8())?;
        let flags = track!(reader.read_u8())?; // TODO: consider flags
        track!(reader.skip(5))?;

        let dimension_sizes = (0..dimensionality)
            .map(|_| track!(reader.read_u64()))
            .collect::<Result<Vec<_>>>()?;

        let dimension_max_sizes = if (flags & 0b0000_0001) != 0 {
            Some(
                (0..dimensionality)
                    .map(|_| track!(reader.read_u64()))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            None
        };

        if (flags & 0b0000_0010) != 0 {
            track_panic!(ErrorKind::Unsupported);
        }

        Ok(Self {
            dimension_sizes,
            dimension_max_sizes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DatatypeClass {
    FixedPoint,
    FloatingPoint,
    Time,
    String,
    BitField,
    Opaque,
    Compound,
    Reference,
    Enumerated,
    VariableLength,
    Array,
}
impl TryFrom<u8> for DatatypeClass {
    type Error = Error;

    fn try_from(f: u8) -> Result<Self> {
        Ok(match f {
            0 => DatatypeClass::FixedPoint,
            1 => DatatypeClass::FloatingPoint,
            2 => DatatypeClass::Time,
            3 => DatatypeClass::String,
            4 => DatatypeClass::BitField,
            5 => DatatypeClass::Opaque,
            6 => DatatypeClass::Compound,
            7 => DatatypeClass::Reference,
            8 => DatatypeClass::Enumerated,
            9 => DatatypeClass::VariableLength,
            10 => DatatypeClass::Array,
            _ => track_panic!(ErrorKind::InvalidFile, "Unknown datatype class: {}", f),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MantissaNorm {
    None,
    AlwaysSet,
    ImpliedToBeSet,
}
impl TryFrom<u8> for MantissaNorm {
    type Error = Error;

    fn try_from(f: u8) -> Result<Self> {
        match f {
            0 => Ok(MantissaNorm::None),
            1 => Ok(MantissaNorm::AlwaysSet),
            2 => Ok(MantissaNorm::ImpliedToBeSet),
            3 => track_panic!(ErrorKind::InvalidFile, "Reserved value"),
            _ => track_panic!(ErrorKind::InvalidInput),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endian {
    Little,
    Big,
    Vax,
}
impl TryFrom<u8> for Endian {
    type Error = Error;

    fn try_from(f: u8) -> Result<Self> {
        match f {
            0b0000_0000 => Ok(Endian::Little),
            0b0000_0001 => Ok(Endian::Big),
            0b0100_0000 => track_panic!(ErrorKind::InvalidFile, "Reserved endian bits"),
            0b0100_0001 => Ok(Endian::Vax),
            _ => track_panic!(ErrorKind::InvalidInput),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FloatingPointDatatype {
    size: u32,

    endian: Endian,
    low_padding_bit: u8,
    high_padding_bit: u8,
    internal_padding_bit: u8,
    mantissa_norm: MantissaNorm,
    sign_location: u8,

    bit_offset: u16,
    bit_precision: u16,
    exponent_location: u8,
    exponent_size: u8,
    mantissa_location: u8,
    mantissa_size: u8,
    exponent_bias: u32,
}
impl FloatingPointDatatype {
    pub fn decode<R: Read>(&self, mut reader: R) -> Result<f64> {
        track_assert_eq!(self.endian, Endian::Little, ErrorKind::Unsupported);
        track_assert_eq!(self.low_padding_bit, 0, ErrorKind::Unsupported);
        track_assert_eq!(self.high_padding_bit, 0, ErrorKind::Unsupported);
        track_assert_eq!(self.internal_padding_bit, 0, ErrorKind::Unsupported);
        track_assert_eq!(
            self.mantissa_norm,
            MantissaNorm::ImpliedToBeSet,
            ErrorKind::Unsupported
        );
        track_assert_eq!(self.sign_location, 31, ErrorKind::Unsupported);

        track_assert_eq!(self.bit_offset, 0, ErrorKind::Unsupported);
        track_assert_eq!(self.bit_precision, 32, ErrorKind::Unsupported);
        track_assert_eq!(self.exponent_location, 23, ErrorKind::Unsupported);
        track_assert_eq!(self.exponent_size, 8, ErrorKind::Unsupported);
        track_assert_eq!(self.mantissa_location, 0, ErrorKind::Unsupported);
        track_assert_eq!(self.mantissa_size, 23, ErrorKind::Unsupported);
        track_assert_eq!(self.exponent_bias, 127, ErrorKind::Unsupported);

        track!(reader.read_f32()).map(f64::from)
    }

    pub fn from_reader<R: Read>(bit_field: u32, size: u32, mut reader: R) -> Result<Self> {
        let bit_offset = track!(reader.read_u16())?;
        let bit_precision = track!(reader.read_u16())?;
        let exponent_location = track!(reader.read_u8())?;
        let exponent_size = track!(reader.read_u8())?;
        let mantissa_location = track!(reader.read_u8())?;
        let mantissa_size = track!(reader.read_u8())?;
        let exponent_bias = track!(reader.read_u32())?;
        track!(reader.skip(4))?;

        Ok(Self {
            size,

            endian: track!(Endian::try_from((bit_field & 0b0100_0001) as u8))?,
            low_padding_bit: ((bit_field >> 1) & 1) as u8,
            high_padding_bit: ((bit_field >> 2) & 1) as u8,
            internal_padding_bit: ((bit_field >> 3) & 1) as u8,
            mantissa_norm: track!(MantissaNorm::try_from(((bit_field >> 4) & 0b11) as u8))?,
            sign_location: (bit_field >> 8) as u8,

            bit_offset,
            bit_precision,
            exponent_location,
            exponent_size,
            mantissa_location,
            mantissa_size,
            exponent_bias,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FixedPointDatatype {
    bit_field: u32,
    size: u32,

    bit_offset: u16,
    bit_precision: u16,
}
impl FixedPointDatatype {
    pub fn from_reader<R: Read>(bit_field: u32, size: u32, mut reader: R) -> Result<Self> {
        let bit_offset = track!(reader.read_u16())?;
        let bit_precision = track!(reader.read_u16())?;
        track!(reader.skip(4))?;

        Ok(Self {
            bit_field,
            size,

            bit_offset,
            bit_precision,
        })
    }
}

/// type=0x03
#[derive(Debug, Clone)]
pub enum DatatypeMessage {
    FixedPoint(FixedPointDatatype),
    FloatingPoint(FloatingPointDatatype),
    // Time,
    // String,
    // BitField,
    // Opaque,
    // Compound,
    // Reference,
    // Enumerated,
    // VariableLength,
    // Array,
}
impl DatatypeMessage {
    // pub fn decode<R: Read>(&self, reader: R) -> Result<DataItem> {
    //     match self {
    //         DatatypeMessage::FloatingPoint(t) => track!(t.decode(reader)).map(DataItem::Float),
    //         _ => track_panic!(ErrorKind::Unsupported),
    //     }
    // }

    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let class_and_version = track!(reader.read_u8())?;
        let version = class_and_version >> 4;
        let class = track!(DatatypeClass::try_from(class_and_version & 0b0000_1111))?;
        track_assert_eq!(version, 1, ErrorKind::Unsupported);

        let bit_field = track!(reader.read_u24())?;
        let size = track!(reader.read_u32())?;

        match class {
            DatatypeClass::FixedPoint => {
                track!(FixedPointDatatype::from_reader(bit_field, size, reader))
                    .map(DatatypeMessage::FixedPoint)
            }
            DatatypeClass::FloatingPoint => {
                track!(FloatingPointDatatype::from_reader(bit_field, size, reader))
                    .map(DatatypeMessage::FloatingPoint)
            }
            _ => track_panic!(ErrorKind::Unsupported; class),
        }
    }
}

/// type=0x05
#[derive(Debug, Clone)]
pub struct FillValueMessage {
    space_allocation_time: u8,
    fill_value_write_time: u8,
    fill_value: Option<Vec<u8>>,
}
impl FillValueMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let version = track!(reader.read_u8())?;
        track_assert_eq!(version, 2, ErrorKind::Unsupported);

        let space_allocation_time = track!(reader.read_u8())?;
        let fill_value_write_time = track!(reader.read_u8())?;
        let fill_value_defined = track!(reader.read_u8())?;
        let fill_value = if fill_value_defined == 1 {
            let size = track!(reader.read_u32())?;
            let fill_value = track!(reader.read_vec(size as usize))?;
            Some(fill_value)
        } else {
            None
        };
        Ok(Self {
            space_allocation_time,
            fill_value_write_time,
            fill_value,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Layout {
    Contiguous { address: u64, size: u64 },
}
impl Layout {
    pub fn from_reader<R: Read>(class: u8, mut reader: R) -> Result<Self> {
        match class {
            0 => track_panic!(ErrorKind::Unsupported),
            1 => {
                let address = track!(reader.read_u64())?;
                let size = track!(reader.read_u64())?;
                Ok(Layout::Contiguous { address, size })
            }
            2 => track_panic!(ErrorKind::Unsupported),
            _ => track_panic!(ErrorKind::InvalidFile, "Unknown layout class: {}", class),
        }
    }
}

/// type=0x08
#[derive(Debug, Clone)]
pub struct DataLayoutMessage {
    layout: Layout,
}
impl DataLayoutMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let version = track!(reader.read_u8())?;
        track_assert_eq!(version, 3, ErrorKind::Unsupported);

        let layout_class = track!(reader.read_u8())?;
        let layout = track!(Layout::from_reader(layout_class, &mut reader))?;
        let _padding = track!(reader.read_all())?;
        Ok(Self { layout })
    }
}

/// type=0x11
#[derive(Debug, Clone)]
pub struct SymbolTableMessage {
    pub b_tree_address: u64,
    pub local_heap_address: u64,
}
impl SymbolTableMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        Ok(Self {
            b_tree_address: track!(reader.read_u64())?,
            local_heap_address: track!(reader.read_u64())?,
        })
    }
}

/// type=0x12
#[derive(Debug, Clone)]
pub struct ObjectModificationTimeMessage {
    unixtime_seconds: u32,
}
impl ObjectModificationTimeMessage {
    pub fn from_reader<R: Read>(mut reader: R) -> Result<Self> {
        let version = track!(reader.read_u8())?;
        track_assert_eq!(version, 1, ErrorKind::Unsupported);
        track!(reader.skip(3))?;

        let unixtime_seconds = track!(reader.read_u32())?;
        Ok(Self { unixtime_seconds })
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Nil(NilMessage),
    Dataspace(DataspaceMessage),
    // LinkInfo,
    Datatype(DatatypeMessage),
    // FillValueOld,
    FillValue(FillValueMessage),
    // Link,
    // ExternalDataFile,
    DataLayout(DataLayoutMessage),
    // Bogus,
    // GroupInfo,
    // FilePipeline,
    // Attribute,
    // ObjectComment,
    // ObjectModificationTimeOld,
    // SharedMessageTable,
    // ObjectHeaderContinuation,
    SymbolTable(SymbolTableMessage),
    ObjectModificationTime(ObjectModificationTimeMessage),
    // BTreeKValues,
    // DriverInfo,
    // AttributeInfo,
    // ObjectReferenceCount,
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackable::result::TopLevelResult;

    #[test]
    fn floating_point_decode_works() -> TopLevelResult {
        let datatype = FloatingPointDatatype {
            size: 4,
            endian: Endian::Little,
            low_padding_bit: 0,
            high_padding_bit: 0,
            internal_padding_bit: 0,
            mantissa_norm: MantissaNorm::ImpliedToBeSet,
            sign_location: 31,
            bit_offset: 0,
            bit_precision: 32,
            exponent_location: 23,
            exponent_size: 8,
            mantissa_location: 0,
            mantissa_size: 23,
            exponent_bias: 127,
        };
        let bytes = [166, 73, 90, 67];

        let item = track!(datatype.decode(&bytes[..]))?;
        assert_eq!(item, 218.28768920898438);
        Ok(())
    }
}
