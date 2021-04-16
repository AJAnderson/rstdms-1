use crate::error::{Result, TdmsReadError};
use crate::object_path::{ObjectPathCache, ObjectPathId};
use crate::properties::TdmsProperty;
use crate::toc::{TocFlag, TocMask};
use crate::types::{TdsType, LittleEndianReader, TypeReader};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

const RAW_DATA_INDEX_NO_DATA: u32 = 0xFFFFFFFF;
const RAW_DATA_INDEX_MATCHES_PREVIOUS: u32 = 0x00000000;
const FORMAT_CHANGING_SCALER: u32 = 0x00001269;
const DIGITAL_LINE_SCALER: u32 = 0x0000126A;


#[derive(Debug)]
pub struct TdmsMetadata {
    pub properties: HashMap<ObjectPathId, Vec<TdmsProperty>>,
    pub object_paths: ObjectPathCache,
}

#[derive(Debug)]
pub struct TdmsSegment {
    data_position: u64,
    next_segment_position: u64,
    objects: Vec<SegmentObject>,
}

impl TdmsSegment {
    fn new(data_position: u64, next_segment_position: u64, objects: Vec<SegmentObject>) -> TdmsSegment {
        TdmsSegment { data_position, next_segment_position, objects }
    }
}

#[derive(Debug)]
pub struct SegmentObject {
    pub object_id: ObjectPathId,
    pub raw_data_index: Option<RawDataIndex>,
}

impl SegmentObject {
    pub fn no_data(object_id: ObjectPathId) -> SegmentObject {
        SegmentObject { object_id, raw_data_index: None }
    }

    pub fn with_data(object_id: ObjectPathId, raw_data_index: RawDataIndex) -> SegmentObject {
        SegmentObject { object_id, raw_data_index: Some(raw_data_index) }
    }
}

#[derive(Debug)]
pub struct RawDataIndex {
    pub number_of_values: u64,
    pub data_type: TdsType,
    pub data_size: u64,
}

pub fn read_metadata<T: Read + Seek>(reader: &mut T) -> Result<TdmsMetadata> {
    let mut properties = HashMap::new();
    let mut object_paths = ObjectPathCache::new();
    loop {
        let position = reader.seek(SeekFrom::Current(0))?;
        match read_segment(reader, position, &mut object_paths, &mut properties) {
            Err(e) => return Err(e),
            Ok(None) => {
                // Reached end of file
                break;
            }
            Ok(Some(segment)) => {
                // Seek to the start of the next segment
                reader.seek(SeekFrom::Start(segment.next_segment_position))?;
            }
        }
    }
    Ok(TdmsMetadata { properties, object_paths })
}

fn read_segment<T: Read + Seek>(
    reader: &mut T,
    position: u64,
    object_paths: &mut ObjectPathCache,
    properties: &mut HashMap<ObjectPathId, Vec<TdmsProperty>>,
) -> Result<Option<TdmsSegment>> {
    let mut header_bytes = [0u8; 4];
    let mut bytes_read = 0;
    while bytes_read < 4 {
        match reader.read(&mut header_bytes[bytes_read..])? {
            0 => return Ok(None),
            n => bytes_read += n,
        }
    }

    // Check segment header
    let expected_header = [0x54, 0x44, 0x53, 0x6d];
    if header_bytes != expected_header {
        return Err(TdmsReadError::TdmsError(format!(
            "Invalid segment header at position {}: {:?}", position, header_bytes,
        )));
    }

    let mut type_reader = LittleEndianReader::new(reader);
    let toc_mask = TocMask::from_flags(type_reader.read_uint32()?);

    // TODO: Check endianness from ToC mask
    let mut type_reader = LittleEndianReader::new(reader);

    let version = type_reader.read_int32()?;
    let next_segment_offset = type_reader.read_uint64()?;
    let raw_data_offset = type_reader.read_uint64()?;

    let lead_in_length = 28;
    let next_segment_position = position + lead_in_length + next_segment_offset;
    let raw_data_position = position + lead_in_length + raw_data_offset;

    println!("Read segment with toc_mask = {}, version = {}, next_segment_offset = {}, raw_data_offset = {}",
            toc_mask, version, next_segment_offset, raw_data_offset);

    let segment_objects = if toc_mask.has_flag(TocFlag::MetaData) {
        read_object_metadata(&mut type_reader, &toc_mask, object_paths, properties)?
    } else {
        unimplemented!();
    };

    Ok(Some(TdmsSegment::new(raw_data_position, next_segment_position, segment_objects)))
}

fn read_object_metadata<T: TypeReader>(
    reader: &mut T,
    toc_mask: &TocMask,
    object_paths: &mut ObjectPathCache,
    properties: &mut HashMap<ObjectPathId, Vec<TdmsProperty>>,
) -> Result<Vec<SegmentObject>> {
    if !toc_mask.has_flag(TocFlag::NewObjList) {
        unimplemented!();
    }

    let num_objects = reader.read_uint32()?;
    let mut segment_objects = Vec::with_capacity(num_objects as usize);
    for _ in 0..num_objects {
        let object_path = reader.read_string()?;
        let object_id = object_paths.get_or_create_id(object_path);
        let raw_data_index_header = reader.read_uint32()?;
        let segment_object = match raw_data_index_header {
            RAW_DATA_INDEX_NO_DATA => SegmentObject::no_data(object_id),
            RAW_DATA_INDEX_MATCHES_PREVIOUS => unimplemented!(),
            FORMAT_CHANGING_SCALER => unimplemented!(),
            DIGITAL_LINE_SCALER => unimplemented!(),
            _ => {
                // Raw data index header gives length of index information
                let raw_data_index = read_raw_data_index(reader)?;
                SegmentObject::with_data(object_id, raw_data_index)
            }
        };
        segment_objects.push(segment_object);
        let num_properties = reader.read_uint32()?;
        for _ in 0..num_properties {
            let property = TdmsProperty::read(reader)?;
            properties.entry(object_id).or_insert_with(|| Vec::new()).push(property);
        }
    }

    Ok(segment_objects)
}

fn read_raw_data_index<T: TypeReader>(
    reader: &mut T
) -> Result<RawDataIndex> {
    let data_type = reader.read_uint32()?;
    let data_type = TdsType::from_u32(data_type)?;
    let dimension = reader.read_uint32()?;
    let number_of_values = reader.read_uint64()?;

    if dimension != 1 {
        return Err(TdmsReadError::TdmsError(format!("Dimension must be 1, got {}", dimension)));
    }

    let data_size = match data_type.size() {
        Some(type_size) => (type_size as u64) * number_of_values,
        None => {
            if data_type == TdsType::String {
                reader.read_uint64()?
            } else {
                return Err(TdmsReadError::TdmsError(format!("Unsupported data type: {:?}", data_type)));
            }
        }
    };
    Ok(RawDataIndex{number_of_values, data_type, data_size})
}
