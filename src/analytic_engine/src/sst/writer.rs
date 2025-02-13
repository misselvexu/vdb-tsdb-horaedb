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

//! Sst writer trait definition

use std::cmp;

use async_trait::async_trait;
use bytes_ext::Bytes;
use common_types::{
    record_batch::FetchedRecordBatch, request_id::RequestId, schema::Schema, time::TimeRange,
    SequenceNumber,
};
use futures::Stream;
use generic_error::{BoxError, GenericError};
use snafu::{OptionExt, ResultExt};

use crate::table_options::StorageFormat;

pub mod error {
    use common_types::datum::DatumKind;
    use generic_error::GenericError;
    use macros::define_result;
    use snafu::{Backtrace, Snafu};

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display(
            "Failed to perform storage operation, err:{}.\nBacktrace:\n{}",
            source,
            backtrace
        ))]
        Storage {
            source: object_store::ObjectStoreError,
            backtrace: Backtrace,
        },

        #[snafu(display("Failed to encode meta data, err:{}", source))]
        EncodeMetaData { source: GenericError },

        #[snafu(display("Failed to encode pb data, err:{}", source))]
        EncodePbData {
            source: crate::sst::parquet::encoding::Error,
        },

        #[snafu(display("IO failed, file:{file}, source:{source}.\nbacktrace:\n{backtrace}",))]
        Io {
            file: String,
            source: std::io::Error,
            backtrace: Backtrace,
        },

        #[snafu(display(
            "Failed to encode record batch into sst, err:{}.\nBacktrace:\n{}",
            source,
            backtrace
        ))]
        EncodeRecordBatch {
            source: GenericError,
            backtrace: Backtrace,
        },

        #[snafu(display(
            "Expect column to be timestamp, actual:{datum_kind}.\nBacktrace:\n{backtrace}"
        ))]
        ExpectTimestampColumn {
            datum_kind: DatumKind,
            backtrace: Backtrace,
        },

        #[snafu(display("Failed to build parquet filter, err:{}", source))]
        BuildParquetFilter { source: GenericError },

        #[snafu(display("Failed to build parquet filter msg:{msg}.\nBacktrace:\n{backtrace}"))]
        BuildParquetFilterNoCause { msg: String, backtrace: Backtrace },

        #[snafu(display("Failed to poll record batch, err:{}", source))]
        PollRecordBatch { source: GenericError },

        #[snafu(display("Failed to read data, err:{}", source))]
        ReadData { source: GenericError },

        #[snafu(display("Other kind of error, msg:{}.\nBacktrace:\n{}", msg, backtrace))]
        OtherNoCause { msg: String, backtrace: Backtrace },

        #[snafu(display("Empty time range.\nBacktrace:\n{}", backtrace))]
        EmptyTimeRange { backtrace: Backtrace },

        #[snafu(display("Empty schema.\nBacktrace:\n{}", backtrace))]
        EmptySchema { backtrace: Backtrace },

        #[snafu(display("Failed to convert time range, err:{}", source))]
        ConvertTimeRange { source: GenericError },

        #[snafu(display("Failed to convert sst info, err:{}", source))]
        ConvertSstInfo { source: GenericError },

        #[snafu(display("Failed to convert schema, err:{}", source))]
        ConvertSchema { source: GenericError },
    }

    define_result!(Error);
}

pub use error::*;

pub type RecordBatchStreamItem = std::result::Result<FetchedRecordBatch, GenericError>;
// TODO(yingwen): SstReader also has a RecordBatchStream, can we use same type?
pub type RecordBatchStream = Box<dyn Stream<Item = RecordBatchStreamItem> + Send + Unpin>;

#[derive(Debug, Clone)]
pub struct SstInfo {
    pub file_size: usize,
    pub row_num: usize,
    pub storage_format: StorageFormat,
    pub meta_path: String,
    /// Real time range, not aligned to segment.
    pub time_range: TimeRange,
}

impl TryFrom<horaedbproto::compaction_service::SstInfo> for SstInfo {
    type Error = Error;

    fn try_from(value: horaedbproto::compaction_service::SstInfo) -> Result<Self> {
        let storage_format = value
            .storage_format
            .try_into()
            .box_err()
            .context(ConvertSstInfo)?;
        let time_range = value
            .time_range
            .context(EmptyTimeRange)?
            .try_into()
            .box_err()
            .context(ConvertTimeRange)?;

        Ok(Self {
            file_size: value.file_size as usize,
            row_num: value.row_num as usize,
            storage_format,
            meta_path: value.meta_path,
            time_range,
        })
    }
}

impl From<SstInfo> for horaedbproto::compaction_service::SstInfo {
    fn from(value: SstInfo) -> Self {
        Self {
            file_size: value.file_size as u64,
            row_num: value.row_num as u64,
            storage_format: value.storage_format.into(),
            meta_path: value.meta_path,
            time_range: Some(value.time_range.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetaData {
    /// Min key of the sst.
    pub min_key: Bytes,
    /// Max key of the sst.
    pub max_key: Bytes,
    /// Time Range of the sst.
    pub time_range: TimeRange,
    /// Max sequence number in the sst.
    pub max_sequence: SequenceNumber,
    /// The schema of the sst.
    pub schema: Schema,
}

impl TryFrom<horaedbproto::compaction_service::MetaData> for MetaData {
    type Error = Error;

    fn try_from(meta: horaedbproto::compaction_service::MetaData) -> Result<Self> {
        let time_range = meta
            .time_range
            .context(EmptyTimeRange)?
            .try_into()
            .box_err()
            .context(ConvertTimeRange)?;
        let schema = meta
            .schema
            .context(EmptySchema)?
            .try_into()
            .box_err()
            .context(ConvertSchema)?;

        Ok(Self {
            min_key: Bytes::from(meta.min_key),
            max_key: Bytes::from(meta.max_key),
            time_range,
            max_sequence: meta.max_sequence,
            schema,
        })
    }
}

impl From<MetaData> for horaedbproto::compaction_service::MetaData {
    fn from(meta: MetaData) -> Self {
        Self {
            min_key: meta.min_key.to_vec(),
            max_key: meta.max_key.to_vec(),
            max_sequence: meta.max_sequence,
            time_range: Some(meta.time_range.into()),
            schema: Some((&meta.schema).into()),
        }
    }
}

/// The writer for sst.
///
/// The caller provides a stream of [RecordBatch] and the writer takes
/// responsibilities for persisting the records.
#[async_trait]
pub trait SstWriter {
    async fn write(
        &mut self,
        request_id: RequestId,
        meta: &MetaData,
        record_stream: RecordBatchStream,
    ) -> Result<SstInfo>;
}

impl MetaData {
    /// Merge multiple meta datas into the one.
    ///
    /// Panic if the metas is empty.
    pub fn merge<I>(mut metas: I, schema: Schema) -> Self
    where
        I: Iterator<Item = MetaData>,
    {
        let first_meta = metas.next().unwrap();
        let mut min_key = first_meta.min_key;
        let mut max_key = first_meta.max_key;
        let mut time_range_start = first_meta.time_range.inclusive_start();
        let mut time_range_end = first_meta.time_range.exclusive_end();
        let mut max_sequence = first_meta.max_sequence;

        for file in metas {
            min_key = cmp::min(file.min_key, min_key);
            max_key = cmp::max(file.max_key, max_key);
            time_range_start = cmp::min(file.time_range.inclusive_start(), time_range_start);
            time_range_end = cmp::max(file.time_range.exclusive_end(), time_range_end);
            max_sequence = cmp::max(file.max_sequence, max_sequence);
        }

        MetaData {
            min_key,
            max_key,
            time_range: TimeRange::new(time_range_start, time_range_end).unwrap(),
            max_sequence,
            schema,
        }
    }
}
