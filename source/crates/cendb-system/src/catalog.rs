//! System catalog: metadata about tables, columns, and indexes.

use std::collections::HashMap;

/// Information about a table.
#[derive(Clone, Debug)]
pub struct TableInfo {
    pub name: String,
    pub model: String, // "relational", "kv", "document", "timeseries", "graph"
    pub row_count: u64,
    pub columns: Vec<ColumnInfo>,
    pub created_at: u64,
}

/// Information about a column.
#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub is_indexed: bool,
}

/// Information about an index.
#[derive(Clone, Debug)]
pub struct IndexInfo {
    pub name: String,
    pub table: String,
    pub column: String,
    pub index_type: String, // "art", "secondary", "fts", "spatial"
    pub entry_count: u64,
}

/// The system catalog: a registry of all tables, columns, and indexes.
pub struct SystemCatalog {
    tables: HashMap<String, TableInfo>,
    indexes: Vec<IndexInfo>,
}

impl SystemCatalog {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            indexes: Vec::new(),
        }
    }

    /// Register a table.
    pub fn register_table(&mut self, info: TableInfo) {
        self.tables.insert(info.name.clone(), info);
    }

    /// Register an index.
    pub fn register_index(&mut self, info: IndexInfo) {
        self.indexes.push(info);
    }

    /// Look up a table by name.
    pub fn get_table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.get(name)
    }

    /// List all tables (simulates `SELECT * FROM __tables`).
    pub fn list_tables(&self) -> Vec<&TableInfo> {
        self.tables.values().collect()
    }

    /// List all indexes (simulates `SELECT * FROM __indexes`).
    pub fn list_indexes(&self) -> &[IndexInfo] {
        &self.indexes
    }

    /// Drop a table from the catalog.
    pub fn drop_table(&mut self, name: &str) -> bool {
        self.tables.remove(name).is_some()
    }

    /// Add a column to an existing table (schema evolution).
    pub fn add_column(&mut self, table: &str, col: ColumnInfo) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        info.columns.push(col);
        Ok(())
    }

    /// Drop a column from a table (schema evolution).
    pub fn drop_column(&mut self, table: &str, col_name: &str) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        info.columns.retain(|c| c.name != col_name);
        Ok(())
    }

    /// Rename a column (schema evolution).
    pub fn rename_column(&mut self, table: &str, old_name: &str, new_name: &str) -> Result<(), String> {
        let info = self.tables.get_mut(table).ok_or("table not found")?;
        for col in &mut info.columns {
            if col.name == old_name {
                col.name = new_name.to_string();
                return Ok(());
            }
        }
        Err("column not found".to_string())
    }
}

impl Default for SystemCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_basic() {
        let mut cat = SystemCatalog::new();
        cat.register_table(TableInfo {
            name: "users".to_string(),
            model: "relational".to_string(),
            row_count: 1000,
            columns: vec![
                ColumnInfo { name: "id".into(), data_type: "i64".into(), nullable: false, is_primary_key: true, is_indexed: true },
                ColumnInfo { name: "name".into(), data_type: "bytes".into(), nullable: false, is_primary_key: false, is_indexed: false },
            ],
            created_at: 1000,
        });

        assert_eq!(cat.list_tables().len(), 1);
        assert!(cat.get_table("users").is_some());

        // Schema evolution: add column.
        cat.add_column("users", ColumnInfo {
            name: "email".into(),
            data_type: "bytes".into(),
            nullable: true,
            is_primary_key: false,
            is_indexed: false,
        }).unwrap();
        assert_eq!(cat.get_table("users").unwrap().columns.len(), 3);

        // Drop column.
        cat.drop_column("users", "email").unwrap();
        assert_eq!(cat.get_table("users").unwrap().columns.len(), 2);
    }

    #[test]
    fn index_registry() {
        let mut cat = SystemCatalog::new();
        cat.register_index(IndexInfo {
            name: "idx_users_name".into(),
            table: "users".into(),
            column: "name".into(),
            index_type: "secondary".into(),
            entry_count: 1000,
        });
        assert_eq!(cat.list_indexes().len(), 1);
    }
}
