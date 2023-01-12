// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Lance Data File Reader

// Standard
use std::ops::Range;
use std::sync::Arc;

use arrow_arith::arithmetic::subtract_scalar;
use arrow_array::cast::as_primitive_array;
use arrow_array::{
    ArrayRef, Int64Array, LargeListArray, ListArray, RecordBatch, StructArray, UInt64Array,
};
use arrow_schema::{DataType, Field as ArrowField};
use async_recursion::async_recursion;
use byteorder::{ByteOrder, LittleEndian};
use futures::stream::{self, Stream};
use object_store::path::Path;
use prost::Message;

use crate::arrow::*;
use crate::encodings::{dictionary::DictionaryDecoder, Decoder};
use crate::error::{Error, Result};
use crate::format::Manifest;
use crate::format::{pb, Metadata, PageTable};
use crate::io::object_reader::ObjectReader;
use crate::io::{read_metadata_offset, read_struct};
use crate::{
    datatypes::{Field, Schema},
    format::PageInfo,
};

use super::object_store::ObjectStore;

/// Read Manifest on URI.
pub async fn read_manifest(object_store: &ObjectStore, path: &Path) -> Result<Manifest> {
    let file_size = object_store.inner.head(path).await?.size;
    const PREFETCH_SIZE: usize = 64 * 1024;
    let range = Range {
        start: std::cmp::max(file_size as i64 - PREFETCH_SIZE as i64, 0) as usize,
        end: file_size,
    };
    let buf = object_store.inner.get_range(path, range).await?;
    if buf.len() < 16 {
        return Err(Error::IO(
            "Invalid format: file size is smaller than 16 bytes".to_string(),
        ));
    }
    if !buf.ends_with(super::MAGIC) {
        return Err(Error::IO(
            "Invalid format: magic number does not match".to_string(),
        ));
    }
    let manifest_pos = LittleEndian::read_i64(&buf[buf.len() - 16..buf.len() - 8]) as usize;
    assert!(file_size - manifest_pos < buf.len());
    let proto =
        pb::Manifest::decode(&buf[buf.len() - (file_size - manifest_pos) + 4..buf.len() - 16])?;
    Ok(Manifest::from(proto))
}

/// Compute row id from `fragment_id` and the `offset` of the row in the fragment.
fn compute_row_id(fragment_id: u64, offset: i32) -> u64 {
    (fragment_id << 32) + offset as u64
}

/// Lance File Reader.
///
/// It reads arrow data from one data file.
pub struct FileReader<'a> {
    object_reader: ObjectReader<'a>,
    metadata: Metadata,
    page_table: PageTable,
    projection: Option<Schema>,

    /// The id of the fragment which this file belong to.
    /// For simple file access, this can just be zero.
    fragment_id: u64,

    /// If set true, returns the row ID from the dataset alongside with the
    /// actual data.
    with_row_id: bool,
}

impl<'a> FileReader<'a> {
    /// Open file reader
    pub(crate) async fn try_new_with_fragment(
        object_store: &'a ObjectStore,
        path: &Path,
        fragment_id: u64,
        manifest: Option<&Manifest>,
    ) -> Result<FileReader<'a>> {
        let mut object_reader =
            ObjectReader::new(object_store, path.clone(), object_store.prefetch_size())?;

        let file_size = object_reader.size().await?;
        let begin = if file_size < object_store.prefetch_size() {
            0
        } else {
            file_size - object_store.prefetch_size()
        };
        let tail_bytes = object_reader
            .object_store
            .inner
            .get_range(path, begin..file_size)
            .await?;
        let metadata_pos = read_metadata_offset(&tail_bytes)?;

        let metadata: Metadata = if metadata_pos < file_size - tail_bytes.len() {
            // We have not read the metadata bytes yet.
            object_reader.read_struct(metadata_pos).await?
        } else {
            let offset = tail_bytes.len() - (file_size - metadata_pos);
            read_struct(&tail_bytes.slice(offset..))?
        };

        let (projection, num_columns) = if let Some(m) = manifest {
            (m.schema.clone(), m.schema.max_field_id().unwrap() + 1)
        } else {
            let mut m: Manifest = object_reader
                .read_struct(metadata.manifest_position.unwrap())
                .await?;
            m.schema.load_dictionary(&object_reader).await?;
            (m.schema.clone(), m.schema.max_field_id().unwrap() + 1)
        };
        let page_table = PageTable::load(
            &object_reader,
            metadata.page_table_position,
            num_columns,
            metadata.num_batches() as i32,
        )
        .await?;

        Ok(Self {
            object_reader,
            metadata,
            projection: Some(projection),
            page_table,
            fragment_id,
            with_row_id: false,
        })
    }

    /// Open one Lance data file for read.
    pub async fn try_new(object_store: &'a ObjectStore, path: &Path) -> Result<FileReader<'a>> {
        Self::try_new_with_fragment(object_store, path, 0, None).await
    }

    /// Set the projection [Schema].
    pub fn set_projection(&mut self, schema: Schema) {
        self.projection = Some(schema)
    }

    /// Instruct the FileReader to return meta row id column.
    pub(crate) fn with_row_id(&mut self, v: bool) -> &mut Self {
        self.with_row_id = v;
        self
    }

    /// Schema of the returning RecordBatch.
    pub fn schema(&self) -> &Schema {
        self.projection.as_ref().unwrap()
    }

    pub fn num_batches(&self) -> usize {
        self.metadata.num_batches()
    }

    /// Read a batch of data from the file.
    ///
    /// The schema of the returned [RecordBatch] is set by [`FileReader::schema()`].
    pub async fn read_batch(&self, batch_id: i32) -> Result<RecordBatch> {
        let schema = self.projection.as_ref().unwrap();
        let batch_offset = self
            .metadata
            .get_offset(batch_id)
            .ok_or_else(|| Error::IO(format!("batch {} does not exist", batch_id)))?;
        read_batch(
            &self.object_reader,
            schema,
            batch_id,
            &self.page_table,
            self.fragment_id,
            batch_offset,
            self.with_row_id,
        )
        .await
    }

    /// Convert this [`FileReader`] into a [Stream] / [AsyncIterator](std::async_iter::AsyncIterator).
    ///
    /// Currently, it only does batch based scan.
    /// Will add support for scanning with batch size later.
    ///
    // TODO: use IntoStream trait?
    pub fn into_stream(&self) -> impl Stream<Item = Result<RecordBatch>> + '_ {
        let num_batches = self.num_batches() as i32;

        // Deref a bunch.
        let object_reader = &self.object_reader;
        let schema = self.schema();
        let page_table = &self.page_table;
        let fragment_id = self.fragment_id;
        let with_row_id = self.with_row_id;

        stream::unfold(0_i32, move |batch_id| async move {
            let num_batches = num_batches;
            let batch_offset = match self
                .metadata
                .get_offset(batch_id)
                .ok_or_else(|| Error::IO(format!("batch {} does not exist", batch_id)))
            {
                Ok(o) => o,
                Err(e) => return Some((Err(e), batch_id + 1)),
            };
            if batch_id < num_batches {
                let batch = read_batch(
                    object_reader,
                    schema,
                    batch_id,
                    page_table,
                    fragment_id,
                    batch_offset,
                    with_row_id,
                )
                .await;
                Some((batch, batch_id + 1))
            } else {
                None
            }
        })
    }
}

async fn read_batch(
    reader: &ObjectReader<'_>,
    schema: &Schema,
    batch_id: i32,
    page_table: &PageTable,
    fragment_id: u64,
    batch_offset: i32,
    with_row_id: bool,
) -> Result<RecordBatch> {
    let mut arrs = vec![];
    for field in schema.fields.iter() {
        let arr = read_array(reader, field, batch_id, page_table).await?;
        arrs.push(arr);
    }
    let mut batch = RecordBatch::try_new(Arc::new(schema.into()), arrs)?;
    if with_row_id {
        let row_id_arr = Arc::new(UInt64Array::from_iter_values(
            (batch_offset..(batch_offset + batch.num_rows() as i32))
                .map(|o| compute_row_id(fragment_id, o))
                .collect::<Vec<_>>(),
        ));
        batch = batch.try_with_column(
            ArrowField::new("_rowid", DataType::UInt64, false),
            row_id_arr,
        )?;
    }
    Ok(batch)
}

#[async_recursion]
async fn read_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let data_type = field.data_type();

    use DataType::*;

    if data_type.is_fixed_stride() {
        read_fixed_stride_array(reader, field, batch_id, page_table).await
    } else {
        match data_type {
            Utf8 | LargeUtf8 | Binary | LargeBinary => {
                read_binary_array(reader, field, batch_id, page_table).await
            }
            Struct(_) => read_struct_array(reader, field, batch_id, page_table).await,
            Dictionary(_, _) => read_dictionary_array(reader, field, batch_id, page_table).await,
            List(_) => read_list_array(reader, field, batch_id, page_table).await,
            LargeList(_) => read_large_list_array(reader, field, batch_id, page_table).await,
            _ => {
                unimplemented!("{}", format!("No support for {data_type} yet"));
            }
        }
    }
}

fn get_page_info<'a>(
    page_table: &'a PageTable,
    field: &'a Field,
    batch_id: i32,
) -> Result<&'a PageInfo> {
    page_table.get(field.id, batch_id).ok_or_else(|| {
        Error::IO(format!(
            "No page info found for field: {}, field_id={} batch={}",
            field.name, field.id, batch_id
        ))
    })
}

/// Read primitive array for batch `batch_idx`.
async fn read_fixed_stride_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let page_info = get_page_info(page_table, field, batch_id)?;

    reader
        .read_fixed_stride_array(&field.data_type(), page_info.position, page_info.length)
        .await
}

async fn read_binary_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let page_info = get_page_info(page_table, field, batch_id)?;

    reader
        .read_binary_array(&field.data_type(), page_info.position, page_info.length)
        .await
}

async fn read_dictionary_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let page_info = get_page_info(page_table, field, batch_id)?;
    let data_type = field.data_type();
    let decoder = DictionaryDecoder::new(
        reader,
        page_info.position,
        page_info.length,
        &data_type,
        field
            .dictionary
            .as_ref()
            .unwrap()
            .values
            .as_ref()
            .unwrap()
            .clone(),
    );
    decoder.decode().await
}

async fn read_struct_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    // TODO: use tokio to make the reads in parallel.
    let mut sub_arrays = vec![];
    for child in field.children.as_slice() {
        let arr = read_array(reader, child, batch_id, page_table).await?;
        sub_arrays.push((child.into(), arr));
    }

    Ok(Arc::new(StructArray::from(sub_arrays)))
}

async fn read_list_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let page_info = get_page_info(page_table, field, batch_id)?;

    let position_arr = reader
        .read_fixed_stride_array(&DataType::Int32, page_info.position, page_info.length)
        .await?;
    let positions = as_primitive_array(position_arr.as_ref());
    let start_position = positions.value(0);
    // Compute offsets
    let offset_arr = subtract_scalar(positions, start_position)?;
    let value_arrs = read_array(reader, &field.children[0], batch_id, page_table).await?;

    Ok(Arc::new(ListArray::try_new(value_arrs, &offset_arr)?))
}

// TODO: merge with [read_list_array]?
async fn read_large_list_array(
    reader: &ObjectReader<'_>,
    field: &Field,
    batch_id: i32,
    page_table: &PageTable,
) -> Result<ArrayRef> {
    let page_info = get_page_info(page_table, field, batch_id)?;
    let position_arr = reader
        .read_fixed_stride_array(&DataType::Int64, page_info.position, page_info.length)
        .await?;
    let positions: &Int64Array = as_primitive_array(position_arr.as_ref());
    let start_position = positions.value(0);
    // Compute offsets
    let offset_arr = subtract_scalar(positions, start_position)?;
    let value_arrs = read_array(reader, &field.children[0], batch_id, page_table).await?;

    Ok(Arc::new(LargeListArray::try_new(value_arrs, &offset_arr)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_array::{cast::as_primitive_array, Float32Array, Int64Array};
    use arrow_schema::{Field as ArrowField, Schema as ArrowSchema};
    use futures::StreamExt;

    use crate::io::FileWriter;

    #[tokio::test]
    async fn file_reader_into_stream() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new("i", DataType::Int64, true),
            ArrowField::new("f", DataType::Float32, false),
        ]);
        let schema = Schema::try_from(&arrow_schema).unwrap();

        let store = ObjectStore::memory();
        let path = Path::from("/foo");
        let writer = store.create(&path).await.unwrap();

        // Write 5 batches.
        let mut file_writer = FileWriter::new(writer, &schema);
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from_iter((0..100).collect::<Vec<_>>())),
            Arc::new(Float32Array::from_iter(
                (0..100).map(|n| n as f32).collect::<Vec<_>>(),
            )),
        ];
        let batch = RecordBatch::try_new(Arc::new(arrow_schema.clone()), columns).unwrap();
        for _ in 0..5 {
            file_writer.write(&batch).await.unwrap();
        }
        file_writer.finish().await.unwrap();

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let stream = reader.into_stream();

        assert_eq!(stream.count().await, 5);

        let stream = reader.into_stream();
        assert!(
            stream
                .map(|f| f.unwrap() == batch)
                .all(|f| async move { f })
                .await
        );
    }

    #[tokio::test]
    async fn read_with_row_id() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new("i", DataType::Int64, true),
            ArrowField::new("f", DataType::Float32, false),
        ]);
        let schema = Schema::try_from(&arrow_schema).unwrap();

        let store = ObjectStore::memory();
        let path = Path::from("/foo");
        let writer = store.create(&path).await.unwrap();

        // Write 10 batches.
        let mut file_writer = FileWriter::new(writer, &schema);
        for batch_id in 0..10 {
            let value_range = batch_id * 10..batch_id * 10 + 10;
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from_iter(
                    value_range.clone().collect::<Vec<_>>(),
                )),
                Arc::new(Float32Array::from_iter(
                    value_range.map(|n| n as f32).collect::<Vec<_>>(),
                )),
            ];
            let batch = RecordBatch::try_new(Arc::new(arrow_schema.clone()), columns).unwrap();
            file_writer.write(&batch).await.unwrap();
        }
        file_writer.finish().await.unwrap();

        let fragment = 123;
        let mut reader = FileReader::try_new_with_fragment(&store, &path, fragment, None)
            .await
            .unwrap();
        reader.with_row_id(true);

        for b in 0..10 {
            let batch = reader.read_batch(b).await.unwrap();
            assert!(batch.column_with_name("_rowid").is_some());
            let row_ids_col = batch.column_with_name("_rowid").unwrap();
            // Do the same computation as `compute_row_id`.
            let start_pos = (fragment << 32) as u64 + 10 * b as u64;

            assert_eq!(
                &UInt64Array::from_iter_values(start_pos..start_pos + 10),
                as_primitive_array(row_ids_col)
            );
        }
    }
}
