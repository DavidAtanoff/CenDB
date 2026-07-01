//! cendb: the facade crate that re-exports all CenDB sub-crates.
//!
//! Users of the engine can either depend on individual sub-crates (for
//! minimal binary footprint — see §7 of the spec) or on this facade for
//! convenience. The facade re-exports the most commonly used types from
//! each layer:
//!
//!   * [`cendb_core`] — primitives (PageId, errors, config).
//!   * [`cendb_storage`] — PAX storage (block builder, reader, segment).
//!   * [`cendb_buffer`] — buffer pool (frame, LRU-K, pinned page).
//!   * [`cendb_projection`] — multi-model projections (KV, relational,
//!     document, time-series, graph).
//!   * [`cendb_index`] — Adaptive Radix Tree (ART) primary index.
//!   * [`cendb_tx`] — MVCC + WAL + ARIES-lite recovery.
//!   * [`cendb_cenql`] — CenQL pipeline query language parser.
//!   * [`cendb_ffi`] — C-ABI for cross-language bindings.
//!
//! ## Feature gates
//!
//! Each sub-crate can be turned off via Cargo features to honour the
//! "pay only for what you touch" mandate (§0.2 of the spec):
//!
//!   * `kv` (default) — enables the KV projection.
//!   * `relational` (default) — enables the relational projection.
//!   * `document` (default) — enables the document projection.
//!   * `timeseries` (default) — enables the time-series projection.
//!   * `graph` (default) — enables the graph projection.
//!   * `ffi` (default) — enables the C-ABI.
//!   * `index` (default) — enables the ART index.
//!   * `tx` (default) — enables MVCC + WAL.
//!   * `cenql` (default) — enables the CenQL parser.
//!
//! A minimal KV-only build: `--no-default-features --features kv`.

pub mod jit_integration;
pub mod optimizer_integration;

pub use cendb_buffer;
pub use cendb_core;

#[cfg(feature = "cas")]
pub use cendb_cas;

#[cfg(feature = "cenql")]
pub use cendb_cenql;

#[cfg(feature = "executor")]
pub use cendb_executor;

#[cfg(feature = "ffi")]
pub use cendb_ffi;

#[cfg(feature = "index")]
pub use cendb_index;

#[cfg(feature = "optimizer")]
pub use cendb_optimizer;

pub use cendb_projection;
pub use cendb_storage;

#[cfg(feature = "tx")]
pub use cendb_tx;

/// Convenience re-export of the most common types.
pub mod prelude {
    pub use cendb_core::{
        BlockId, CenDbConfig, FrameId, CenError, CenResult, CenStatus, Model, NodeId, PageId,
        RowLocator, SegmentId, SlotId, Value, ValueKind,
    };
    pub use cendb_storage::header::{BlockHeader, ColumnDirectory, ColumnSpec, SegmentHeader};
    pub use cendb_storage::pax::{PaxBlock, PaxBlockBuilder, PaxBlockReader, RowId};
    pub use cendb_storage::segment::{BlockDirectory, SegmentFile, SegmentWriter};

    pub use cendb_buffer::{BufferPool, Frame, LruK, PinnedPage, PoolStats, ReadHint};

    pub use cendb_projection::{
        CsrOverlay, DocValue, GraphProjection, CenDoc, CenDocBuilder, KvProjection, KvStore,
        RelationalProjection, RelationalTable, TimeSeriesProjection, TimeSeriesSchema,
        TimeSeriesStore,
    };

    #[cfg(feature = "index")]
    pub use cendb_index::{ArtIter, ArtTree};

    #[cfg(feature = "tx")]
    pub use cendb_tx::{
        AriesRecovery, IsolationLevel, LogRecord, LogRecordType, MvccError, MvccResult,
        TimestampOracle, Transaction, TransactionManager, TransactionState, VersionHeader,
        WalConfig, WalError, WalResult, WriteAheadLog,
    };

    #[cfg(feature = "cenql")]
    pub use cendb_cenql::{
        AggExpr, BinaryOp, CenqlPipeline, CenqlStage, EdgeDirection, Expr, GraphMatchPattern,
        JoinKind, ParseError, ParseResult, Parser, SortDir, Token, TokenKind, Tokenizer,
        WindowSpec,
    };

    #[cfg(feature = "cenql")]
    pub fn parse_cenql(src: &str) -> Result<CenqlPipeline, ParseError> {
        Parser::new(src)?.parse_pipeline()
    }

    #[cfg(feature = "cas")]
    pub use cendb_cas::{BlobId, BlobStore, BlobStoreStats, CompressionKind, Hash};

    #[cfg(feature = "optimizer")]
    pub use cendb_optimizer::{
        Cost, CostModel, JoinMethod, LogicalPlan, PhysicalOperator, PhysicalPlan, PlanNode,
        StatsCatalog, TableStats, ColumnStats,
    };

    #[cfg(feature = "executor")]
    pub use cendb_executor::{
        filter_f64_gt, filter_f64_lt, filter_i64_eq, filter_i64_ge, filter_i64_gt, filter_i64_le,
        filter_i64_lt, filter_i64_ne, sum_f64, sum_i64, Morsel, MorselBatch, SelectionVector,
    };
}

/// Library version string.
pub const VERSION: &str = "1.0.0";
