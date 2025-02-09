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

use std::{
    collections::HashMap,
    ops::{Add, Deref, Range},
    sync::Arc,
};

use object_store::ObjectStore;
use parquet::basic::{Compression, Encoding, ZstdLevel};

use crate::sst::FileId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub i64);

impl Add for Timestamp {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Add<i64> for Timestamp {
    type Output = Self;

    fn add(self, rhs: i64) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl From<i64> for Timestamp {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl Deref for Timestamp {
    type Target = i64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Timestamp {
    pub const MAX: Timestamp = Timestamp(i64::MAX);
    pub const MIN: Timestamp = Timestamp(i64::MIN);
}

#[derive(Clone, Debug)]
pub struct TimeRange(Range<Timestamp>);

impl From<Range<Timestamp>> for TimeRange {
    fn from(value: Range<Timestamp>) -> Self {
        Self(value)
    }
}

impl Deref for TimeRange {
    type Target = Range<Timestamp>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TimeRange {
    pub fn new(start: Timestamp, end: Timestamp) -> Self {
        Self(start..end)
    }

    pub fn overlaps(&self, other: &TimeRange) -> bool {
        self.0.start < other.0.end && other.0.start < self.0.end
    }
}

pub type ObjectStoreRef = Arc<dyn ObjectStore>;

pub struct WriteResult {
    pub id: FileId,
    pub size: usize,
}

pub struct ColumnOptions {
    pub enable_dict: Option<bool>,
    pub enable_bloom_filter: Option<bool>,
    pub encoding: Option<Encoding>,
    pub compression: Option<Compression>,
}

pub struct WriteOptions {
    pub max_row_group_size: usize,
    pub write_bacth_size: usize,
    pub enable_sorting_columns: bool,
    // use to set column props with default value
    pub enable_dict: bool,
    pub enable_bloom_filter: bool,
    pub encoding: Encoding,
    pub compression: Compression,
    // use to set column props with column name
    pub column_options: Option<HashMap<String, ColumnOptions>>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            max_row_group_size: 8192,
            write_bacth_size: 1024,
            enable_sorting_columns: true,
            enable_dict: false,
            enable_bloom_filter: false,
            encoding: Encoding::PLAIN,
            compression: Compression::ZSTD(ZstdLevel::default()),
            column_options: None,
        }
    }
}
