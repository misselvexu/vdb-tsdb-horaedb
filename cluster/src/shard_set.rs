// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

use std::{collections::HashMap, sync::Arc};

use meta_client::types::{ShardId, ShardInfo, TableInfo, TablesOfShard};
use snafu::{ensure, OptionExt};

use crate::{
    shard_operator::{
        CloseContext, CloseTableContext, CreateTableContext, DropTableContext, OpenContext,
        OpenTableContext, ShardOperator,
    },
    Result, ShardVersionMismatch, TableAlreadyExists, TableNotFound, UpdateFrozenShard,
};

/// Shard set
///
/// Manage all shards opened on current node
#[derive(Debug, Default, Clone)]
pub struct ShardSet {
    inner: Arc<std::sync::RwLock<HashMap<ShardId, ShardRef>>>,
}

impl ShardSet {
    // Fetch all the shard infos.
    pub fn all_shards(&self) -> Vec<ShardRef> {
        let inner = self.inner.read().unwrap();
        inner.values().cloned().collect()
    }

    // Get the shard by its id.
    pub fn get(&self, shard_id: ShardId) -> Option<ShardRef> {
        let inner = self.inner.read().unwrap();
        inner.get(&shard_id).cloned()
    }

    /// Remove the shard.
    pub fn remove(&self, shard_id: ShardId) -> Option<ShardRef> {
        let mut inner = self.inner.write().unwrap();
        inner.remove(&shard_id)
    }

    /// Insert the tables of one shard.
    pub fn insert(&self, shard_id: ShardId, shard: ShardRef) {
        let mut inner = self.inner.write().unwrap();
        inner.insert(shard_id, shard);
    }
}

/// Shard
///
/// NOTICE: all write operations on a shard will be performed sequentially.
pub struct Shard {
    data: ShardDataRef,
    operator: tokio::sync::Mutex<ShardOperator>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard").field("data", &self.data).finish()
    }
}

impl Shard {
    pub fn new(tables_of_shard: TablesOfShard) -> Self {
        let data = Arc::new(std::sync::RwLock::new(ShardData {
            shard_info: tables_of_shard.shard_info,
            tables: tables_of_shard.tables,
            frozen: false,
        }));

        let operator = tokio::sync::Mutex::new(ShardOperator { data: data.clone() });

        Self { data, operator }
    }

    pub fn shard_info(&self) -> ShardInfo {
        let data = self.data.read().unwrap();
        data.shard_info.clone()
    }

    pub fn find_table(&self, schema_name: &str, table_name: &str) -> Option<TableInfo> {
        let data = self.data.read().unwrap();
        data.find_table(schema_name, table_name)
    }

    pub async fn open(&self, ctx: OpenContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.open(ctx).await
    }

    pub async fn close(&self, ctx: CloseContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.close(ctx).await
    }

    pub async fn create_table(&self, ctx: CreateTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.create_table(ctx).await
    }

    pub async fn drop_table(&self, ctx: DropTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.drop_table(ctx).await
    }

    pub async fn open_table(&self, ctx: OpenTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.open_table(ctx).await
    }

    pub async fn close_table(&self, ctx: CloseTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.close_table(ctx).await
    }
}

pub type ShardRef = Arc<Shard>;

#[derive(Debug, Clone)]
pub struct UpdatedTableInfo {
    pub prev_version: u64,
    pub shard_info: ShardInfo,
    pub table_info: TableInfo,
}

/// Shard data
#[derive(Debug)]
pub struct ShardData {
    /// Shard info
    pub shard_info: ShardInfo,

    /// Tables in shard
    pub tables: Vec<TableInfo>,

    /// Flag indicating that further updates are prohibited
    pub frozen: bool,
}

impl ShardData {
    pub fn find_table(&self, schema_name: &str, table_name: &str) -> Option<TableInfo> {
        self.tables
            .iter()
            .find(|table| table.schema_name == schema_name && table.name == table_name)
            .cloned()
    }

    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    pub fn try_insert_table(&mut self, updated_info: UpdatedTableInfo) -> Result<()> {
        let UpdatedTableInfo {
            prev_version: prev_shard_version,
            shard_info: curr_shard,
            table_info: new_table,
        } = updated_info;

        ensure!(
            !self.frozen,
            UpdateFrozenShard {
                shard_id: curr_shard.id,
            }
        );

        ensure!(
            self.shard_info.version == prev_shard_version,
            ShardVersionMismatch {
                shard_info: self.shard_info.clone(),
                expect_version: prev_shard_version,
            }
        );

        let table = self.tables.iter().find(|v| v.id == new_table.id);
        ensure!(
            table.is_none(),
            TableAlreadyExists {
                msg: "the table to insert has already existed",
            }
        );

        // Update tables of shard.
        self.shard_info = curr_shard;
        self.tables.push(new_table);

        Ok(())
    }

    pub fn try_remove_table(&mut self, updated_info: UpdatedTableInfo) -> Result<()> {
        let UpdatedTableInfo {
            prev_version: prev_shard_version,
            shard_info: curr_shard,
            table_info: new_table,
        } = updated_info;

        ensure!(
            !self.frozen,
            UpdateFrozenShard {
                shard_id: curr_shard.id,
            }
        );

        ensure!(
            self.shard_info.version == prev_shard_version,
            ShardVersionMismatch {
                shard_info: self.shard_info.clone(),
                expect_version: prev_shard_version,
            }
        );

        let table_idx = self
            .tables
            .iter()
            .position(|v| v.id == new_table.id)
            .with_context(|| TableNotFound {
                msg: format!("the table to remove is not found, table:{new_table:?}"),
            })?;

        // Update tables of shard.
        self.shard_info = curr_shard;
        self.tables.swap_remove(table_idx);

        Ok(())
    }
}

pub type ShardDataRef = Arc<std::sync::RwLock<ShardData>>;
