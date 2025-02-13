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

use std::{sync::Arc, vec};

use anyhow::Context;
use arrow::{
    array::{Int64Array, RecordBatch},
    datatypes::SchemaRef,
};
use async_trait::async_trait;
use datafusion::{
    common::DFSchema,
    datasource::{
        listing::PartitionedFile,
        physical_plan::{FileScanConfig, ParquetExec},
    },
    execution::{context::ExecutionProps, object_store::ObjectStoreUrl, SendableRecordBatchStream},
    logical_expr::{utils::conjunction, Expr},
    physical_expr::{create_physical_expr, LexOrdering},
    physical_plan::{execute_stream, memory::MemoryExec, sorts::sort::SortExec},
    physical_planner::create_physical_sort_exprs,
    prelude::{ident, SessionContext},
};
use futures::StreamExt;
use macros::ensure;
use object_store::path::Path;
use parquet::{
    arrow::{async_writer::ParquetObjectWriter, AsyncArrowWriter},
    file::properties::WriterProperties,
    format::SortingColumn,
    schema::types::ColumnPath,
};

use crate::{
    manifest::Manifest,
    read::DefaultParquetFileReaderFactory,
    sst::{allocate_id, FileId, FileMeta},
    types::{ObjectStoreRef, TimeRange, Timestamp, WriteOptions, WriteResult},
    Result,
};

pub struct WriteRequest {
    batch: RecordBatch,
}

pub struct ScanRequest {
    range: TimeRange,
    predicate: Vec<Expr>,
    /// `None` means all columns.
    projections: Option<Vec<usize>>,
}

pub struct CompactRequest {}

/// Time-aware merge storage interface.
#[async_trait]
pub trait TimeMergeStorage {
    fn schema(&self) -> &SchemaRef;

    async fn write(&self, req: WriteRequest) -> Result<()>;

    /// Implementation shoule ensure that the returned stream is sorted by time,
    /// from old to latest.
    async fn scan(&self, req: ScanRequest) -> Result<SendableRecordBatchStream>;

    async fn compact(&self, req: CompactRequest) -> Result<()>;
}

/// `TimeMergeStorage` implementation using cloud object storage.
pub struct CloudObjectStorage {
    path: String,
    store: ObjectStoreRef,
    arrow_schema: SchemaRef,
    num_primary_key: usize,
    timestamp_index: usize,
    manifest: Manifest,

    df_schema: DFSchema,
    write_props: WriterProperties,
}

/// It will organize the data in the following way:
/// ```plaintext
/// {root_path}/manifest/snapshot
/// {root_path}/manifest/timestamp1
/// {root_path}/manifest/timestamp2
/// {root_path}/manifest/...
/// {root_path}/data/timestamp_a.sst
/// {root_path}/data/timestamp_b.sst
/// {root_path}/data/...
/// ```
impl CloudObjectStorage {
    pub async fn try_new(
        root_path: String,
        store: ObjectStoreRef,
        arrow_schema: SchemaRef,
        num_primary_key: usize,
        timestamp_index: usize,
        write_options: WriteOptions,
    ) -> Result<Self> {
        let manifest_prefix = crate::manifest::PREFIX_PATH;
        let manifest =
            Manifest::try_new(format!("{root_path}/{manifest_prefix}"), store.clone()).await?;
        let df_schema = DFSchema::try_from(arrow_schema.clone()).context("build DFSchema")?;
        let write_props = Self::build_write_props(write_options, num_primary_key);
        Ok(Self {
            path: root_path,
            num_primary_key,
            timestamp_index,
            store,
            arrow_schema,
            manifest,
            df_schema,
            write_props,
        })
    }

    fn build_file_path(&self, id: FileId) -> String {
        let root = &self.path;
        let prefix = crate::sst::PREFIX_PATH;
        format!("{root}/{prefix}/{id}")
    }

    async fn write_batch(&self, req: WriteRequest) -> Result<WriteResult> {
        let file_id = allocate_id();
        let file_path = self.build_file_path(file_id);
        let file_path = Path::from(file_path);
        let object_store_writer = ParquetObjectWriter::new(self.store.clone(), file_path.clone());
        let mut writer = AsyncArrowWriter::try_new(
            object_store_writer,
            self.schema().clone(),
            Some(self.write_props.clone()),
        )
        .context("create arrow writer")?;

        // sort record batch
        let mut batches = self.sort_batch(req.batch).await?;
        while let Some(batch) = batches.next().await {
            let batch = batch.context("get sorted batch")?;
            writer.write(&batch).await.context("write arrow batch")?;
        }
        writer.close().await.context("close arrow writer")?;
        let object_meta = self
            .store
            .head(&file_path)
            .await
            .context("get object meta")?;

        Ok(WriteResult {
            id: file_id,
            size: object_meta.size,
        })
    }

    fn build_sort_exprs(&self) -> Result<LexOrdering> {
        let sort_exprs = (0..self.num_primary_key)
            .map(|i| {
                ident(self.schema().field(i).name())
                    .sort(true /* asc */, true /* nulls_first */)
            })
            .collect::<Vec<_>>();
        let sort_exprs =
            create_physical_sort_exprs(&sort_exprs, &self.df_schema, &ExecutionProps::default())
                .context("create physical sort exprs")?;

        Ok(sort_exprs)
    }

    async fn sort_batch(&self, batch: RecordBatch) -> Result<SendableRecordBatchStream> {
        let ctx = SessionContext::default();
        let schema = batch.schema();
        let sort_exprs = self.build_sort_exprs()?;
        let batch_plan =
            MemoryExec::try_new(&[vec![batch]], schema, None).context("build batch plan")?;
        let physical_plan = Arc::new(SortExec::new(sort_exprs, Arc::new(batch_plan)));

        let res =
            execute_stream(physical_plan, ctx.task_ctx()).context("execute sort physical plan")?;
        Ok(res)
    }

    fn build_write_props(write_options: WriteOptions, num_primary_key: usize) -> WriterProperties {
        let sorting_columns = write_options.enable_sorting_columns.then(|| {
            (0..num_primary_key)
                .map(|i| {
                    SortingColumn::new(i as i32, false /* desc */, true /* nulls_first */)
                })
                .collect::<Vec<_>>()
        });

        let mut builder = WriterProperties::builder()
            .set_max_row_group_size(write_options.max_row_group_size)
            .set_write_batch_size(write_options.write_bacth_size)
            .set_sorting_columns(sorting_columns)
            .set_dictionary_enabled(write_options.enable_dict)
            .set_bloom_filter_enabled(write_options.enable_bloom_filter)
            .set_encoding(write_options.encoding)
            .set_compression(write_options.compression);

        if write_options.column_options.is_none() {
            return builder.build();
        }

        for (col_name, col_opt) in write_options.column_options.unwrap() {
            let col_path = ColumnPath::new(vec![col_name.to_string()]);
            if let Some(enable_dict) = col_opt.enable_dict {
                builder = builder.set_column_dictionary_enabled(col_path.clone(), enable_dict);
            }
            if let Some(enable_bloom_filter) = col_opt.enable_bloom_filter {
                builder =
                    builder.set_column_bloom_filter_enabled(col_path.clone(), enable_bloom_filter);
            }
            if let Some(encoding) = col_opt.encoding {
                builder = builder.set_column_encoding(col_path.clone(), encoding);
            }
            if let Some(compression) = col_opt.compression {
                builder = builder.set_column_compression(col_path, compression);
            }
        }

        builder.build()
    }
}

#[async_trait]
impl TimeMergeStorage for CloudObjectStorage {
    fn schema(&self) -> &SchemaRef {
        &self.arrow_schema
    }

    async fn write(&self, req: WriteRequest) -> Result<()> {
        ensure!(req.batch.schema_ref().eq(self.schema()), "schema not match");

        let num_rows = req.batch.num_rows();
        let time_column = req
            .batch
            .column(self.timestamp_index)
            .as_any()
            .downcast_ref::<Int64Array>()
            .context("timestamp column should be int64")?;

        let mut start = Timestamp::MAX;
        let mut end = Timestamp::MIN;
        for v in time_column.values() {
            start = start.min(Timestamp(*v));
            end = end.max(Timestamp(*v));
        }
        let time_range = TimeRange::new(start, end + 1);
        let WriteResult {
            id: file_id,
            size: file_size,
        } = self.write_batch(req).await?;
        let file_meta = FileMeta {
            max_sequence: file_id, // Since file_id in increasing order, we can use it as sequence.
            num_rows: num_rows as u32,
            size: file_size as u32,
            time_range,
        };
        self.manifest.add_file(file_id, file_meta).await?;

        Ok(())
    }

    async fn scan(&self, req: ScanRequest) -> Result<SendableRecordBatchStream> {
        let ssts = self.manifest.find_ssts(&req.range).await;
        // we won't use url for selecting object_store.
        let dummy_url = ObjectStoreUrl::parse("empty://").unwrap();
        // TODO: we could group ssts based on time range.
        // TODO: fetch using multiple threads since read from parquet will incur CPU
        // when convert between arrow and parquet.
        let file_groups = ssts
            .iter()
            .map(|f| PartitionedFile::new(self.build_file_path(f.id), f.meta.size as u64))
            .collect::<Vec<_>>();
        let scan_config = FileScanConfig::new(dummy_url, self.schema().clone())
            .with_file_group(file_groups)
            .with_projection(req.projections);

        let mut builder = ParquetExec::builder(scan_config).with_parquet_file_reader_factory(
            Arc::new(DefaultParquetFileReaderFactory::new(self.store.clone())),
        );
        if let Some(expr) = conjunction(req.predicate) {
            let filters = create_physical_expr(&expr, &self.df_schema, &ExecutionProps::new())
                .context("create pyhsical expr")?;
            builder = builder.with_predicate(filters);
        }

        let parquet_exec = builder.build();
        let sort_exprs = self.build_sort_exprs()?;
        let physical_plan = Arc::new(SortExec::new(sort_exprs, Arc::new(parquet_exec)));

        let ctx = SessionContext::default();
        // TODO: dedup record batch based on primary keys and sequence number.
        let res =
            execute_stream(physical_plan, ctx.task_ctx()).context("execute sort physical plan")?;

        Ok(res)
    }

    async fn compact(&self, req: CompactRequest) -> Result<()> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use arrow::{
        array::UInt8Array,
        datatypes::{DataType, Field, Schema},
    };
    use object_store::local::LocalFileSystem;

    use super::*;

    #[tokio::test]
    async fn test_sort_batch() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::UInt8, false),
            Field::new("b", DataType::UInt8, false),
            Field::new("c", DataType::UInt8, false),
            Field::new("d", DataType::UInt8, false),
        ]));

        let store = Arc::new(LocalFileSystem::new());
        let storage = CloudObjectStorage::try_new(
            "/tmp/storage".to_string(),
            store,
            schema.clone(),
            1,
            1,
            WriteOptions::default(),
        )
        .await
        .unwrap();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt8Array::from(vec![2, 1, 3, 4, 8, 6, 5, 7])),
                Arc::new(UInt8Array::from(vec![1, 3, 4, 8, 2, 6, 5, 7])),
                Arc::new(UInt8Array::from(vec![8, 6, 2, 4, 3, 1, 5, 7])),
                Arc::new(UInt8Array::from(vec![2, 7, 4, 6, 1, 3, 5, 8])),
            ],
        )
        .unwrap();

        let mut sorted_batches = storage.sort_batch(batch).await.unwrap();
        let expected_bacth = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt8Array::from(vec![1, 2, 3, 4, 5, 6, 7, 8])),
                Arc::new(UInt8Array::from(vec![3, 1, 4, 8, 5, 6, 7, 2])),
                Arc::new(UInt8Array::from(vec![6, 8, 2, 4, 5, 1, 7, 3])),
                Arc::new(UInt8Array::from(vec![7, 2, 4, 6, 5, 3, 8, 1])),
            ],
        )
        .unwrap();

        let mut offset = 0;
        while let Some(sorted_batch) = sorted_batches.next().await {
            let sorted_batch = sorted_batch.unwrap();
            let length = sorted_batch.num_rows();
            let batch = expected_bacth.slice(offset, length);
            assert!(sorted_batch.eq(&batch));
            offset += length;
        }
    }
}
